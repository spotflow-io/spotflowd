//! OS metric collector — reads /proc and /sys on each tick.
//!
//! Some counter metrics (network errors/drops) are sent as **absolute cumulative
//! totals**, matching the Zephyr SDK convention.  The Spotflow platform computes
//! deltas and rates server-side.
//!
//! Disk I/O (`disk_read_bytes`, `disk_write_bytes`, `disk_read_ops`,
//! `disk_write_ops`) and network throughput (`network_rx_bytes`,
//! `network_tx_bytes`) compute deltas locally so the values flow through
//! normal aggregation.
//!
//! `disk_io_util_percent` is a derived gauge computed locally from consecutive
//! io_ms readings.
//!
//! Gauge metrics (CPU%, memory, temperature) emit on every tick.

use super::{MetricSample, MetricValue};
use crate::config::MetricsOsConfig;
use std::collections::HashMap;
use std::time::Instant;

// ---------------------------------------------------------------------------
// Internal counter snapshots (kept between ticks for delta computation)
// ---------------------------------------------------------------------------

struct CpuTicks {
    total: u64,
    idle: u64,
}

#[derive(Clone, Default)]
struct DiskCounters {
    read_sectors: u64,
    write_sectors: u64,
    read_ops: u64,
    write_ops: u64,
    /// Milliseconds the device was busy doing I/O (diskstats field 10, 0-based).
    /// None on kernels that expose fewer than 13 diskstats fields.
    io_ms: Option<u64>,
}

struct NetCounters {
    rx_bytes: u64,
    tx_bytes: u64,
    rx_errors: u64,
    tx_errors: u64,
    rx_drops: u64,
    tx_drops: u64,
}

// ---------------------------------------------------------------------------
// Collector
// ---------------------------------------------------------------------------

pub struct Collector {
    prev_cpu: Option<CpuTicks>,
    prev_disk: HashMap<String, DiskCounters>,
    prev_disk_time: Option<Instant>,
    prev_net: HashMap<String, NetCounters>,
}

impl Collector {
    pub fn new() -> Self {
        Self {
            prev_cpu: None,
            prev_disk: HashMap::new(),
            prev_disk_time: None,
            prev_net: HashMap::new(),
        }
    }

    /// Read all enabled metric groups.  Returns `(samples, uptime_ms)`.
    pub fn collect(&mut self, cfg: &MetricsOsConfig) -> (Vec<MetricSample>, u64) {
        let uptime_ms = read_uptime_ms();
        let mut samples = Vec::new();

        if cfg.groups.cpu {
            self.collect_cpu(&mut samples);
            collect_load_avg(&mut samples);
            collect_cpu_temp(&mut samples);
        }
        if cfg.groups.memory {
            collect_memory(&mut samples);
        }
        if cfg.groups.disk {
            self.collect_disk_io(&mut samples);
            collect_disk_space(&mut samples, &cfg.disk.mount_points);
        }
        if cfg.groups.network {
            let filter = if cfg.network.interfaces.is_empty() {
                None
            } else {
                Some(&cfg.network.interfaces)
            };
            self.collect_network(&mut samples, filter);
        }
        if cfg.groups.system {
            collect_system(&mut samples);
        }

        (samples, uptime_ms)
    }

    // --- CPU ---

    fn collect_cpu(&mut self, out: &mut Vec<MetricSample>) {
        let ticks = match read_cpu_ticks() {
            Some(t) => t,
            None => return,
        };
        if let Some(prev) = &self.prev_cpu {
            let total_d = ticks.total.saturating_sub(prev.total);
            let idle_d = ticks.idle.saturating_sub(prev.idle);
            if total_d > 0 {
                let pct = (total_d - idle_d) as f64 / total_d as f64 * 100.0;
                out.push(sample(
                    "cpu_utilization_percent",
                    MetricValue::Float(pct),
                    &[],
                ));
            }
        }
        self.prev_cpu = Some(ticks);
    }

    // --- Disk I/O ---

    fn collect_disk_io(&mut self, out: &mut Vec<MetricSample>) {
        let now = Instant::now();
        let elapsed_ms = self
            .prev_disk_time
            .map(|t| now.duration_since(t).as_millis() as u64)
            .unwrap_or(0);

        let curr = read_diskstats();
        for (dev, c) in &curr {
            let lbl = &[("device", dev.clone())];

            // Disk I/O: emit deltas between consecutive readings so the
            // values flow through normal aggregation.  Skip the first tick.
            if let Some(p) = self.prev_disk.get(dev) {
                let read_bytes_delta = (c.read_sectors.saturating_sub(p.read_sectors)) * 512;
                let write_bytes_delta = (c.write_sectors.saturating_sub(p.write_sectors)) * 512;
                let read_ops_delta = c.read_ops.saturating_sub(p.read_ops);
                let write_ops_delta = c.write_ops.saturating_sub(p.write_ops);

                out.push(sample(
                    "disk_read_bytes",
                    MetricValue::Int(read_bytes_delta as i64),
                    lbl,
                ));
                out.push(sample(
                    "disk_write_bytes",
                    MetricValue::Int(write_bytes_delta as i64),
                    lbl,
                ));
                out.push(sample(
                    "disk_read_ops",
                    MetricValue::Int(read_ops_delta as i64),
                    lbl,
                ));
                out.push(sample(
                    "disk_write_ops",
                    MetricValue::Int(write_ops_delta as i64),
                    lbl,
                ));

                // io_util% — derived gauge from consecutive io_ms readings.
                if elapsed_ms > 0 {
                    if let (Some(curr_io), Some(prev_io)) = (c.io_ms, p.io_ms) {
                        let io_ms_delta = curr_io.saturating_sub(prev_io);
                        let util = (io_ms_delta as f64 / elapsed_ms as f64 * 100.0).min(100.0);
                        out.push(sample(
                            "disk_io_util_percent",
                            MetricValue::Float(util),
                            lbl,
                        ));
                    }
                }
            }
        }
        self.prev_disk = curr;
        self.prev_disk_time = Some(now);
    }

    // --- Network ---

    fn collect_network(&mut self, out: &mut Vec<MetricSample>, filter: Option<&Vec<String>>) {
        let curr = read_net_dev();
        for (iface, c) in &curr {
            if let Some(f) = filter {
                if !f.contains(iface) {
                    continue;
                }
            }
            let lbl = &[("interface", iface.clone())];

            // rx/tx bytes: emit delta between consecutive readings so the
            // value flows through normal aggregation instead of growing
            // monotonically.  Skip the first tick (no previous reading).
            if let Some(prev) = self.prev_net.get(iface) {
                let rx_delta = c.rx_bytes.saturating_sub(prev.rx_bytes);
                let tx_delta = c.tx_bytes.saturating_sub(prev.tx_bytes);
                out.push(sample(
                    "network_rx_bytes",
                    MetricValue::Int(rx_delta as i64),
                    lbl,
                ));
                out.push(sample(
                    "network_tx_bytes",
                    MetricValue::Int(tx_delta as i64),
                    lbl,
                ));
            }

            // errors / drops — keep as cumulative counters for now.
            out.push(counter_sample(
                "net_rx_errors",
                MetricValue::Int(c.rx_errors as i64),
                lbl,
            ));
            out.push(counter_sample(
                "net_tx_errors",
                MetricValue::Int(c.tx_errors as i64),
                lbl,
            ));
            out.push(counter_sample(
                "net_rx_drops",
                MetricValue::Int(c.rx_drops as i64),
                lbl,
            ));
            out.push(counter_sample(
                "net_tx_drops",
                MetricValue::Int(c.tx_drops as i64),
                lbl,
            ));
        }
        self.prev_net = curr;
    }
}

// ---------------------------------------------------------------------------
// Stateless collectors (no inter-tick state needed)
// ---------------------------------------------------------------------------

fn collect_load_avg(out: &mut Vec<MetricSample>) {
    if let Some((l1, l5, l15)) = read_load_avg() {
        out.push(sample("cpu_load_avg_1m", MetricValue::Float(l1), &[]));
        out.push(sample("cpu_load_avg_5m", MetricValue::Float(l5), &[]));
        out.push(sample("cpu_load_avg_15m", MetricValue::Float(l15), &[]));
    }
}

fn collect_cpu_temp(out: &mut Vec<MetricSample>) {
    for (zone_type, celsius) in read_thermal_zones() {
        out.push(sample(
            "cpu_temperature",
            MetricValue::Float(celsius),
            &[("zone", zone_type)],
        ));
    }
}

fn collect_memory(out: &mut Vec<MetricSample>) {
    // I2: mem_total_bytes is constant (RAM doesn't change) — omit it.
    let Some((total, available, swap_total, swap_free)) = read_meminfo() else {
        return;
    };
    out.push(sample(
        "mem_available_bytes",
        MetricValue::Int(available as i64),
        &[],
    ));
    if total > 0 {
        let pct = (total - available) as f64 / total as f64 * 100.0;
        out.push(sample("mem_used_percent", MetricValue::Float(pct), &[]));
    }
    if swap_total > 0 {
        let pct = (swap_total - swap_free) as f64 / swap_total as f64 * 100.0;
        out.push(sample("swap_used_percent", MetricValue::Float(pct), &[]));
    }
}

fn collect_disk_space(out: &mut Vec<MetricSample>, mounts: &[String]) {
    for mount in mounts {
        if let Some((free, total, inodes_free, inodes_total)) = disk_statvfs(mount) {
            out.push(sample(
                "disk_free_bytes",
                MetricValue::Int(free as i64),
                &[("mount", mount.clone())],
            ));
            if total > 0 {
                let pct = (total - free) as f64 / total as f64 * 100.0;
                out.push(sample(
                    "disk_used_percent",
                    MetricValue::Float(pct),
                    &[("mount", mount.clone())],
                ));
            }
            if inodes_total > 0 {
                let pct = (inodes_total - inodes_free) as f64 / inodes_total as f64 * 100.0;
                out.push(sample(
                    "disk_inodes_used_percent",
                    MetricValue::Float(pct),
                    &[("mount", mount.clone())],
                ));
            }
        }
    }
}

fn collect_system(out: &mut Vec<MetricSample>) {
    // uptime_ms is emitted directly as a raw ReadyMetric in mod.rs (bypasses
    // aggregation so it is never summed across the window).
    if let Some(n) = read_process_count() {
        out.push(sample("process_count", MetricValue::Int(n as i64), &[]));
    }
    if let Some((used, max)) = read_fd_count() {
        // Point-in-time gauges (current allocation vs. limit). Emitted raw, not
        // summed — fd_max is ~i64::MAX on modern kernels and summing it across
        // the window overflows f64 into meaningless values.
        out.push(gauge_sample("fd_used", MetricValue::Int(used as i64), &[]));
        out.push(gauge_sample("fd_max", MetricValue::Int(max as i64), &[]));
    }
}

// ---------------------------------------------------------------------------
// /proc and /sys readers
// ---------------------------------------------------------------------------

pub fn read_uptime_ms() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| {
            s.split_ascii_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
        })
        .map(|secs| (secs * 1000.0) as u64)
        .unwrap_or(0)
}

fn read_cpu_ticks() -> Option<CpuTicks> {
    let content = std::fs::read_to_string("/proc/stat").ok()?;
    let line = content.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_ascii_whitespace()
        .skip(1)
        .filter_map(|s| s.parse().ok())
        .collect();
    if fields.len() < 4 {
        return None;
    }
    // Fields: user nice system idle [iowait irq softirq steal ...]
    let total: u64 = fields.iter().sum();
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0); // idle + iowait
    Some(CpuTicks { total, idle })
}

fn read_load_avg() -> Option<(f64, f64, f64)> {
    let s = std::fs::read_to_string("/proc/loadavg").ok()?;
    let mut it = s.split_ascii_whitespace();
    Some((
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    ))
}

fn read_thermal_zones() -> Vec<(String, f64)> {
    let base = std::path::Path::new("/sys/class/thermal");
    if !base.exists() {
        return vec![];
    }
    let Ok(entries) = std::fs::read_dir(base) else {
        return vec![];
    };

    // First pass: collect raw (type_string, celsius) pairs.
    let mut raw: Vec<(String, f64)> = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("thermal_zone") {
            continue;
        }
        let path = entry.path();
        let Ok(temp_str) = std::fs::read_to_string(path.join("temp")) else {
            continue;
        };
        let temp_raw: i64 = temp_str.trim().parse().unwrap_or(0);
        let zone_type = std::fs::read_to_string(path.join("type"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| name.to_string_lossy().into_owned());
        raw.push((zone_type, temp_raw as f64 / 1000.0));
    }

    // I3: count occurrences of each type string.
    let mut type_count: HashMap<String, usize> = HashMap::new();
    for (t, _) in &raw {
        *type_count.entry(t.clone()).or_insert(0) += 1;
    }

    // Second pass: for duplicate type strings, append a 0-based index so each
    // zone gets a unique stream key in the aggregator (e.g. "x86_pkg_temp_0").
    let mut type_seen: HashMap<String, usize> = HashMap::new();
    let mut zones = Vec::with_capacity(raw.len());
    for (zone_type, celsius) in raw {
        let label = if *type_count.get(&zone_type).unwrap_or(&0) > 1 {
            let idx = type_seen.entry(zone_type.clone()).or_insert(0);
            let label = format!("{zone_type}_{idx}");
            *idx += 1;
            label
        } else {
            zone_type
        };
        zones.push((label, celsius));
    }
    zones
}

fn read_meminfo() -> Option<(u64, u64, u64, u64)> {
    // Returns (total_bytes, available_bytes, swap_total_bytes, swap_free_bytes)
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = 0u64;
    let mut available = 0u64;
    let mut swap_total = 0u64;
    let mut swap_free = 0u64;
    for line in content.lines() {
        let mut it = line.split_ascii_whitespace();
        let (Some(key), Some(val)) = (it.next(), it.next()) else {
            continue;
        };
        let kb: u64 = val.parse().unwrap_or(0);
        match key {
            "MemTotal:" => total = kb * 1024,
            "MemAvailable:" => available = kb * 1024,
            "SwapTotal:" => swap_total = kb * 1024,
            "SwapFree:" => swap_free = kb * 1024,
            _ => {}
        }
    }
    Some((total, available, swap_total, swap_free))
}

fn read_diskstats() -> HashMap<String, DiskCounters> {
    let mut map = HashMap::new();
    let Ok(content) = std::fs::read_to_string("/proc/diskstats") else {
        return map;
    };
    for line in content.lines() {
        let fields: Vec<&str> = line.split_ascii_whitespace().collect();
        if fields.len() < 10 {
            continue;
        }
        let dev = fields[2];
        // Skip virtual/pseudo devices.
        if dev.starts_with("loop") || dev.starts_with("ram") || dev.starts_with("sr") {
            continue;
        }
        // /proc/diskstats field layout (0-based after split):
        //   2: device  3: reads_completed  5: sectors_read  7: writes_completed
        //   9: sectors_written  12: ms_doing_io
        map.insert(
            dev.to_string(),
            DiskCounters {
                read_sectors: fields[5].parse().unwrap_or(0),
                write_sectors: fields[9].parse().unwrap_or(0),
                read_ops: fields[3].parse().unwrap_or(0),
                write_ops: fields[7].parse().unwrap_or(0),
                io_ms: fields.get(12).and_then(|s| s.parse().ok()),
            },
        );
    }
    map
}

/// Returns `(free_bytes, total_bytes, inodes_free, inodes_total)` for `path`.
fn disk_statvfs(path: &str) -> Option<(u64, u64, u64, u64)> {
    let c_path = std::ffi::CString::new(path).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if ret == 0 {
        Some((
            stat.f_bavail as u64 * stat.f_frsize as u64,
            stat.f_blocks as u64 * stat.f_frsize as u64,
            stat.f_favail as u64,
            stat.f_files as u64,
        ))
    } else {
        None
    }
}

fn read_net_dev() -> HashMap<String, NetCounters> {
    let mut map = HashMap::new();
    let Ok(content) = std::fs::read_to_string("/proc/net/dev") else {
        return map;
    };
    for line in content.lines().skip(2) {
        // Format: "  iface: rx_bytes rx_pkts rx_errs rx_drop rx_fifo rx_frame rx_comp rx_mcast
        //                   tx_bytes tx_pkts tx_errs tx_drop tx_fifo tx_colls tx_carr tx_comp"
        // Fields (0-based): 0=rx_bytes 2=rx_errs 3=rx_drop
        //                   8=tx_bytes 10=tx_errs 11=tx_drop (requires >= 12 fields)
        let Some(colon) = line.find(':') else {
            continue;
        };
        let iface = line[..colon].trim();
        if iface == "lo" {
            continue;
        }
        let fields: Vec<u64> = line[colon + 1..]
            .split_ascii_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        if fields.len() < 12 {
            continue;
        }
        map.insert(
            iface.to_string(),
            NetCounters {
                rx_bytes: fields[0],
                tx_bytes: fields[8],
                rx_errors: fields[2],
                tx_errors: fields[10],
                rx_drops: fields[3],
                tx_drops: fields[11],
            },
        );
    }
    map
}

fn read_process_count() -> Option<u32> {
    // /proc/loadavg field 4 is "running/total"; we want total.
    std::fs::read_to_string("/proc/loadavg")
        .ok()?
        .split_ascii_whitespace()
        .nth(3)?
        .split('/')
        .nth(1)?
        .parse()
        .ok()
}

/// Returns `(allocated_fds, max_fds)` from `/proc/sys/fs/file-nr`.
fn read_fd_count() -> Option<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/sys/fs/file-nr").ok()?;
    let mut it = s.split_ascii_whitespace();
    let used: u64 = it.next()?.parse().ok()?;
    let _legacy_free: u64 = it.next()?.parse().ok()?; // always 0 on Linux 2.6+
    let max: u64 = it.next()?.parse().ok()?;
    Some((used, max))
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn sample(
    name: &'static str,
    value: MetricValue,
    labels: &[(&'static str, String)],
) -> MetricSample {
    MetricSample {
        name: name.to_string(),
        value,
        labels: labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        counter: false,
        raw: false,
    }
}

/// Like `sample`, but marks the metric as a cumulative counter.
/// Counters bypass aggregation and are always emitted as raw values.
fn counter_sample(
    name: &'static str,
    value: MetricValue,
    labels: &[(&'static str, String)],
) -> MetricSample {
    MetricSample {
        name: name.to_string(),
        value,
        labels: labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        counter: true,
        raw: false,
    }
}

/// Like `sample`, but marks the metric as a point-in-time gauge that is emitted
/// instantaneously and never summed across the aggregation window.
fn gauge_sample(
    name: &'static str,
    value: MetricValue,
    labels: &[(&'static str, String)],
) -> MetricSample {
    MetricSample {
        name: name.to_string(),
        value,
        labels: labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        counter: false,
        raw: true,
    }
}
