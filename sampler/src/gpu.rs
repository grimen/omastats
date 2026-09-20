use crate::util::{
    bounded_text, kill_process_group, list_dir, opt_f64, read_bounded_line, read_f64, read_text,
    run, system_command, which, EXTERNAL_TEXT_LIMIT, STREAM_LINE_LIMIT,
};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const NVIDIA_QUERY: &str =
    "index,pci.bus_id,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,clocks.gr,clocks.max.gr,fan.speed";
const GPU_LIMIT: usize = 8;
const GPU_RESCAN: Duration = Duration::from_secs(5);
const HWMON_PREFIXES: [&str; 6] = ["temp", "power", "freq", "fan", "pwm", "energy"];
const GPU_FIELDS: [&str; 23] = [
    "id",
    "card",
    "name",
    "vendor",
    "kind",
    "driver",
    "bootVga",
    "util",
    "memUsed",
    "memTotal",
    "memBusy",
    "gttUsed",
    "gttTotal",
    "temp",
    "tempJunction",
    "tempMem",
    "power",
    "mhz",
    "maxMhz",
    "memMhz",
    "fan",
    "fanPercent",
    "fanRpm",
];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Vendor {
    Nvidia,
    Amd,
    Intel,
}

impl Vendor {
    fn from_pci(id: &str) -> Option<Self> {
        match id {
            "0x1002" => Some(Self::Amd),
            "0x8086" => Some(Self::Intel),
            "0x10de" => Some(Self::Nvidia),
            _ => None,
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Nvidia => "nvidia",
            Self::Amd => "amd",
            Self::Intel => "intel",
        }
    }

    fn fallback_name(self) -> &'static str {
        match self {
            Self::Nvidia => "NVIDIA",
            Self::Amd => "AMD",
            Self::Intel => "Intel",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum GpuKind {
    External,
    Discrete,
    Integrated,
}

impl GpuKind {
    fn id(self) -> &'static str {
        match self {
            Self::External => "external",
            Self::Discrete => "discrete",
            Self::Integrated => "integrated",
        }
    }

    fn order(self) -> u8 {
        match self {
            Self::External => 0,
            Self::Discrete => 1,
            Self::Integrated => 2,
        }
    }
}

/// `00000000:01:00.0` (nvidia-smi) and `0000:01:00.0` (sysfs) compare equal.
fn normalize_bus_id(value: &str) -> String {
    let lower = value.trim().to_lowercase();
    let parts: Vec<&str> = lower.split(':').collect();
    if parts.len() != 3 {
        return String::new();
    }
    let domain = parts[0];
    let domain = &domain[domain.len().saturating_sub(4)..];
    format!("{domain:0>4}:{}:{}", parts[1], parts[2])
}

/// Discrete, external (Thunderbolt/USB4) or integrated; used for labels and ordering.
fn classify_gpu(
    vendor: Vendor,
    slot: &str,
    removable: bool,
    has_mem_busy: bool,
    has_mclk: bool,
) -> GpuKind {
    if removable {
        return GpuKind::External;
    }
    match vendor {
        Vendor::Intel if slot == "0000:00:02.0" => GpuKind::Integrated,
        Vendor::Amd if !(has_mem_busy || has_mclk) => GpuKind::Integrated,
        _ => GpuKind::Discrete,
    }
}

/// Primary first: external, discrete, integrated; then memory, boot display, card order.
fn gpu_sort_key(
    kind: GpuKind,
    mem_total: Option<f64>,
    boot_vga: bool,
    number: u32,
) -> (u8, i64, u8, u32) {
    (
        kind.order(),
        -(mem_total.unwrap_or(0.0) as i64),
        if boot_vga { 0 } else { 1 },
        number,
    )
}

fn parse_nvidia_line(line: &str) -> Option<(String, Map<String, Value>)> {
    let parts: Vec<&str> = line.split(',').map(|p| p.trim()).collect();
    if parts.len() < 11 {
        return None;
    }
    let bus = normalize_bus_id(parts[1]);
    if bus.is_empty() {
        return None;
    }
    let num = |s: &str| s.parse::<f64>().ok();
    let mib = |s: &str| num(s).map(|v| v * 1024.0 * 1024.0);
    let mut out = Map::new();
    out.insert(
        "name".into(),
        json!(bounded_text(parts[2], EXTERNAL_TEXT_LIMIT)),
    );
    out.insert("util".into(), opt_f64(num(parts[3])));
    out.insert("memUsed".into(), opt_f64(mib(parts[4])));
    out.insert("memTotal".into(), opt_f64(mib(parts[5])));
    out.insert("temp".into(), opt_f64(num(parts[6])));
    out.insert("power".into(), opt_f64(num(parts[7])));
    out.insert("mhz".into(), opt_f64(num(parts[8])));
    out.insert("maxMhz".into(), opt_f64(num(parts[9])));
    out.insert("fan".into(), opt_f64(num(parts[10])));
    out.insert("fanPercent".into(), opt_f64(num(parts[10])));
    Some((bus, out))
}

/// `temp1_input` -> ("temp", "temp1"); anything that is not a channel file is skipped.
fn hwmon_channel_key(entry: &str) -> Option<(&'static str, &str)> {
    let key = ["_label", "_input", "_average"]
        .iter()
        .find_map(|suffix| entry.strip_suffix(suffix))
        .unwrap_or(entry);
    for prefix in HWMON_PREFIXES {
        if let Some(digits) = key.strip_prefix(prefix) {
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                return Some((prefix, key));
            }
        }
    }
    None
}

/// Highest level of an amdgpu `pp_dpm_sclk` table ("2: 2799Mhz *").
fn max_dpm_mhz(table: &str) -> Option<f64> {
    table
        .lines()
        .filter_map(|line| {
            let lower = line.to_lowercase();
            let head = lower[..lower.find("mhz")?].trim_end();
            let start = head
                .rfind(|c: char| !c.is_ascii_digit())
                .map_or(0, |i| i + 1);
            head[start..].parse::<f64>().ok()
        })
        .next_back()
}

fn card_number(card: &str) -> Option<u32> {
    let digits = card.strip_prefix("card")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn base_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn nonzero(value: Option<f64>) -> Option<f64> {
    value.filter(|v| *v != 0.0)
}

struct GpuDevice {
    id: String,
    card: String,
    number: u32,
    device: String,
    vendor: Vendor,
    driver: String,
    /// prefix -> [(label, "/…/temp1")], read once so sampling only opens values.
    channels: HashMap<&'static str, Vec<(String, String)>>,
    name: String,
    kind: GpuKind,
    boot_vga: bool,
    max_mhz: Option<f64>,
    energy: Option<(f64, Instant)>,
}

impl GpuDevice {
    fn channel(&self, prefix: &str, labels: &[&str], fallback: bool) -> Option<f64> {
        let entries = self.channels.get(prefix)?;
        let chosen = labels
            .iter()
            .find_map(|wanted| entries.iter().find(|(label, _)| label == wanted))
            .or_else(|| if fallback { entries.first() } else { None })?;
        let value = read_f64(format!("{}_input", chosen.1));
        if value.is_none() && prefix == "power" {
            return read_f64(format!("{}_average", chosen.1));
        }
        value
    }

    fn sample_sysfs(&mut self, out: &mut Map<String, Value>) {
        let path = self.device.clone();
        let card = Path::new(&path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut set = |key: &str, value: Option<f64>| {
            out.insert(key.to_string(), opt_f64(value));
        };
        set("util", read_f64(format!("{path}/gpu_busy_percent")));
        set("memBusy", read_f64(format!("{path}/mem_busy_percent")));
        set("memUsed", read_f64(format!("{path}/mem_info_vram_used")));
        set("memTotal", read_f64(format!("{path}/mem_info_vram_total")));
        set("gttUsed", read_f64(format!("{path}/mem_info_gtt_used")));
        set("gttTotal", read_f64(format!("{path}/mem_info_gtt_total")));
        let celsius = |v: Option<f64>| nonzero(v).map(|t| t / 1000.0);
        set(
            "temp",
            celsius(self.channel("temp", &["edge", "junction"], true)),
        );
        set(
            "tempJunction",
            celsius(self.channel("temp", &["junction"], false)),
        );
        set("tempMem", celsius(self.channel("temp", &["mem"], false)));

        let mut power = nonzero(self.channel("power", &["ppt", "power"], true)).map(|p| p / 1e6);
        if power.is_none() {
            if let Some(energy) = self.channel("energy", &["pkg", "package"], true) {
                let now = Instant::now();
                if let Some((previous, stamp)) = self.energy {
                    let seconds = now.duration_since(stamp).as_secs_f64();
                    if seconds > 0.0 && energy >= previous {
                        power = Some((energy - previous) / 1e6 / seconds);
                    }
                }
                self.energy = Some((energy, now));
            }
        }
        set("power", power);

        let mhz = match self.channel("freq", &["sclk"], true) {
            Some(sclk) => Some(sclk / 1e6),
            None => nonzero(read_f64(format!("{card}/gt_cur_freq_mhz")))
                .or_else(|| nonzero(read_f64(format!("{card}/gt/gt0/rps_cur_freq_mhz"))))
                .or_else(|| nonzero(read_f64(format!("{path}/tile0/gt0/freq0/cur_freq")))),
        };
        set("mhz", mhz);
        set(
            "memMhz",
            nonzero(self.channel("freq", &["mclk"], false)).map(|f| f / 1e6),
        );
        set(
            "maxMhz",
            nonzero(self.max_mhz)
                .or_else(|| nonzero(read_f64(format!("{card}/gt_max_freq_mhz"))))
                .or_else(|| nonzero(read_f64(format!("{path}/tile0/gt0/freq0/max_freq")))),
        );
        set("fanRpm", self.channel("fan", &[], true));
        let pwm_path = self
            .channels
            .get("pwm")
            .and_then(|entries| entries.first())
            .map(|(_, path)| path.clone());
        if let Some(pwm_path) = pwm_path {
            if let Some(pwm) = read_f64(&pwm_path) {
                let max = nonzero(read_f64(format!("{pwm_path}_max"))).unwrap_or(255.0);
                let percent = ((pwm / max * 100.0).clamp(0.0, 100.0) * 10.0).round() / 10.0;
                set("fanPercent", Some(percent));
                set("fan", Some(percent));
            }
        }
    }
}

type NvidiaLatest = Arc<Mutex<HashMap<String, Map<String, Value>>>>;

/// Every GPU: NVIDIA via a long-running `nvidia-smi -l` reader, AMD/Intel via sysfs.
pub struct GpuSampler {
    drm: String,
    devices: Vec<GpuDevice>,
    signature: Vec<(String, String)>,
    last_scan: Instant,
    latest: NvidiaLatest,
    child: Option<Child>,
}

impl GpuSampler {
    pub fn new() -> Self {
        let mut sampler = Self {
            drm: "/sys/class/drm".to_string(),
            devices: Vec::new(),
            signature: Vec::new(),
            last_scan: Instant::now(),
            latest: Arc::new(Mutex::new(HashMap::new())),
            child: None,
        };
        sampler.detect();
        sampler
    }

    /// (card, PCI slot) for every DRM card; cheap enough to repeat for hotplug.
    fn cards(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for card in list_dir(&self.drm) {
            if card_number(&card).is_none() {
                continue;
            }
            if let Ok(real) = std::fs::canonicalize(format!("{}/{card}/device", self.drm)) {
                out.push((card, base_name(&real)));
            }
        }
        out
    }

    fn detect(&mut self) {
        let cards = self.cards();
        self.last_scan = Instant::now();
        let mut devices: Vec<GpuDevice> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for (card, slot) in &cards {
            let device = format!("{}/{card}/device", self.drm);
            let vendor = read_text(format!("{device}/vendor"))
                .and_then(|v| Vendor::from_pci(v.to_lowercase().as_str()));
            let vendor = match vendor {
                Some(v) if !seen.contains(slot) && devices.len() < GPU_LIMIT => v,
                _ => continue,
            };
            seen.insert(slot.clone());
            let hwmon = list_dir(format!("{device}/hwmon"))
                .into_iter()
                .next()
                .map(|hw| format!("{device}/hwmon/{hw}"));
            let channels = hwmon
                .as_deref()
                .map(Self::hwmon_channels)
                .unwrap_or_default();
            let has_mclk = channels
                .get("freq")
                .is_some_and(|entries| entries.iter().any(|(label, _)| label == "mclk"));
            let has_mem_busy = Path::new(&format!("{device}/mem_busy_percent")).exists();
            let driver = std::fs::canonicalize(format!("{device}/driver"))
                .map(|p| base_name(&p))
                .unwrap_or_default();
            let max_mhz = if vendor == Vendor::Amd {
                read_text(format!("{device}/pp_dpm_sclk")).and_then(|t| max_dpm_mhz(&t))
            } else {
                None
            };
            devices.push(GpuDevice {
                id: slot.clone(),
                card: card.clone(),
                number: card_number(card).unwrap_or(0),
                vendor,
                driver: bounded_text(&driver, EXTERNAL_TEXT_LIMIT),
                channels,
                name: bounded_text(&Self::pci_name(&device), EXTERNAL_TEXT_LIMIT),
                kind: classify_gpu(
                    vendor,
                    slot,
                    Self::removable(&device),
                    has_mem_busy,
                    has_mclk,
                ),
                boot_vga: read_text(format!("{device}/boot_vga")).as_deref() == Some("1"),
                max_mhz,
                energy: None,
                device,
            });
        }
        self.signature = cards;
        self.devices = devices;
        let wants_nvidia = self.devices.iter().any(|d| d.vendor == Vendor::Nvidia);
        if wants_nvidia && self.child.is_none() && which("nvidia-smi") {
            self.start_nvidia();
        } else if !wants_nvidia {
            self.stop();
        }
    }

    /// True when the card or a bridge above it is marked removable (eGPU).
    fn removable(device: &str) -> bool {
        let mut path = match std::fs::canonicalize(device) {
            Ok(p) => p,
            Err(_) => return false,
        };
        for _ in 0..16 {
            if !path.starts_with("/sys/devices/") || path.components().count() < 5 {
                break;
            }
            if read_text(path.join("removable")).as_deref() == Some("removable") {
                return true;
            }
            if !path.pop() {
                break;
            }
        }
        false
    }

    fn hwmon_channels(hwmon: &str) -> HashMap<&'static str, Vec<(String, String)>> {
        let mut channels: HashMap<&'static str, Vec<(String, String)>> = HashMap::new();
        let mut seen: HashSet<String> = HashSet::new();
        for entry in list_dir(hwmon) {
            let (prefix, key) = match hwmon_channel_key(&entry) {
                Some(found) => found,
                None => continue,
            };
            if !seen.insert(key.to_string()) {
                continue;
            }
            let label = read_text(format!("{hwmon}/{key}_label"))
                .unwrap_or_default()
                .to_lowercase();
            channels
                .entry(prefix)
                .or_default()
                .push((label, format!("{hwmon}/{key}")));
        }
        channels
    }

    fn pci_name(device: &str) -> String {
        let slot = match std::fs::canonicalize(device) {
            Ok(p) => base_name(&p),
            Err(_) => return String::new(),
        };
        if !which("lspci") {
            return String::new();
        }
        let out = run("lspci", &["-mm", "-s", &slot], Duration::from_secs(2));
        for line in out.lines() {
            // lspci -mm quotes: slot "class" "vendor" "device" ...
            let quoted: Vec<&str> = line
                .split('"')
                .enumerate()
                .filter(|(i, _)| i % 2 == 1)
                .map(|(_, s)| s)
                .collect();
            if quoted.len() >= 3 {
                let name = quoted[2];
                if let (Some(open), Some(close)) = (name.rfind('['), name.rfind(']')) {
                    if close > open {
                        return bounded_text(&name[open + 1..close], EXTERNAL_TEXT_LIMIT);
                    }
                }
                return bounded_text(name, EXTERNAL_TEXT_LIMIT);
            }
        }
        String::new()
    }

    fn start_nvidia(&mut self) {
        let mut command = match system_command("nvidia-smi") {
            Some(command) => command,
            None => return,
        };
        let child = command
            .arg(format!("--query-gpu={NVIDIA_QUERY}"))
            .arg("--format=csv,noheader,nounits")
            .arg("-l")
            .arg("1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(_) => return,
        };
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                kill_process_group(child.id());
                let _ = child.wait();
                return;
            }
        };
        let latest = Arc::clone(&self.latest);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(line)) = read_bounded_line(&mut reader, STREAM_LINE_LIMIT) {
                if let Some((bus, snapshot)) = parse_nvidia_line(&line) {
                    if let Ok(mut map) = latest.lock() {
                        if map.len() < GPU_LIMIT || map.contains_key(&bus) {
                            map.insert(bus, snapshot);
                        }
                    }
                }
            }
            if let Ok(mut map) = latest.lock() {
                map.clear();
            }
        });
        self.child = Some(child);
    }

    /// Every GPU, primary first.
    pub fn sample_all(&mut self) -> Vec<Value> {
        if self.last_scan.elapsed() >= GPU_RESCAN {
            self.last_scan = Instant::now();
            if self.cards() != self.signature {
                self.detect();
            }
        }
        let latest = self
            .latest
            .lock()
            .map(|map| map.clone())
            .unwrap_or_default();
        let mut gpus = Vec::new();
        for device in &mut self.devices {
            let mut out = Map::new();
            for field in GPU_FIELDS {
                out.insert(field.to_string(), Value::Null);
            }
            out.insert("id".into(), json!(device.id));
            out.insert("card".into(), json!(device.card));
            out.insert("vendor".into(), json!(device.vendor.id()));
            out.insert("kind".into(), json!(device.kind.id()));
            out.insert("driver".into(), json!(device.driver));
            out.insert("bootVga".into(), json!(device.boot_vga));
            let name = if device.name.is_empty() {
                device.vendor.fallback_name()
            } else {
                device.name.as_str()
            };
            out.insert("name".into(), json!(name));
            if device.vendor == Vendor::Nvidia {
                if let Some(snapshot) = latest.get(&device.id) {
                    for (key, value) in snapshot {
                        out.insert(key.clone(), value.clone());
                    }
                }
            } else {
                device.sample_sysfs(&mut out);
            }
            let mem_total = out.get("memTotal").and_then(Value::as_f64);
            let key = gpu_sort_key(device.kind, mem_total, device.boot_vga, device.number);
            gpus.push((key, Value::Object(out)));
        }
        gpus.sort_by_key(|(key, _)| *key);
        gpus.into_iter().map(|(_, gpu)| gpu).collect()
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            kill_process_group(child.id());
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_ids_normalize_to_the_sysfs_form() {
        assert_eq!(normalize_bus_id("00000000:01:00.0"), "0000:01:00.0");
        assert_eq!(normalize_bus_id(" 0000:C8:00.0 "), "0000:c8:00.0");
        assert_eq!(normalize_bus_id("1:02:00.0"), "0001:02:00.0");
        assert_eq!(normalize_bus_id("garbage"), "");
    }

    #[test]
    fn nvidia_lines_parse_per_gpu() {
        let lines = "0, 00000000:01:00.0, NVIDIA GeForce RTX 4090, 12, 1024, 24564, 45, 33.5, 210, 3105, 30\n\
                     1, 00000000:41:00.0, NVIDIA RTX A2000, [N/A], 10, 6138, 38, [N/A], 300, 2100, [N/A]";
        let parsed: Vec<_> = lines.lines().filter_map(parse_nvidia_line).collect();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, "0000:01:00.0");
        assert_eq!(parsed[0].1["name"], "NVIDIA GeForce RTX 4090");
        assert_eq!(parsed[0].1["util"], 12.0);
        assert_eq!(parsed[0].1["memTotal"], 24564.0 * 1024.0 * 1024.0);
        assert_eq!(parsed[0].1["fanPercent"], 30.0);
        assert_eq!(parsed[1].0, "0000:41:00.0");
        assert!(parsed[1].1["util"].is_null());
        assert!(parsed[1].1["power"].is_null());
        assert!(parse_nvidia_line("0, 00000000:01:00.0, short").is_none());
    }

    #[test]
    fn gpus_classify_by_vendor_and_topology() {
        use GpuKind::*;
        assert_eq!(
            classify_gpu(Vendor::Amd, "0000:03:00.0", false, true, true),
            Discrete
        );
        assert_eq!(
            classify_gpu(Vendor::Amd, "0000:c8:00.0", false, false, false),
            Integrated
        );
        assert_eq!(
            classify_gpu(Vendor::Amd, "0000:c8:00.0", true, false, false),
            External
        );
        assert_eq!(
            classify_gpu(Vendor::Intel, "0000:00:02.0", false, false, false),
            Integrated
        );
        assert_eq!(
            classify_gpu(Vendor::Intel, "0000:03:00.0", false, false, false),
            Discrete
        );
        assert_eq!(
            classify_gpu(Vendor::Nvidia, "0000:01:00.0", false, false, false),
            Discrete
        );
    }

    #[test]
    fn primary_prefers_external_then_discrete_then_memory() {
        let gib = 1024.0 * 1024.0 * 1024.0;
        let mut keys = vec![
            (
                "igpu",
                gpu_sort_key(GpuKind::Integrated, Some(2.0 * gib), true, 0),
            ),
            (
                "small",
                gpu_sort_key(GpuKind::Discrete, Some(8.0 * gib), false, 1),
            ),
            (
                "big",
                gpu_sort_key(GpuKind::Discrete, Some(20.0 * gib), false, 2),
            ),
            ("egpu", gpu_sort_key(GpuKind::External, None, false, 3)),
        ];
        keys.sort_by_key(|(_, key)| *key);
        let order: Vec<&str> = keys.iter().map(|(name, _)| *name).collect();
        assert_eq!(order, ["egpu", "big", "small", "igpu"]);
    }

    #[test]
    fn hwmon_entries_map_to_channels() {
        assert_eq!(hwmon_channel_key("temp1_input"), Some(("temp", "temp1")));
        assert_eq!(
            hwmon_channel_key("power1_average"),
            Some(("power", "power1"))
        );
        assert_eq!(hwmon_channel_key("pwm1"), Some(("pwm", "pwm1")));
        assert_eq!(hwmon_channel_key("freq2_label"), Some(("freq", "freq2")));
        assert_eq!(hwmon_channel_key("pwm1_enable"), None);
        assert_eq!(hwmon_channel_key("temp1_crit"), None);
        assert_eq!(hwmon_channel_key("name"), None);
    }

    #[test]
    fn dpm_table_yields_the_top_level() {
        assert_eq!(
            max_dpm_mhz("0: 500Mhz *\n1: 1100Mhz\n2: 2799Mhz"),
            Some(2799.0)
        );
        assert_eq!(max_dpm_mhz(""), None);
    }
}
