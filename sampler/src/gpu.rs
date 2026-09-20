use crate::util::{
    bounded_text, kill_process_group, list_dir, opt_f64, read_bounded_line, read_f64, read_text,
    run, system_command, which, EXTERNAL_TEXT_LIMIT, STREAM_LINE_LIMIT,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::BufReader;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const NVIDIA_QUERY: &str =
    "name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,clocks.gr,clocks.max.gr,fan.speed,pci.bus_id,utilization.memory,utilization.encoder,utilization.decoder,clocks.mem,power.limit";
const GPU_LIMIT: usize = 8;
const GPU_RESCAN: Duration = Duration::from_secs(5);
const AMD_NAMES: &str = "/usr/share/libdrm/amdgpu.ids";

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Nvidia,
    Amd,
    Intel,
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

impl Kind {
    fn vendor(self) -> &'static str {
        match self {
            Kind::Nvidia => "nvidia",
            Kind::Amd => "amd",
            Kind::Intel => "intel",
        }
    }

    fn fallback_name(self) -> &'static str {
        match self {
            Kind::Nvidia => "NVIDIA",
            Kind::Amd => "AMD",
            Kind::Intel => "Intel",
        }
    }
}

/// What a device can be asked this tick: hot-unplugged cards are dropped and
/// runtime-suspended ones are reported without touching their sensors.
#[derive(Debug, PartialEq)]
enum State {
    Active,
    Asleep,
    Gone,
}

fn power_state(runtime_status: Option<&str>) -> State {
    match runtime_status {
        Some("suspended") => State::Asleep,
        _ => State::Active,
    }
}

/// libdrm's `device id, revision, marketing name` table tells apart cards that
/// share one PCI id, which pci.ids can only name as a family.
fn amd_marketing_name(table: &str, device: &str, revision: &str) -> Option<String> {
    let hex = |s: &str| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
    let (device, revision) = (hex(device)?, hex(revision)?);
    table.lines().find_map(|line| {
        let mut fields = line.split(',');
        let matches = hex(fields.next()?)? == device && hex(fields.next()?)? == revision;
        let name = fields.next()?.trim();
        (matches && !name.is_empty()).then(|| name.to_string())
    })
}

/// PCIe generation from sysfs' `16.0 GT/s PCIe`.
fn pcie_gen(speed: &str) -> Option<u32> {
    let rate: f64 = speed.split_whitespace().next()?.parse().ok()?;
    [(2.5, 1), (5.0, 2), (8.0, 3), (16.0, 4), (32.0, 5), (64.0, 6)]
        .iter()
        .find(|(gts, _)| (rate - gts).abs() < 0.01)
        .map(|(_, gen)| *gen)
}

/// amdgpu hides `mem_busy_percent` on APUs, Intel's integrated GPU sits at
/// 00:02.0, and everything else is a card of its own.
fn is_integrated(kind: Kind, slot: &str, has_mem_busy: bool) -> bool {
    match kind {
        Kind::Amd => !has_mem_busy,
        Kind::Intel => slot == "0000:00:02.0",
        Kind::Nvidia => false,
    }
}

struct Device {
    kind: Kind,
    integrated: bool,
    /// Behind a port the kernel marks removable: Thunderbolt or USB4.
    external: bool,
    card: String,
    hwmon: Option<String>,
    name: String,
    id: String,
    mem_total: Option<f64>,
}

impl Device {
    /// Reads PCI and PM core attributes only, which never wake the device.
    fn state(&self) -> State {
        if !std::path::Path::new(&format!("{}/vendor", self.card)).exists() {
            return State::Gone;
        }
        power_state(read_text(format!("{}/power/runtime_status", self.card)).as_deref())
    }

    fn label(&self) -> &str {
        if self.name.is_empty() {
            self.kind.fallback_name()
        } else {
            self.name.as_str()
        }
    }
}

pub struct GpuSampler {
    devices: Vec<Device>,
    /// PCI slots of every DRM card at the last scan; a change means hotplug.
    signature: Vec<String>,
    last_scan: Option<Instant>,
    latest: Arc<Mutex<HashMap<String, Value>>>,
    child: Option<Child>,
}

impl GpuSampler {
    pub fn new() -> Self {
        let mut sampler = Self {
            devices: Vec::new(),
            signature: Vec::new(),
            last_scan: None,
            latest: Arc::new(Mutex::new(HashMap::new())),
            child: None,
        };
        sampler.refresh();
        sampler
    }

    /// Every `cardN` device directory with its PCI slot.
    fn cards() -> Vec<(String, String)> {
        let mut out = Vec::new();
        for card in list_dir("/sys/class/drm") {
            let is_card = card.starts_with("card")
                && card[4..].bytes().all(|b| b.is_ascii_digit())
                && card.len() > 4;
            if is_card {
                let device = format!("/sys/class/drm/{card}/device");
                let slot = Self::pci_slot(&device);
                out.push((device, slot));
            }
        }
        out
    }

    /// Detects again when the set of cards changed: an eGPU came or went.
    fn refresh(&mut self) {
        if self.last_scan.is_some_and(|at| at.elapsed() < GPU_RESCAN) {
            return;
        }
        self.last_scan = Some(Instant::now());
        let cards = Self::cards();
        let mut signature: Vec<String> = cards.iter().map(|(_, slot)| slot.clone()).collect();
        signature.sort();
        if signature != self.signature || self.devices.iter().any(|d| d.state() == State::Gone) {
            self.signature = signature;
            self.detect(cards);
        }
    }

    fn detect(&mut self, cards: Vec<(String, String)>) {
        let previous = std::mem::take(&mut self.devices);
        let had_nvidia = previous.iter().any(|d| d.kind == Kind::Nvidia);
        for (device, id) in cards {
            if self.devices.len() >= GPU_LIMIT {
                break;
            }
            let vendor = read_text(format!("{device}/vendor"))
                .unwrap_or_default()
                .to_lowercase();
            let kind = match vendor.as_str() {
                "0x1002" => Kind::Amd,
                "0x8086" => Kind::Intel,
                "0x10de" => Kind::Nvidia,
                _ => continue,
            };
            let hwmon = list_dir(format!("{device}/hwmon"))
                .into_iter()
                .next()
                .map(|hw| format!("{device}/hwmon/{hw}"));
            let marketing = (kind == Kind::Amd)
                .then(|| {
                    amd_marketing_name(
                        &read_text(AMD_NAMES)?,
                        &read_text(format!("{device}/device"))?,
                        &read_text(format!("{device}/revision"))?,
                    )
                })
                .flatten();
            let name = marketing.unwrap_or_else(|| Self::pci_name(&id));
            let name = bounded_text(&name, EXTERNAL_TEXT_LIMIT);
            let mem_total = previous.iter().find(|d| d.id == id).and_then(|d| d.mem_total);
            let has_mem_busy =
                std::path::Path::new(&format!("{device}/mem_busy_percent")).exists();
            self.devices.push(Device {
                kind,
                integrated: is_integrated(kind, &id, has_mem_busy),
                external: Self::removable(&device),
                card: device,
                hwmon,
                name,
                id,
                mem_total,
            });
        }
        // nvidia-smi enumerates its GPUs when it starts, so it restarts with them.
        let nvidia = self.devices.iter().any(|d| d.kind == Kind::Nvidia);
        if had_nvidia || nvidia {
            self.stop();
            if let Ok(mut latest) = self.latest.lock() {
                latest.clear();
            }
        }
        if nvidia && !(which("nvidia-smi") && self.start_nvidia()) {
            self.devices.retain(|d| d.kind != Kind::Nvidia);
        }
    }

    /// True when the card or a bridge above it is marked removable (eGPU).
    fn removable(device: &str) -> bool {
        let mut path = match std::fs::canonicalize(device) {
            Ok(p) => p,
            Err(_) => return false,
        };
        while path.starts_with("/sys/devices/") && path.components().count() > 4 {
            if read_text(path.join("removable")).as_deref() == Some("removable") {
                return true;
            }
            if !path.pop() {
                break;
            }
        }
        false
    }

    fn pci_slot(device: &str) -> String {
        std::fs::canonicalize(device)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_default()
    }

    fn pci_name(slot: &str) -> String {
        if slot.is_empty() || !which("lspci") {
            return String::new();
        }
        let out = run("lspci", &["-mm", "-s", slot], Duration::from_secs(2));
        for line in out.lines() {
            let fields: Vec<&str> = line.split('"').filter(|s| !s.trim().is_empty()).collect();
            // lspci -mm quotes: slot "class" "vendor" "device" ...
            let quoted: Vec<&str> = line
                .split('"')
                .enumerate()
                .filter(|(i, _)| i % 2 == 1)
                .map(|(_, s)| s)
                .collect();
            let _ = fields;
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

    fn start_nvidia(&mut self) -> bool {
        let mut command = match system_command("nvidia-smi") {
            Some(command) => command,
            None => return false,
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
            Err(_) => return false,
        };
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                kill_process_group(child.id());
                let _ = child.wait();
                return false;
            }
        };
        let latest = Arc::clone(&self.latest);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(line)) = read_bounded_line(&mut reader, STREAM_LINE_LIMIT) {
                if line.is_empty() {
                    continue;
                }
                let parts: Vec<&str> = line.split(',').map(|p| p.trim()).collect();
                if parts.len() < 10 {
                    continue;
                }
                let num = |s: &str| s.parse::<f64>().ok();
                // Older drivers may not know the newer fields: they stay null.
                let extra = |index: usize| parts.get(index).and_then(|s| num(s));
                let mem_used = num(parts[2]).map(|v| v * 1024.0 * 1024.0);
                let mem_total = num(parts[3]).map(|v| v * 1024.0 * 1024.0);
                let id = normalize_bus_id(parts[9]);
                let snapshot = json!({
                    "id": id,
                    "name": bounded_text(parts[0], EXTERNAL_TEXT_LIMIT),
                    "vendor": "nvidia",
                    "util": opt_f64(num(parts[1])),
                    "memUsed": opt_f64(mem_used),
                    "memTotal": opt_f64(mem_total),
                    "temp": opt_f64(num(parts[4])),
                    "power": opt_f64(num(parts[5])),
                    "mhz": opt_f64(num(parts[6])),
                    "maxMhz": opt_f64(num(parts[7])),
                    "fan": opt_f64(num(parts[8])),
                    "memBusy": opt_f64(extra(10)),
                    "vcnBusy": opt_f64(match (extra(11), extra(12)) {
                        (Some(enc), Some(dec)) => Some(enc.max(dec)),
                        (enc, dec) => enc.or(dec),
                    }),
                    "memMhz": opt_f64(extra(13)),
                    "powerCap": opt_f64(extra(14)),
                });
                if let Ok(mut slot) = latest.lock() {
                    slot.insert(id, snapshot);
                }
            }
        });
        self.child = Some(child);
        true
    }

    /// The channel with one of `labels`, else the first one unless `exact`.
    fn hwmon_value(device: &Device, prefix: &str, labels: &[&str], exact: bool) -> Option<f64> {
        let hwmon = device.hwmon.as_ref()?;
        let entries = list_dir(hwmon);
        // Discrete AMD cards report power as `power1_average` only.
        Self::hwmon_channel(hwmon, &entries, prefix, "_input", labels, exact)
            .or_else(|| Self::hwmon_channel(hwmon, &entries, prefix, "_average", labels, exact))
    }

    /// The narrowest and slowest link between the card and the CPU, which is
    /// what an eGPU dock or a x4 slot limits, with the widest the card offers.
    fn pcie_link(card: &str) -> (Option<f64>, Option<f64>, Option<f64>) {
        let (mut gen, mut width, mut max_width): (Option<u32>, Option<f64>, Option<f64>) =
            (None, None, None);
        let mut path = match std::fs::canonicalize(card) {
            Ok(p) => p,
            Err(_) => return (None, None, None),
        };
        while path.starts_with("/sys/devices/") && path.components().count() > 4 {
            if let Some(w) = read_f64(path.join("current_link_width")).filter(|w| *w > 0.0) {
                width = Some(width.map_or(w, |now| now.min(w)));
            }
            if let Some(w) = read_f64(path.join("max_link_width")).filter(|w| *w > 0.0) {
                max_width = Some(max_width.map_or(w, |now| now.max(w)));
            }
            if let Some(g) = read_text(path.join("current_link_speed")).and_then(|s| pcie_gen(&s)) {
                gen = Some(gen.map_or(g, |now| now.min(g)));
            }
            if !path.pop() {
                break;
            }
        }
        (gen.map(f64::from), width, max_width)
    }

    fn hwmon_channel(
        hwmon: &str,
        entries: &[String],
        prefix: &str,
        suffix: &str,
        labels: &[&str],
        exact: bool,
    ) -> Option<f64> {
        let mut chosen: Option<String> = None;
        for entry in entries {
            if entry.starts_with(prefix) && entry.ends_with(suffix) {
                let key = &entry[..entry.len() - suffix.len()];
                let label = read_text(format!("{hwmon}/{key}_label"))
                    .unwrap_or_default()
                    .to_lowercase();
                let preferred = labels.contains(&label.as_str());
                if preferred || (chosen.is_none() && !exact) {
                    chosen = Some(entry.clone());
                    if preferred {
                        break;
                    }
                }
            }
        }
        read_f64(format!("{hwmon}/{}", chosen?))
    }

    fn sample_device(&self, device: &Device) -> Value {
        if device.kind == Kind::Nvidia {
            if let Ok(slot) = self.latest.lock() {
                if let Some(v) = slot.get(&device.id) {
                    return v.clone();
                }
            }
            return json!({
                "id": device.id,
                "name": device.label(),
                "vendor": device.kind.vendor(),
                "util": Value::Null,
            });
        }
        let card = &device.card;
        let parent = std::path::Path::new(card)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let util = read_f64(format!("{card}/gpu_busy_percent"));
        let mem_used = read_f64(format!("{card}/mem_info_vram_used"));
        let mem_total = read_f64(format!("{card}/mem_info_vram_total"));
        let hwmon = |prefix: &str, label: &str| Self::hwmon_value(device, prefix, &[label], true);
        let temp =
            Self::hwmon_value(device, "temp", &["edge", "junction"], false).map(|t| t / 1000.0);
        let power = Self::hwmon_value(device, "power", &["ppt", "power"], false)
            .map(|p| p / 1_000_000.0);
        let power_cap = device
            .hwmon
            .as_ref()
            .and_then(|h| read_f64(format!("{h}/power1_cap")))
            .map(|p| p / 1_000_000.0);
        let fan_rpm = device.hwmon.as_ref().and_then(|h| read_f64(format!("{h}/fan1_input")));
        let fan_max = device.hwmon.as_ref().and_then(|h| read_f64(format!("{h}/fan1_max")));
        let mhz = Self::hwmon_value(device, "freq", &["sclk"], false)
            .map(|f| f / 1_000_000.0)
            .or_else(|| read_f64(format!("{parent}/gt_cur_freq_mhz")))
            .or_else(|| read_f64(format!("{parent}/gt/gt0/rps_cur_freq_mhz")));
        let max_mhz = read_f64(format!("{parent}/gt_max_freq_mhz"));
        json!({
            "id": device.id,
            "name": device.label(),
            "vendor": device.kind.vendor(),
            "util": opt_f64(util),
            "memUsed": opt_f64(mem_used),
            "memTotal": opt_f64(mem_total),
            "temp": opt_f64(temp),
            "power": opt_f64(power),
            "mhz": opt_f64(mhz),
            "maxMhz": opt_f64(max_mhz),
            "fan": Value::Null,
            "tempJunction": opt_f64(hwmon("temp", "junction").map(|t| t / 1000.0)),
            "tempMem": opt_f64(hwmon("temp", "mem").map(|t| t / 1000.0)),
            "memBusy": opt_f64(read_f64(format!("{card}/mem_busy_percent"))),
            "vcnBusy": opt_f64(read_f64(format!("{card}/vcn_busy_percent"))),
            "memMhz": opt_f64(hwmon("freq", "mclk").map(|f| f / 1_000_000.0)),
            "fanRpm": opt_f64(fan_rpm),
            "fanMax": opt_f64(fan_max),
            "powerCap": opt_f64(power_cap),
        })
    }

    /// Every GPU, the one with the most memory first: that one is the default readout.
    pub fn sample(&mut self) -> Vec<Value> {
        self.refresh();
        let mut gpus: Vec<Value> = Vec::new();
        for index in 0..self.devices.len() {
            let state = self.devices[index].state();
            if state == State::Gone {
                self.last_scan = None;
                continue;
            }
            let device = &self.devices[index];
            let kind = if device.integrated { "integrated" } else { "discrete" };
            let external = device.external;
            match state {
                State::Gone => {}
                State::Asleep => {
                    let device = &self.devices[index];
                    gpus.push(json!({
                        "id": device.id,
                        "name": device.label(),
                        "vendor": device.kind.vendor(),
                        "util": Value::Null,
                        "memTotal": opt_f64(device.mem_total),
                        "asleep": true,
                    }));
                }
                State::Active => {
                    let gpu = self.sample_device(&self.devices[index]);
                    if let Some(total) = gpu["memTotal"].as_f64() {
                        self.devices[index].mem_total = Some(total);
                    }
                    let mut gpu = gpu;
                    if !self.devices[index].integrated {
                        let (gen, width, max_width) = Self::pcie_link(&self.devices[index].card);
                        gpu["pcieGen"] = opt_f64(gen);
                        gpu["pcieWidth"] = opt_f64(width);
                        gpu["pcieMaxWidth"] = opt_f64(max_width);
                    }
                    gpus.push(gpu);
                }
            }
            if let Some(gpu) = gpus.last_mut() {
                gpu["kind"] = json!(kind);
                gpu["external"] = json!(external);
            }
        }
        let mem = |v: &Value| v["memTotal"].as_f64().unwrap_or(0.0);
        gpus.sort_by(|a, b| mem(b).total_cmp(&mem(a)));
        gpus
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
    use super::{
        amd_marketing_name, is_integrated, normalize_bus_id, pcie_gen, power_state, Kind, State,
    };

    #[test]
    fn bus_ids_normalize_to_the_sysfs_form() {
        assert_eq!(normalize_bus_id("00000000:01:00.0"), "0000:01:00.0");
        assert_eq!(normalize_bus_id(" 0000:C8:00.0 "), "0000:c8:00.0");
        assert_eq!(normalize_bus_id("garbage"), "");
    }

    #[test]
    fn amd_cards_sharing_a_pci_id_are_named_by_revision() {
        let table = "# comment\n1.0.0\n744C,\tC8,\tAMD Radeon RX 7900 XTX\n744C,\tCC,\tAMD Radeon RX 7900 XT\n";
        let name = |device, revision| amd_marketing_name(table, device, revision);
        assert_eq!(name("0x744c", "0xcc").as_deref(), Some("AMD Radeon RX 7900 XT"));
        assert_eq!(name("0x744c", "0xc8").as_deref(), Some("AMD Radeon RX 7900 XTX"));
        assert_eq!(name("0x744c", "0x01"), None);
        assert_eq!(name("", "0xcc"), None);
    }

    #[test]
    fn integrated_gpus_are_told_from_cards() {
        assert!(is_integrated(Kind::Amd, "0000:c8:00.0", false));
        assert!(!is_integrated(Kind::Amd, "0000:03:00.0", true));
        assert!(is_integrated(Kind::Intel, "0000:00:02.0", false));
        assert!(!is_integrated(Kind::Intel, "0000:03:00.0", false));
        assert!(!is_integrated(Kind::Nvidia, "0000:01:00.0", false));
    }

    #[test]
    fn link_speeds_map_to_pcie_generations() {
        assert_eq!(pcie_gen("16.0 GT/s PCIe"), Some(4));
        assert_eq!(pcie_gen("2.5 GT/s PCIe"), Some(1));
        assert_eq!(pcie_gen("8.0 GT/s PCIe"), Some(3));
        assert_eq!(pcie_gen("Unknown"), None);
    }

    #[test]
    fn only_suspended_devices_are_asleep() {
        assert_eq!(power_state(Some("suspended")), State::Asleep);
        assert_eq!(power_state(Some("active")), State::Active);
        assert_eq!(power_state(Some("unsupported")), State::Active);
        assert_eq!(power_state(None), State::Active);
    }
}
