use crate::util::{bounded_text, rate, read_text, round1, run, EXTERNAL_TEXT_LIMIT};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};

const PROC_LIMIT: usize = 6;
const FULL_LIMIT: usize = 400;
const CONNECTION_LIMIT: usize = 12;
const GPU_PROC_LIMIT: usize = 8;
/// A group is only offered for ending when every one of its pids fits.
const END_PID_LIMIT: usize = 32;
const PROCESS_SCAN_LIMIT: usize = 16_384;
const FD_SCAN_LIMIT: usize = 4096;
const PROC_FILE_LIMIT: u64 = 64 * 1024;
const SOCKET_SCAN_LIMIT: usize = 16_384;

#[derive(Clone, Copy)]
struct Prev {
    ticks: u64,
    read_bytes: u64,
    write_bytes: u64,
}

struct Group {
    name: String,
    pid: u32,
    cpu: f64,
    mem: u64,
    read: f64,
    write: f64,
    count: u32,
    pids: Vec<u32>,
    /// Every process of the group belongs to the user running the sampler.
    own: bool,
}

/// The pids the UI may signal: all of the group's, and only when they are all
/// the user's own and few enough to list in full.
fn endable_pids(own: bool, pids: &[u32], count: u32) -> &[u32] {
    if own && count as usize == pids.len() {
        pids
    } else {
        &[]
    }
}

/// One DRM client as the kernel describes it in `/proc/<pid>/fdinfo/<fd>`.
#[derive(Debug, Default, PartialEq)]
struct DrmClient {
    pdev: String,
    client: String,
    /// Nanoseconds each engine (gfx, compute, dec, enc…) has spent on this client.
    engines: HashMap<String, u64>,
    vram: u64,
}

/// The standard `drm-*` keys, shared by amdgpu, i915 and xe.
fn parse_drm_fdinfo(text: &str) -> Option<DrmClient> {
    let mut out = DrmClient::default();
    let (mut resident, mut total): (Option<u64>, Option<u64>) = (None, None);
    let size = |value: &str| -> Option<u64> {
        let mut fields = value.split_whitespace();
        let number: u64 = fields.next()?.parse().ok()?;
        Some(number.saturating_mul(match fields.next() {
            Some("KiB") => 1 << 10,
            Some("MiB") => 1 << 20,
            Some("GiB") => 1 << 30,
            _ => 1,
        }))
    };
    for line in text.lines() {
        let (key, value) = match line.split_once(':') {
            Some((key, value)) => (key.trim(), value.trim()),
            None => continue,
        };
        match key {
            "drm-pdev" => out.pdev = bounded_text(value, EXTERNAL_TEXT_LIMIT),
            "drm-client-id" => out.client = bounded_text(value, EXTERNAL_TEXT_LIMIT),
            "drm-resident-vram" => resident = size(value),
            "drm-memory-vram" | "drm-total-vram" => total = total.or(size(value)),
            _ => {
                let engine = match key.strip_prefix("drm-engine-") {
                    Some(engine) if !engine.starts_with("capacity-") => engine,
                    _ => continue,
                };
                if let Some(ns) = value.split_whitespace().next().and_then(|n| n.parse().ok()) {
                    out.engines.insert(engine.to_string(), ns);
                }
            }
        }
    }
    out.vram = resident.or(total).unwrap_or(0);
    (!out.pdev.is_empty() && !out.client.is_empty()).then_some(out)
}

pub struct ProcessSampler {
    prev: HashMap<u32, Prev>,
    threads: f64,
    clk_tck: f64,
    page_size: u64,
    buffer: Vec<u8>,
    names: HashMap<u32, String>,
    sockets_prev: HashMap<String, (u64, u64)>,
    sockets_time: Option<Instant>,
    gpu_prev: HashMap<(String, String), HashMap<String, u64>>,
    gpu_time: Option<Instant>,
}

/// A readable name for a process: the kernel's 15-character comm, or the
/// executable's basename when comm looks truncated.
pub fn display_name(pid: u32, comm: &str) -> String {
    if comm.len() < 15 {
        return comm.to_string();
    }
    if let Some(cmdline) = read_text(format!("/proc/{pid}/cmdline")) {
        let first = cmdline.split('\0').next().unwrap_or("");
        let base = first.rsplit('/').next().unwrap_or("");
        if !base.is_empty()
            && (base.starts_with(&comm[..8.min(comm.len())]) || base.len() > comm.len())
        {
            return bounded_text(base, EXTERNAL_TEXT_LIMIT);
        }
    }
    bounded_text(comm, EXTERNAL_TEXT_LIMIT)
}

impl ProcessSampler {
    pub fn new() -> Self {
        let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        Self {
            prev: HashMap::new(),
            threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1) as f64,
            clk_tck: if clk_tck > 0.0 { clk_tck } else { 100.0 },
            page_size: if page_size > 0 { page_size } else { 4096 },
            buffer: Vec::with_capacity(4096),
            names: HashMap::new(),
            sockets_prev: HashMap::new(),
            sockets_time: None,
            gpu_prev: HashMap::new(),
            gpu_time: None,
        }
    }

    fn process_name(&self, pid: u32) -> String {
        if let Some(name) = self.names.get(&pid) {
            return name.clone();
        }
        let stat = read_text(format!("/proc/{pid}/stat")).unwrap_or_default();
        match (stat.find('('), stat.rfind(')')) {
            (Some(open), Some(close)) if close > open => display_name(pid, &stat[open + 1..close]),
            _ => format!("pid {pid}"),
        }
    }

    /// GPU time and video memory per process, from the DRM clients behind each
    /// process's open `/dev/dri` files. Only readable processes are counted,
    /// and drivers without DRM fdinfo (NVIDIA's) report nothing.
    pub fn gpu_usage(&mut self) -> Value {
        let mut clients: HashMap<(String, String), (u32, DrmClient)> = HashMap::new();
        if let Ok(entries) = fs::read_dir("/proc") {
            let mut scanned = 0usize;
            for entry in entries.filter_map(Result::ok) {
                let pid: u32 = match entry.file_name().to_string_lossy().parse() {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                scanned += 1;
                if scanned > PROCESS_SCAN_LIMIT {
                    break;
                }
                let fds = match fs::read_dir(format!("/proc/{pid}/fd")) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                for fd in fds.filter_map(Result::ok).take(FD_SCAN_LIMIT) {
                    let is_drm = fs::read_link(fd.path()).is_ok_and(|t| t.starts_with("/dev/dri/"));
                    if !is_drm {
                        continue;
                    }
                    let number = fd.file_name().to_string_lossy().to_string();
                    let client = read_text(format!("/proc/{pid}/fdinfo/{number}"))
                        .and_then(|text| parse_drm_fdinfo(&text));
                    if let Some(client) = client {
                        // A client shared through dup() or fork() counts once.
                        clients
                            .entry((client.pdev.clone(), client.client.clone()))
                            .or_insert((pid, client));
                    }
                }
            }
        }

        let now = Instant::now();
        let elapsed = self
            .gpu_time
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or(0.0);
        let usable = elapsed > 0.0 && elapsed < 5.0;
        struct Usage {
            pid: u32,
            gpu: f64,
            vram: u64,
            /// The GPU holding most of the process's video memory.
            id: String,
            most: u64,
        }
        let mut usage: HashMap<String, Usage> = HashMap::new();
        let mut current = HashMap::new();
        for (key, (pid, client)) in clients {
            let before = self.gpu_prev.get(&key);
            // The busiest engine, as `gpu_busy_percent` reports for the whole card.
            let busiest = client
                .engines
                .iter()
                .filter_map(|(engine, ns)| Some(ns.saturating_sub(*before?.get(engine)?)))
                .max()
                .unwrap_or(0);
            let percent = if usable {
                (busiest as f64 / (elapsed * 1e9) * 100.0).clamp(0.0, 100.0)
            } else {
                0.0
            };
            let entry = usage.entry(self.process_name(pid)).or_insert(Usage {
                pid,
                gpu: 0.0,
                vram: 0,
                id: client.pdev.clone(),
                most: 0,
            });
            entry.gpu = (entry.gpu + percent).min(100.0);
            entry.vram += client.vram;
            if client.vram > entry.most {
                entry.most = client.vram;
                entry.id = client.pdev.clone();
            }
            current.insert(key, client.engines);
        }
        self.gpu_prev = current;
        self.gpu_time = Some(now);

        let mut rows: Vec<(String, Usage)> = usage
            .into_iter()
            .filter(|(_, u)| u.gpu > 0.0 || u.vram > 0)
            .collect();
        rows.sort_by(|a, b| b.1.gpu.total_cmp(&a.1.gpu).then(b.1.vram.cmp(&a.1.vram)));
        rows.truncate(GPU_PROC_LIMIT);
        json!(rows
            .into_iter()
            .map(|(name, u)| json!({
                "name": name, "pid": u.pid, "gpu": round1(u.gpu), "vram": u.vram, "id": u.id,
            }))
            .collect::<Vec<_>>())
    }

    pub fn reset(&mut self) {
        self.prev.clear();
        self.names.clear();
    }

    fn read_small(&mut self, path: &str) -> Option<&[u8]> {
        self.buffer.clear();
        let file = fs::File::open(path).ok()?;
        file.take(PROC_FILE_LIMIT + 1)
            .read_to_end(&mut self.buffer)
            .ok()?;
        if self.buffer.len() as u64 > PROC_FILE_LIMIT {
            return None;
        }
        Some(&self.buffer)
    }

    pub fn sample(&mut self, elapsed: f64, full: bool) -> Value {
        let mut current: HashMap<u32, Prev> = HashMap::new();
        let mut groups: HashMap<String, Group> = HashMap::new();
        let mut names: HashMap<u32, String> = HashMap::new();
        let entries = match fs::read_dir("/proc") {
            Ok(e) => e,
            Err(_) => return json!({ "cpu": [], "mem": [], "io": [] }),
        };
        let uid = unsafe { libc::getuid() };
        let mut scanned = 0usize;
        for entry in entries.filter_map(Result::ok) {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            let pid: u32 = match name.parse() {
                Ok(p) => p,
                Err(_) => continue,
            };
            scanned += 1;
            if scanned > PROCESS_SCAN_LIMIT {
                break;
            }
            let stat = match self.read_small(&format!("/proc/{pid}/stat")) {
                Some(s) => String::from_utf8_lossy(s).to_string(),
                None => continue,
            };
            let close = match stat.rfind(')') {
                Some(c) => c,
                None => continue,
            };
            let open = match stat.find('(') {
                Some(o) => o,
                None => continue,
            };
            let comm = stat[open + 1..close].to_string();
            let fields: Vec<&str> = stat[close + 2..].split_whitespace().collect();
            if fields.len() < 22 || fields[0] == "Z" {
                continue;
            }
            let utime: u64 = fields[11].parse().unwrap_or(0);
            let stime: u64 = fields[12].parse().unwrap_or(0);
            let rss_pages: u64 = fields[21].parse().unwrap_or(0);
            let ticks = utime + stime;
            let rss = rss_pages * self.page_size;

            let mut read_bytes = 0u64;
            let mut write_bytes = 0u64;
            if let Some(io) = self.read_small(&format!("/proc/{pid}/io")) {
                let text = String::from_utf8_lossy(io);
                for line in text.lines() {
                    if let Some(v) = line.strip_prefix("read_bytes:") {
                        read_bytes = v.trim().parse().unwrap_or(0);
                    } else if let Some(v) = line.strip_prefix("write_bytes:") {
                        write_bytes = v.trim().parse().unwrap_or(0);
                        break;
                    }
                }
            }
            current.insert(
                pid,
                Prev {
                    ticks,
                    read_bytes,
                    write_bytes,
                },
            );

            let prev = self.prev.get(&pid).copied();
            let (cpu, io_read, io_write) = match prev {
                Some(p) if elapsed > 0.0 => (
                    ticks.saturating_sub(p.ticks) as f64 / self.clk_tck / elapsed * 100.0
                        / self.threads,
                    rate(Some(read_bytes as f64), Some(p.read_bytes as f64), elapsed),
                    rate(
                        Some(write_bytes as f64),
                        Some(p.write_bytes as f64),
                        elapsed,
                    ),
                ),
                _ => (0.0, 0.0, 0.0),
            };

            // Names are stable per pid; resolve cmdline once.
            let display = match self.names.get(&pid) {
                Some(n) => n.clone(),
                None => display_name(pid, &comm),
            };
            names.insert(pid, display.clone());
            let group = groups.entry(display.clone()).or_insert_with(|| Group {
                name: display,
                pid,
                cpu: 0.0,
                mem: 0,
                read: 0.0,
                write: 0.0,
                count: 0,
                pids: Vec::new(),
                own: true,
            });
            group.own &= entry.metadata().is_ok_and(|m| m.uid() == uid);
            if group.pids.len() < END_PID_LIMIT {
                group.pids.push(pid);
            }
            group.cpu += cpu.max(0.0);
            group.mem += rss;
            group.read += io_read;
            group.write += io_write;
            group.count += 1;
        }
        self.prev = current;
        self.names = names;

        let mut list: Vec<&Group> = groups.values().collect();
        let to_json = |g: &Group| {
            json!({
                "name": g.name, "pid": g.pid, "count": g.count,
                "cpu": round1(g.cpu), "mem": g.mem, "read": g.read, "write": g.write,
                "pids": endable_pids(g.own, &g.pids, g.count),
            })
        };

        list.sort_by(|a, b| {
            b.cpu
                .partial_cmp(&a.cpu)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let all: Option<Vec<Value>> = if full {
            Some(list.iter().take(FULL_LIMIT).map(|g| to_json(g)).collect())
        } else {
            None
        };
        let cpu: Vec<Value> = list
            .iter()
            .take(PROC_LIMIT)
            .take_while(|g| g.cpu > 0.0)
            .map(|g| to_json(g))
            .collect();
        list.sort_by_key(|group| std::cmp::Reverse(group.mem));
        let mem: Vec<Value> = list
            .iter()
            .take(PROC_LIMIT)
            .take_while(|g| g.mem > 0)
            .map(|g| to_json(g))
            .collect();
        list.sort_by(|a, b| {
            (b.read + b.write)
                .partial_cmp(&(a.read + a.write))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let io: Vec<Value> = list
            .iter()
            .take(PROC_LIMIT)
            .take_while(|g| g.read + g.write > 0.0)
            .map(|g| to_json(g))
            .collect();

        let mut out = json!({ "cpu": cpu, "mem": mem, "io": io, "total": groups.len() });
        if let Some(all) = all {
            out["all"] = Value::Array(all);
        }
        out
    }

    /// Network traffic per process. The kernel keeps cumulative bytes_sent /
    /// bytes_received on every TCP socket and hands them to any user through
    /// socket diagnostics (`ss -ti`); differencing them per socket between
    /// calls gives real per-process rates without root. Sockets are tied to
    /// processes through /proc/<pid>/fd (own processes) or, failing that,
    /// named after their cgroup (system services). UDP, and so QUIC, carries
    /// no such counters and is not attributed.
    pub fn network_usage(&mut self, full: bool) -> Value {
        struct Sock {
            sent: u64,
            recv: u64,
            cgroup: String,
            established: bool,
        }
        let out = run("ss", &["-tineH"], Duration::from_secs(2));
        let mut sockets: HashMap<String, Sock> = HashMap::new();
        let mut current: Option<(String, Sock)> = None;
        for line in out.lines() {
            if line.starts_with(' ') || line.starts_with('\t') {
                if let Some((_, sock)) = current.as_mut() {
                    for tok in line.split_whitespace() {
                        if let Some(v) = tok.strip_prefix("bytes_sent:") {
                            sock.sent = v.parse().unwrap_or(0);
                        } else if let Some(v) = tok.strip_prefix("bytes_received:") {
                            sock.recv = v.parse().unwrap_or(0);
                        }
                    }
                }
                continue;
            }
            if let Some((ino, sock)) = current.take() {
                if sockets.len() >= SOCKET_SCAN_LIMIT {
                    self.sockets_prev.clear();
                    self.sockets_time = None;
                    return json!([]);
                }
                sockets.insert(ino, sock);
            }
            let mut ino = String::new();
            let mut cgroup = String::new();
            for tok in line.split_whitespace() {
                if let Some(v) = tok.strip_prefix("ino:") {
                    ino = v.to_string();
                } else if let Some(v) = tok.strip_prefix("cgroup:") {
                    cgroup = v.to_string();
                }
            }
            if !ino.is_empty() {
                current = Some((
                    ino,
                    Sock {
                        sent: 0,
                        recv: 0,
                        cgroup,
                        established: line.starts_with("ESTAB"),
                    },
                ));
            }
        }
        if let Some((ino, sock)) = current.take() {
            if sockets.len() >= SOCKET_SCAN_LIMIT {
                self.sockets_prev.clear();
                self.sockets_time = None;
                return json!([]);
            }
            sockets.insert(ino, sock);
        }
        if sockets.is_empty() {
            self.sockets_prev.clear();
            self.sockets_time = None;
            return json!([]);
        }

        // Socket inode -> owning process, for the processes we may inspect.
        let mut owner: HashMap<String, (u32, String)> = HashMap::new();
        if let Ok(entries) = fs::read_dir("/proc") {
            let mut scanned = 0usize;
            for entry in entries.filter_map(Result::ok) {
                let pid: u32 = match entry.file_name().to_string_lossy().parse() {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                scanned += 1;
                if scanned > PROCESS_SCAN_LIMIT {
                    break;
                }
                let fds = match fs::read_dir(format!("/proc/{pid}/fd")) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                let mut name: Option<String> = None;
                for fd in fds.filter_map(Result::ok).take(FD_SCAN_LIMIT) {
                    let target = match fs::read_link(fd.path()) {
                        Ok(t) => t.to_string_lossy().to_string(),
                        Err(_) => continue,
                    };
                    let inode = match target
                        .strip_prefix("socket:[")
                        .and_then(|s| s.strip_suffix(']'))
                    {
                        Some(i) if sockets.contains_key(i) => i.to_string(),
                        _ => continue,
                    };
                    if name.is_none() {
                        name = Some(match self.names.get(&pid) {
                            Some(n) => n.clone(),
                            None => {
                                let stat =
                                    read_text(format!("/proc/{pid}/stat")).unwrap_or_default();
                                match (stat.find('('), stat.rfind(')')) {
                                    (Some(o), Some(c)) if c > o => {
                                        display_name(pid, &stat[o + 1..c])
                                    }
                                    _ => format!("pid {pid}"),
                                }
                            }
                        });
                    }
                    owner.insert(inode, (pid, name.clone().unwrap_or_default()));
                }
            }
        }

        let now = Instant::now();
        let elapsed = self
            .sockets_time
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or(0.0);
        // A long gap means inode numbers may have been recycled; start over.
        let usable = elapsed > 0.0 && elapsed < 5.0;

        struct Usage {
            name: String,
            pid: u32,
            pids: HashSet<u32>,
            connections: u32,
            rx: f64,
            tx: f64,
        }
        let mut groups: HashMap<String, Usage> = HashMap::new();
        for (ino, sock) in &sockets {
            let (pid, name) = match owner.get(ino) {
                Some((p, n)) => (*p, n.clone()),
                None => (0, cgroup_label(&sock.cgroup)),
            };
            let group = groups.entry(name.clone()).or_insert(Usage {
                name,
                pid,
                pids: HashSet::new(),
                connections: 0,
                rx: 0.0,
                tx: 0.0,
            });
            group.pids.insert(pid);
            if sock.established {
                group.connections += 1;
            }
            if usable {
                if let Some((prev_sent, prev_recv)) = self.sockets_prev.get(ino) {
                    group.tx += sock.sent.saturating_sub(*prev_sent) as f64 / elapsed;
                    group.rx += sock.recv.saturating_sub(*prev_recv) as f64 / elapsed;
                }
            }
        }
        self.sockets_prev = sockets
            .iter()
            .map(|(k, s)| (k.clone(), (s.sent, s.recv)))
            .collect();
        self.sockets_time = Some(now);

        let mut list: Vec<&Usage> = groups
            .values()
            .filter(|g| g.connections > 0 || g.rx + g.tx > 0.0)
            .collect();
        list.sort_by(|a, b| {
            (b.rx + b.tx)
                .partial_cmp(&(a.rx + a.tx))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.connections.cmp(&a.connections))
                .then(a.name.cmp(&b.name))
        });
        let out: Vec<Value> = list
            .iter()
            .take(if full { FULL_LIMIT } else { CONNECTION_LIMIT })
            .map(|g| {
                json!({
                    "name": g.name, "pid": g.pid, "count": g.pids.len(),
                    "connections": g.connections, "rx": g.rx, "tx": g.tx,
                })
            })
            .collect();
        json!(out)
    }
}

/// Human label for a socket whose owner we cannot inspect, from its cgroup:
/// app-<launcher>-<name>-<id>.scope -> name, <name>.service -> name.
fn cgroup_label(cgroup: &str) -> String {
    let last = cgroup
        .rsplit('/')
        .next()
        .unwrap_or("")
        .replace("\\x2d", "-");
    if last.is_empty() {
        return "other".to_string();
    }
    let stem = last
        .trim_end_matches(".scope")
        .trim_end_matches(".service")
        .trim_end_matches(".mount")
        .trim_end_matches(".slice");
    if let Some(rest) = stem.strip_prefix("app-") {
        let parts: Vec<&str> = rest.split('-').collect();
        if parts.len() >= 3 {
            return bounded_text(&parts[1..parts.len() - 1].join("-"), EXTERNAL_TEXT_LIMIT);
        }
        if parts.len() == 2 {
            return bounded_text(parts[1], EXTERNAL_TEXT_LIMIT);
        }
    }
    if stem.is_empty() {
        "other".to_string()
    } else {
        bounded_text(stem, EXTERNAL_TEXT_LIMIT)
    }
}

#[cfg(test)]
mod tests {
    use super::{endable_pids, parse_drm_fdinfo};

    #[test]
    fn drm_fdinfo_yields_engines_and_video_memory() {
        let text = "pos:\t0\nflags:\t02100002\ndrm-driver:\tamdgpu\ndrm-client-id:\t8\n\
            drm-pdev:\t0000:03:00.0\ndrm-total-vram:\t12 KiB\ndrm-resident-vram:\t3 MiB\n\
            drm-engine-gfx:\t1500 ns\ndrm-engine-dec:\t20 ns\ndrm-engine-capacity-gfx:\t1\n";
        let client = parse_drm_fdinfo(text).expect("a DRM client");
        assert_eq!(client.pdev, "0000:03:00.0");
        assert_eq!(client.client, "8");
        assert_eq!(client.vram, 3 << 20);
        assert_eq!(client.engines.get("gfx"), Some(&1500));
        assert_eq!(client.engines.get("dec"), Some(&20));
        assert_eq!(client.engines.len(), 2);
        assert_eq!(parse_drm_fdinfo("pos:\t0\nflags:\t02\n"), None);
    }

    #[test]
    fn only_complete_groups_of_own_processes_can_be_ended() {
        assert_eq!(endable_pids(true, &[10, 11], 2), &[10, 11]);
        assert!(endable_pids(false, &[10, 11], 2).is_empty());
        // More processes than were listed: ending some of them would mislead.
        assert!(endable_pids(true, &[10, 11], 40).is_empty());
    }
}
