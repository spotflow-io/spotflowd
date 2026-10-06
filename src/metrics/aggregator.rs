//! Per-stream aggregation with persistent sequence numbers.
//!
//! Each metric stream is identified by a key: `metric_name` for label-less
//! metrics, or `metric_name|key=val|key=val` (labels sorted) for labeled ones.
//!
//! For each stream, a `StreamState` accumulates sum/count/min/max until the
//! configured aggregation window elapses, then emits a `ReadyMetric`.
//!
//! Sequence numbers are stored in `<storage.state_dir>/metrics/metrics_seq.cbor` and loaded on
//! startup so they survive daemon restarts.

use super::{MetricSample, ReadyMetric, AGG_1DAY, AGG_1HOUR, AGG_1MIN, AGG_NONE};
use crate::config::MetricsConfig;
use anyhow::Result;
use ciborium::value::Value as CborValue;
use std::collections::HashMap;
use std::fmt::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Per-stream accumulator
// ---------------------------------------------------------------------------

struct StreamState {
    name: String,
    labels: Vec<(String, String)>,
    sum: f64,
    count: u64,
    min: f64,
    max: f64,
    window_start: Instant,
    uptime_ms: u64,
}

impl StreamState {
    fn new(name: String, labels: Vec<(String, String)>, v: f64, uptime_ms: u64) -> Self {
        Self {
            name,
            labels,
            sum: v,
            count: 1,
            min: v,
            max: v,
            window_start: Instant::now(),
            uptime_ms,
        }
    }

    fn update(&mut self, v: f64, uptime_ms: u64) {
        self.sum += v;
        self.count += 1;
        if v < self.min {
            self.min = v;
        }
        if v > self.max {
            self.max = v;
        }
        self.uptime_ms = uptime_ms;
    }
}

// ---------------------------------------------------------------------------
// Aggregator
// ---------------------------------------------------------------------------

/// Maximum number of distinct metric streams tracked simultaneously.
/// Protects against unbounded memory growth from high-cardinality labels
/// (e.g. unique request IDs) with long aggregation windows (1h, 1d).
const MAX_STREAMS: usize = 10_000;

pub struct Aggregator {
    agg_cbor: u8,
    agg_duration: Option<Duration>,
    states: HashMap<String, StreamState>,
    seq: HashMap<String, u64>,
    seq_path: PathBuf,
    dropped_streams: u64,
}

impl Aggregator {
    pub fn new(cfg: &MetricsConfig, seq_dir: PathBuf) -> Result<Self> {
        let (agg_cbor, agg_duration) = match cfg.aggregation_interval.as_str() {
            "none" => (AGG_NONE, None),
            "1m" => (AGG_1MIN, Some(Duration::from_secs(60))),
            "1h" => (AGG_1HOUR, Some(Duration::from_secs(3600))),
            "1d" => (AGG_1DAY, Some(Duration::from_secs(86400))),
            other => anyhow::bail!("unknown aggregation_interval: {other}"),
        };
        let seq_path = seq_dir.join("metrics_seq.cbor");
        let seq = load_seq(&seq_path);
        debug!("loaded {} persisted metric sequence numbers", seq.len());
        Ok(Self {
            agg_cbor,
            agg_duration,
            states: HashMap::new(),
            seq,
            seq_path,
            dropped_streams: 0,
        })
    }

    /// Feed samples from one collection tick.  Returns metrics ready to publish.
    pub fn ingest(&mut self, samples: Vec<MetricSample>, uptime_ms: u64) -> Vec<ReadyMetric> {
        let mut ready = Vec::new();

        for s in samples {
            let key = stream_key(&s.name, &s.labels);
            let v = s.value.as_f64();

            if self.agg_duration.is_none() || s.counter || s.raw {
                // AGG_NONE, cumulative counter, or point-in-time gauge: publish
                // immediately as a raw sample. Counters must never be summed (the
                // platform computes deltas server-side); raw gauges must never be
                // summed either (e.g. fd_max ≈ i64::MAX would overflow f64).
                let seq = self.next_seq(&key);
                ready.push(ReadyMetric {
                    name: s.name,
                    agg_cbor: AGG_NONE,
                    labels: s.labels,
                    sum: v,
                    count: None,
                    min: None,
                    max: None,
                    seq,
                    uptime_ms,
                });
            } else {
                // Timed aggregation: accumulate into stream state.
                if self.states.contains_key(&key) {
                    self.states.get_mut(&key).unwrap().update(v, uptime_ms);
                } else if self.states.len() < MAX_STREAMS {
                    self.states
                        .insert(key, StreamState::new(s.name, s.labels, v, uptime_ms));
                } else {
                    warn!("metric stream limit ({MAX_STREAMS}) reached, dropping: {key}");
                    self.dropped_streams += 1;
                }
            }
        }

        // Flush any streams whose aggregation window has elapsed.
        if let Some(duration) = self.agg_duration {
            let now = Instant::now();
            let expired: Vec<String> = self
                .states
                .iter()
                .filter(|(_, st)| now.duration_since(st.window_start) >= duration)
                .map(|(k, _)| k.clone())
                .collect();

            for key in expired {
                let st = self.states.remove(&key).unwrap();
                let seq = self.next_seq(&key);
                debug!(
                    "flush metric stream {key}: sum={:.3} count={} min={:.3} max={:.3}",
                    st.sum, st.count, st.min, st.max
                );
                ready.push(ReadyMetric {
                    name: st.name,
                    agg_cbor: self.agg_cbor,
                    labels: st.labels,
                    sum: st.sum,
                    count: Some(st.count),
                    min: Some(st.min),
                    max: Some(st.max),
                    seq,
                    uptime_ms: st.uptime_ms,
                });
            }
        }

        ready
    }

    /// Atomically persist sequence numbers to disk.
    pub fn save_seq(&self) -> Result<()> {
        if let Some(parent) = self.seq_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let map: Vec<(CborValue, CborValue)> = self
            .seq
            .iter()
            .map(|(k, v)| (CborValue::Text(k.clone()), CborValue::Integer((*v).into())))
            .collect();
        let mut buf = Vec::new();
        ciborium::into_writer(&CborValue::Map(map), &mut buf)
            .map_err(|e| anyhow::anyhow!("CBOR encode metrics_seq: {e}"))?;
        // Atomic write: write to .tmp then rename so a crash mid-write
        // never leaves a partially-written file.
        let tmp = self.seq_path.with_extension("cbor.tmp");
        std::fs::write(&tmp, &buf)?;
        std::fs::rename(&tmp, &self.seq_path)?;
        Ok(())
    }

    pub fn next_seq(&mut self, key: &str) -> u64 {
        let seq = self.seq.entry(key.to_string()).or_insert(0);
        *seq += 1;
        *seq
    }

    pub fn take_dropped_streams(&mut self) -> u64 {
        std::mem::take(&mut self.dropped_streams)
    }
}

// ---------------------------------------------------------------------------
// Stream key: "name" or "name|k=v|k=v" (labels assumed sorted by caller)
// ---------------------------------------------------------------------------

fn stream_key(name: &str, labels: &[(String, String)]) -> String {
    if labels.is_empty() {
        return name.to_string();
    }
    // M1: sort by key here so callers don't need to guarantee ordering.
    let mut sorted: Vec<_> = labels.iter().collect();
    sorted.sort_by_key(|(k, _)| k.as_str());
    // Single allocation: estimate capacity and write directly.
    let cap = name.len()
        + sorted
            .iter()
            .map(|(k, v)| k.len() + v.len() + 2)
            .sum::<usize>();
    let mut key = String::with_capacity(cap);
    key.push_str(name);
    for (k, v) in &sorted {
        key.push('|');
        let _ = write!(key, "{k}={v}");
    }
    key
}

// ---------------------------------------------------------------------------
// Sequence number persistence (CBOR map: tstr → uint)
// ---------------------------------------------------------------------------

fn load_seq(path: &PathBuf) -> HashMap<String, u64> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return HashMap::new(),
    };
    let value: CborValue = match ciborium::from_reader(data.as_slice()) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("failed to parse metrics_seq.cbor, starting fresh: {e}");
            return HashMap::new();
        }
    };
    let CborValue::Map(entries) = value else {
        return HashMap::new();
    };
    let mut map = HashMap::new();
    for (k, v) in entries {
        if let (CborValue::Text(key), CborValue::Integer(seq)) = (k, v) {
            if let Ok(n) = u64::try_from(seq) {
                map.insert(key, n);
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::Aggregator;
    use crate::config::MetricsConfig;
    use crate::metrics::{MetricSample, MetricValue, AGG_NONE};

    fn aggregator_1m() -> Aggregator {
        // MetricsConfig::default() uses aggregation_interval = "1m".
        let dir = tempfile::tempdir().unwrap();
        Aggregator::new(&MetricsConfig::default(), dir.path().to_path_buf()).unwrap()
    }

    fn make_sample(name: &str, value: MetricValue, counter: bool, raw: bool) -> MetricSample {
        MetricSample {
            name: name.to_string(),
            value,
            labels: vec![],
            counter,
            raw,
        }
    }

    #[test]
    fn raw_gauge_is_emitted_immediately_and_never_summed() {
        let mut agg = aggregator_1m();
        // fd_max ≈ i64::MAX — summing across a window would overflow f64.
        let big = i64::MAX;
        let s = make_sample("fd_max", MetricValue::Int(big), false, true);

        let r1 = agg.ingest(vec![s.clone()], 1000);
        let r2 = agg.ingest(vec![s.clone()], 2000);

        // Each tick emits its own raw reading under the "1m" window.
        assert_eq!(r1.len(), 1);
        assert_eq!(r2.len(), 1);
        assert_eq!(r1[0].agg_cbor, AGG_NONE);
        assert!(r1[0].count.is_none());
        // Value passes through untouched — not accumulated to ~2×.
        assert_eq!(r1[0].sum, big as f64);
        assert_eq!(r2[0].sum, big as f64);
    }

    #[test]
    fn plain_gauge_accumulates_within_the_window() {
        let mut agg = aggregator_1m();
        let s = make_sample("mem_used_percent", MetricValue::Float(40.0), false, false);
        // A normal gauge is buffered for aggregation, so nothing is emitted yet.
        assert!(agg.ingest(vec![s], 1000).is_empty());
    }
}
