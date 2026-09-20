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
    "name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,clocks.gr,clocks.max.gr,fan.speed,pci.bus_id";
const GPU_LIMIT: usize = 8;
const GPU_RESCAN: Duration = Duration::from_secs(5);

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

struct Device {
    kind: Kind,
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
            let name = bounded_text(&Self::pci_name(&id), EXTERNAL_TEXT_LIMIT);
            let mem_total = previous.iter().find(|d| d.id == id).and_then(|d| d.mem_total);
            self.devices.push(Device {
                kind,
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
                });
                if let Ok(mut slot) = latest.lock() {
                    slot.insert(id, snapshot);
                }
            }
        });
        self.child = Some(child);
        true
    }

    fn hwmon_value(device: &Device, prefix: &str, labels: &[&str]) -> Option<f64> {
        let hwmon = device.hwmon.as_ref()?;
        let entries = list_dir(hwmon);
        // Discrete AMD cards report power as `power1_average` only.
        Self::hwmon_channel(hwmon, &entries, prefix, "_input", labels)
            .or_else(|| Self::hwmon_channel(hwmon, &entries, prefix, "_average", labels))
    }

    fn hwmon_channel(
        hwmon: &str,
        entries: &[String],
        prefix: &str,
        suffix: &str,
        labels: &[&str],
    ) -> Option<f64> {
        let mut chosen: Option<String> = None;
        for entry in entries {
            if entry.starts_with(prefix) && entry.ends_with(suffix) {
                let key = &entry[..entry.len() - suffix.len()];
                let label = read_text(format!("{hwmon}/{key}_label"))
                    .unwrap_or_default()
                    .to_lowercase();
                let preferred = labels.contains(&label.as_str());
                if preferred || chosen.is_none() {
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
        let temp = Self::hwmon_value(device, "temp", &["edge", "junction"]).map(|t| t / 1000.0);
        let power =
            Self::hwmon_value(device, "power", &["ppt", "power"]).map(|p| p / 1_000_000.0);
        let mhz = Self::hwmon_value(device, "freq", &["sclk"])
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
        })
    }

    /// Every GPU, the one with the most memory first: that one is the default readout.
    pub fn sample(&mut self) -> Vec<Value> {
        self.refresh();
        let mut gpus: Vec<Value> = Vec::new();
        for index in 0..self.devices.len() {
            match self.devices[index].state() {
                State::Gone => self.last_scan = None,
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
                    gpus.push(gpu);
                }
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
    use super::{normalize_bus_id, power_state, State};

    #[test]
    fn bus_ids_normalize_to_the_sysfs_form() {
        assert_eq!(normalize_bus_id("00000000:01:00.0"), "0000:01:00.0");
        assert_eq!(normalize_bus_id(" 0000:C8:00.0 "), "0000:c8:00.0");
        assert_eq!(normalize_bus_id("garbage"), "");
    }

    #[test]
    fn only_suspended_devices_are_asleep() {
        assert_eq!(power_state(Some("suspended")), State::Asleep);
        assert_eq!(power_state(Some("active")), State::Active);
        assert_eq!(power_state(Some("unsupported")), State::Active);
        assert_eq!(power_state(None), State::Active);
    }
}
