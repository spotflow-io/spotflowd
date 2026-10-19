//! OS metric collector — reads /proc and /sys on each tick.
//!
//! Device-wide network byte counters and per-interface errors/drops are sent as
//! **cumulative totals**, matching the Zephyr SDK convention. The Spotflow
//! platform computes deltas and rates server-side.
//!
//! Disk I/O (`disk_read_bytes`, `disk_write_bytes`, `disk_read_ops`,
//! `disk_write_ops`) and per-interface throughput (`net_rx_bytes`,
//! `net_tx_bytes`) compute deltas locally so the values flow through
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
    // Observed traffic since collector startup. Keep totals across interface
    // disappearance/reset so the device-wide dashboard counters never decrease.
    network_rx_bytes: u64,
    network_tx_bytes: u64,
}

impl Collector {
    pub fn new() -> Self {
        Self {
            prev_cpu: None,
            prev_disk: HashMap::new(),
            prev_disk_time: None,
            prev_net: HashMap::new(),
            network_rx_bytes: 0,
            network_tx_bytes: 0,
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
                Some(cfg.network.interfaces.as_slice())
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

    fn collect_network(&mut self, out: &mut Vec<MetricSample>, filter: Option<&[String]>) {
        self.collect_network_snapshot(out, read_net_dev(), filter);
    }

    fn collect_network_snapshot(
        &mut self,
        out: &mut Vec<MetricSample>,
        curr: HashMap<String, NetCounters>,
        filter: Option<&[String]>,
    ) {
        for (iface, c) in &curr {
            if let Some(f) = filter {
                if !f.contains(iface) {
                    continue;
                }
            }
            let lbl = &[("interface", iface.clone())];

            // Preserve per-interface deltas for custom dashboards under separate
            // names: the built-in widget ignores labels and expects device totals.
            // The first reading of each interface establishes its baseline.
            if let Some(prev) = self.prev_net.get(iface) {
                let rx_delta = counter_delta(c.rx_bytes, prev.rx_bytes);
                let tx_delta = counter_delta(c.tx_bytes, prev.tx_bytes);
                self.network_rx_bytes = self.network_rx_bytes.saturating_add(rx_delta);
                self.network_tx_bytes = self.network_tx_bytes.saturating_add(tx_delta);
                out.push(sample(
                    "net_rx_bytes",
                    MetricValue::Int(rx_delta as i64),
                    lbl,
                ));
                out.push(sample(
                    "net_tx_bytes",
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
        // One unlabeled cumulative stream per direction, including an initial
        // zero baseline. Bypass timed aggregation: summing counters would make
        // the dashboard's server-side delta/rate calculation incorrect.
        out.push(counter_sample(
            "network_rx_bytes",
            MetricValue::Int(self.network_rx_bytes as i64),
            &[],
        ));
        out.push(counter_sample(
            "network_tx_bytes",
            MetricValue::Int(self.network_tx_bytes as i64),
            &[],
        ));
        self.prev_net = curr;
    }
}

// ---------------------------------------------------------------------------
// Stateless collectors (no inter-tick state needed)
// ---------------------------------------------------------------------------

fn counter_delta(current: u64, previous: u64) -> u64 {
    // A lower reading indicates that the kernel/interface counter reset. Count
    // traffic since that reset rather than decreasing the device-wide total.
    current.checked_sub(previous).unwrap_or(current)
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MetricsConfig;
    use crate::metrics::aggregator::Aggregator;
    use crate::metrics::AGG_NONE;

    fn network_snapshot(interfaces: &[(&str, u64, u64)]) -> HashMap<String, NetCounters> {
        interfaces
            .iter()
            .map(|&(name, rx_bytes, tx_bytes)| {
                (
                    name.to_string(),
                    NetCounters {
                        rx_bytes,
                        tx_bytes,
                        rx_errors: 0,
                        tx_errors: 0,
                        rx_drops: 0,
                        tx_drops: 0,
                    },
                )
            })
            .collect()
    }

    fn collect(
        collector: &mut Collector,
        interfaces: &[(&str, u64, u64)],
        filter: Option<&[String]>,
    ) -> Vec<MetricSample> {
        let mut samples = Vec::new();
        collector.collect_network_snapshot(&mut samples, network_snapshot(interfaces), filter);
        samples
    }

    fn assert_totals(samples: &[MetricSample], rx: u64, tx: u64) {
        for (name, expected) in [("network_rx_bytes", rx), ("network_tx_bytes", tx)] {
            let totals: Vec<_> = samples.iter().filter(|s| s.name == name).collect();
            assert_eq!(totals.len(), 1, "expected one device-wide {name} stream");
            let total = totals[0];
            assert!(total.labels.is_empty());
            assert!(total.counter);
            assert_eq!(total.value.as_f64(), expected as f64);
        }
    }

    #[test]
    fn network_totals_are_cumulative_and_keep_per_interface_deltas() {
        let mut collector = Collector::new();
        let first = collect(&mut collector, &[("eth0", 1000, 2000)], None);
        assert_totals(&first, 0, 0);
        assert!(!first.iter().any(|s| s.name == "net_rx_bytes"));

        let second = collect(&mut collector, &[("eth0", 1600, 2900)], None);
        assert_totals(&second, 600, 900);
        for (name, expected) in [("net_rx_bytes", 600.0), ("net_tx_bytes", 900.0)] {
            let sample = second.iter().find(|s| s.name == name).unwrap();
            assert_eq!(sample.labels, vec![("interface".into(), "eth0".into())]);
            assert!(!sample.counter);
            assert_eq!(sample.value.as_f64(), expected);
        }

        let third = collect(&mut collector, &[("eth0", 1800, 3300)], None);
        assert_totals(&third, 800, 1300);
    }

    #[test]
    fn network_totals_sum_interfaces_instead_of_reporting_the_largest() {
        let mut collector = Collector::new();
        collect(
            &mut collector,
            &[("eth0", 1000, 2000), ("wlan0", 500, 100)],
            None,
        );
        let samples = collect(
            &mut collector,
            &[("eth0", 1400, 2100), ("wlan0", 800, 150)],
            None,
        );
        assert_totals(&samples, 700, 150);
        assert_eq!(
            samples.iter().filter(|s| s.name == "net_rx_bytes").count(),
            2
        );
    }

    #[test]
    fn network_filter_applies_to_totals_and_per_interface_metrics() {
        let mut collector = Collector::new();
        let filter = vec!["wlan0".to_string()];
        collect(
            &mut collector,
            &[("eth0", 1000, 2000), ("wlan0", 100, 200)],
            Some(&filter),
        );
        let samples = collect(
            &mut collector,
            &[("eth0", 9000, 9000), ("wlan0", 400, 600)],
            Some(&filter),
        );
        assert_totals(&samples, 300, 400);
        for sample in samples.iter().filter(|s| !s.labels.is_empty()) {
            assert_eq!(sample.labels, vec![("interface".into(), "wlan0".into())]);
        }
    }

    #[test]
    fn network_totals_survive_interface_disappearance_and_reappearance() {
        let mut collector = Collector::new();
        collect(&mut collector, &[("eth0", 1000, 2000)], None);
        assert_totals(
            &collect(&mut collector, &[("eth0", 1100, 2200)], None),
            100,
            200,
        );
        assert_totals(&collect(&mut collector, &[], None), 100, 200);
        // A reappearing/new interface establishes a fresh baseline, rather than
        // injecting its entire historical traffic into the device-wide counter.
        assert_totals(
            &collect(&mut collector, &[("eth0", 5000, 6000)], None),
            100,
            200,
        );
        assert_totals(
            &collect(&mut collector, &[("eth0", 5050, 6100)], None),
            150,
            300,
        );
    }

    #[test]
    fn network_counter_resets_do_not_decrease_device_totals() {
        let mut collector = Collector::new();
        collect(&mut collector, &[("eth0", 1000, 2000)], None);
        assert_totals(
            &collect(&mut collector, &[("eth0", 1100, 2300)], None),
            100,
            300,
        );
        assert_totals(
            &collect(&mut collector, &[("eth0", 20, 2400)], None),
            120,
            400,
        );
        assert_totals(
            &collect(&mut collector, &[("eth0", 50, 10)], None),
            150,
            410,
        );
    }

    #[test]
    fn network_totals_bypass_all_aggregation_windows() {
        let dir = tempfile::tempdir().unwrap();
        for interval in ["none", "1m", "1h", "1d"] {
            let config = MetricsConfig {
                aggregation_interval: interval.into(),
                ..MetricsConfig::default()
            };
            let mut aggregator = Aggregator::new(&config, dir.path().join(interval)).unwrap();
            let mut collector = Collector::new();
            let baseline = collect(&mut collector, &[("eth0", 1000, 2000)], None);
            aggregator.ingest(baseline, 1000);
            let samples = collect(&mut collector, &[("eth0", 1300, 2500)], None);
            let ready = aggregator.ingest(samples, 2000);
            for (name, expected) in [("network_rx_bytes", 300.0), ("network_tx_bytes", 500.0)] {
                let totals: Vec<_> = ready.iter().filter(|m| m.name == name).collect();
                assert_eq!(totals.len(), 1);
                let total = totals[0];
                assert!(total.labels.is_empty());
                assert_eq!(total.agg_cbor, AGG_NONE);
                assert_eq!(total.sum, expected);
                assert_eq!(total.seq, 2);
                assert!(total.count.is_none());
                assert!(total.min.is_none());
                assert!(total.max.is_none());
            }
        }
    }
}
