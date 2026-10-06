//! Metrics subsystem — collects OS metrics and publishes them to Spotflow.
//!
//! Architecture:
//!   Collector (reads /proc, /sys every `metrics.os.collection_interval_secs`)
//!     → Aggregator (accumulates sum/count/min/max per stream per window)
//!       → Bounded memory queue → concurrent MQTT delivery
//!
//! Aggregation intervals mirror the Spotflow / Zephyr SDK:
//!   "none" → each sample published immediately (raw, sum only)
//!   "1m"   → one message per stream per minute with sum/count/min/max
//!   "1h"   → one message per stream per hour
//!   "1d"   → one message per stream per day
//!
//! Sequence numbers are persisted across restarts in
//!   `<storage.state_dir>/metrics/metrics_seq.cbor`
//! using an atomic write (write-then-rename) to prevent corruption.

pub mod aggregator;
pub mod collector;
pub mod publisher;
pub mod socket;

use crate::config::MetricsConfig;
use crate::mqtt::MqttPublisher;
use crate::status::{now_ms, StatusHandle};
use anyhow::Result;
use std::future::Future;
use std::path::PathBuf;
use tokio::sync::{mpsc, watch};
use tokio::time::{interval, Duration, MissedTickBehavior};
use tracing::warn;

// ---------------------------------------------------------------------------
// CBOR aggregation-interval codes (Spotflow spec, key 22)
// ---------------------------------------------------------------------------
pub const AGG_NONE: u8 = 0;
pub const AGG_1MIN: u8 = 1;
pub const AGG_1HOUR: u8 = 3;
pub const AGG_1DAY: u8 = 4;

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

/// A raw value from the OS.
#[derive(Debug, Clone, Copy)]
pub enum MetricValue {
    Int(i64),
    Float(f64),
}

impl MetricValue {
    pub fn as_f64(self) -> f64 {
        match self {
            MetricValue::Int(i) => i as f64,
            MetricValue::Float(f) => f,
        }
    }
}

/// A single sample produced by the collector or a custom app on each tick.
#[derive(Debug, Clone)]
pub struct MetricSample {
    /// Metric name. Owned so custom apps can supply dynamic names.
    pub name: String,
    pub value: MetricValue,
    /// (key, value) pairs. Sorted by key for consistent stream-key generation.
    pub labels: Vec<(String, String)>,
    /// True for absolute cumulative counters (e.g. network_rx_bytes).
    /// These bypass aggregation and are always emitted as raw values —
    /// the Spotflow platform computes deltas server-side.
    pub counter: bool,
    /// True for point-in-time gauges that must be reported instantaneously and
    /// never summed across the aggregation window (e.g. fd_max, which is close
    /// to i64::MAX — summing it overflows f64 precision into nonsense). Unlike
    /// `counter`, the value is not cumulative, so the platform must not delta it.
    pub raw: bool,
}

/// A metric ready to publish — either raw (agg=none) or fully aggregated.
#[derive(Debug)]
pub struct ReadyMetric {
    pub name: String,
    pub agg_cbor: u8,
    pub labels: Vec<(String, String)>,
    /// Always present (raw value for NONE, aggregated sum otherwise).
    pub sum: f64,
    /// None for AGG_NONE — omit count/min/max from payload per spec.
    pub count: Option<u64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub seq: u64,
    pub uptime_ms: u64,
}

// ---------------------------------------------------------------------------
// Main task
// ---------------------------------------------------------------------------

/// Maximum number of queued metrics, including the reading being published.
/// When exceeded, the oldest waiting entries are dropped to bound memory usage.
const MAX_PENDING: usize = 1000;

/// How often sequence numbers are persisted to disk (I1: flash wear reduction).
const SEQ_SAVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

pub async fn run(
    cfg: MetricsConfig,
    seq_dir: PathBuf,
    publisher: MqttPublisher,
    shutdown: watch::Receiver<bool>,
    status: StatusHandle,
) -> Result<()> {
    let connection = publisher.clone();
    run_with_transport(
        cfg,
        seq_dir,
        move || connection.is_connected(),
        move |payload| {
            let publisher = publisher.clone();
            async move { publisher.publish_payload(payload).await }
        },
        shutdown,
        status,
    )
    .await
}

async fn run_with_transport<C, P, F>(
    cfg: MetricsConfig,
    seq_dir: PathBuf,
    connected: C,
    publish: P,
    mut shutdown: watch::Receiver<bool>,
    status: StatusHandle,
) -> Result<()>
where
    C: Fn() -> bool,
    P: FnMut(Vec<u8>) -> F,
    F: Future<Output = Result<()>>,
{
    let mut coll = collector::Collector::new();
    let mut agg = aggregator::Aggregator::new(&cfg, seq_dir)?;
    let mut ticker = interval(Duration::from_secs(cfg.os.collection_interval_secs));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Spawn the custom metrics socket listener if enabled.
    let mut custom_rx: Option<mpsc::Receiver<MetricSample>> = None;
    if cfg.custom.enabled {
        let (tx, rx) = mpsc::channel::<MetricSample>(1024);
        custom_rx = Some(rx);
        let custom_cfg = cfg.custom.clone();
        let shutdown_rx = shutdown.clone();
        let socket_status = status.clone();
        tokio::spawn(async move {
            if let Err(e) = socket::run(custom_cfg, tx, shutdown_rx, socket_status.clone()).await {
                socket_status.source_error("custom_metrics", &e);
                warn!("custom metrics socket failed: {e}");
            }
        });
    }

    let pending = publisher::PendingMetrics::new(status.clone());
    let delivery = publisher::run(pending.clone(), connected, publish, shutdown.clone());
    // I1: track last save time to throttle disk writes.
    let mut seq_dirty = false;
    let mut last_seq_save = std::time::Instant::now();

    let collection = async {
        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                _ = ticker.tick() => {
                    // Collect OS metrics (if enabled) and read current uptime.
                    let (mut samples, uptime_ms) = if cfg.os.enabled {
                        let collected = coll.collect(&cfg.os);
                        status.source_collected("os_metrics");
                        collected
                    } else {
                        (Vec::new(), collector::read_uptime_ms())
                    };

                    // Drain any custom metrics that arrived since the last tick.
                    if let Some(ref mut rx) = custom_rx {
                        while let Ok(s) = rx.try_recv() {
                            samples.push(s);
                        }
                    }

                    let mut ready = agg.ingest(samples, uptime_ms);
                    let dropped_streams = agg.take_dropped_streams();
                    if dropped_streams > 0 {
                        status.record_drop("metric_stream_limit", dropped_streams);
                    }

                    // uptime_ms is a gauge — it must NOT be aggregated (summed).
                    // Emit it directly as a raw reading on every tick so the
                    // platform always sees the true instantaneous uptime.
                    if cfg.os.enabled && cfg.os.groups.system {
                        let seq = agg.next_seq("uptime_ms");
                        ready.push(ReadyMetric {
                            name: "uptime_ms".to_string(),
                            agg_cbor: AGG_NONE,
                            labels: vec![],
                            sum: uptime_ms as f64,
                            count: None,
                            min: None,
                            max: None,
                            seq,
                            uptime_ms,
                        });
                    }

                    if let Err(error) = pending.enqueue(ready, now_ms()) {
                        warn!("metrics encode error: {error}");
                    }

                    seq_dirty |= pending.take_published();
                    // Persist sequence numbers at most once per minute, without
                    // waiting for delivery or writing merely because we're offline.
                    if seq_dirty && last_seq_save.elapsed() >= SEQ_SAVE_INTERVAL {
                        if let Err(e) = agg.save_seq() {
                            status.storage_failure("metrics_sequence", &e);
                            warn!("failed to save metric sequence numbers: {e}");
                        } else {
                            status.storage_success("metrics_sequence");
                            seq_dirty = false;
                        }
                        last_seq_save = std::time::Instant::now();
                    }
                }
            }
        }
    };
    // Join both loops so shutdown cancels delivery and sequence persistence
    // finishes before the metrics subsystem returns.
    tokio::join!(collection, delivery);

    // Always persist seq numbers on clean shutdown.
    seq_dirty |= pending.take_published();
    if seq_dirty {
        if let Err(e) = agg.save_seq() {
            status.storage_failure("metrics_sequence", &e);
            warn!("failed to save metric sequence numbers on shutdown: {e}");
        } else {
            status.storage_success("metrics_sequence");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, MetricsCustomConfig, MetricsGroupsConfig, MetricsOsConfig};
    use rumqttc::{AsyncClient, MqttOptions, QoS};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;
    use tokio::time::{sleep, timeout};

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        timeout(Duration::from_secs(5), async {
            while !condition() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("metrics did not make progress");
    }

    #[tokio::test]
    async fn custom_metrics_publish_when_os_collection_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut config: Config = toml::from_str(
            "[device]\nid = 'test'\ningest_key = 'test'\n\
             [metrics]\naggregation_interval = 'none'\n\
             [metrics.os]\nenabled = false\ncollection_interval_secs = 1\n\
             [metrics.custom]\nenabled = true\n",
        )
        .unwrap();
        config.storage.state_dir = dir.path().join("state");
        config.metrics.custom.socket_path = dir.path().join("metrics.sock");
        config.validate().unwrap();
        let status = StatusHandle::new(dir.path().join("config.toml"), &config);
        let (sent_tx, mut sent_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run_with_transport(
            config.metrics.clone(),
            config.storage.metrics_dir(),
            || true,
            move |payload| {
                let sent_tx = sent_tx.clone();
                async move {
                    sent_tx.send(payload)?;
                    Ok(())
                }
            },
            shutdown_rx,
            status.clone(),
        ));

        wait_until(|| status.snapshot().sources["custom_metrics"].health == "healthy").await;
        let mut stream = UnixStream::connect(&config.metrics.custom.socket_path)
            .await
            .unwrap();
        stream
            .write_all(b"{\"name\":\"queue_depth\",\"value\":42}\n")
            .await
            .unwrap();
        let payload = timeout(Duration::from_secs(5), sent_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let metric: ciborium::value::Value = ciborium::from_reader(payload.as_slice()).unwrap();
        let ciborium::value::Value::Map(fields) = metric else {
            panic!("expected a metric map");
        };
        assert!(fields.contains(&(
            ciborium::value::Value::Integer(21.into()),
            ciborium::value::Value::Text("queue_depth".into()),
        )));

        let snapshot = status.snapshot();
        assert!(!snapshot.sources["os_metrics"].enabled);
        assert_eq!(snapshot.sources["os_metrics"].health, "disabled");
        assert!(snapshot.sources["os_metrics"]
            .last_successful_collection_ms
            .is_none());
        assert!(snapshot.sources["custom_metrics"]
            .last_successful_collection_ms
            .is_some());
        assert!(sent_rx.try_recv().is_err());

        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let file =
            std::fs::File::open(config.storage.metrics_dir().join("metrics_seq.cbor")).unwrap();
        let sequences: HashMap<String, u64> = ciborium::from_reader(file).unwrap();
        assert_eq!(sequences["queue_depth"], 1);
        assert!(!config.storage.spool_dir().exists());
        assert!(!config.storage.crashdump_dir().exists());
    }

    #[tokio::test]
    async fn collection_and_shutdown_continue_when_mqtt_queue_is_full() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = MetricsConfig {
            aggregation_interval: "none".into(),
            os: MetricsOsConfig {
                enabled: true,
                collection_interval_secs: 1,
                groups: MetricsGroupsConfig {
                    cpu: false,
                    memory: false,
                    disk: false,
                    network: false,
                    system: true,
                },
                ..MetricsOsConfig::default()
            },
            custom: MetricsCustomConfig {
                enabled: true,
                socket_path: dir.path().join("metrics.sock"),
            },
        };
        let mut config: Config =
            toml::from_str("[device]\nid = 'test'\ningest_key = 'test'\n").unwrap();
        config.metrics = cfg.clone();
        config.storage.state_dir = dir.path().join("state");
        let status = StatusHandle::new(dir.path().join("config.toml"), &config);
        // Leave the real rumqttc event loop unpolled. One publish fits in the
        // local queue; the next must wait until shutdown cancels delivery.
        let (client, _eventloop) = AsyncClient::new(MqttOptions::new("test", "127.0.0.1", 1883), 1);
        let attempts = Arc::new(AtomicUsize::new(0));
        let publish_attempts = attempts.clone();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run_with_transport(
            cfg.clone(),
            config.storage.metrics_dir(),
            || true,
            move |payload| {
                publish_attempts.fetch_add(1, Ordering::Relaxed);
                let client = client.clone();
                async move {
                    client
                        .publish("ingest-cbor", QoS::AtMostOnce, false, payload)
                        .await
                        .map_err(anyhow::Error::from)
                }
            },
            shutdown_rx,
            status.clone(),
        ));

        wait_until(|| {
            attempts.load(Ordering::Relaxed) == 2
                && status.snapshot().sources["custom_metrics"].health == "healthy"
        })
        .await;
        let mut stream = UnixStream::connect(&cfg.custom.socket_path).await.unwrap();
        for value in [7, 8] {
            let before = status.snapshot().sources["custom_metrics"].last_successful_collection_ms;
            stream
                .write_all(
                    format!("{{\"name\":\"blocked_custom\",\"value\":{value}}}\n").as_bytes(),
                )
                .await
                .unwrap();
            wait_until(|| {
                status.snapshot().sources["custom_metrics"].last_successful_collection_ms > before
            })
            .await;
            let before = status.snapshot().sources["os_metrics"].last_successful_collection_ms;
            wait_until(|| {
                status.snapshot().sources["os_metrics"].last_successful_collection_ms > before
            })
            .await;
        }
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
        let queue = status.snapshot().queue;
        assert!(queue.records > 1);
        assert!(queue.records <= MAX_PENDING as u64);
        assert!(queue.bytes > 0);
        assert!(queue.oldest_record_ms.is_some());

        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_millis(500), task)
            .await
            .expect("shutdown waited for the blocked MQTT publish")
            .unwrap()
            .unwrap();
        let file =
            std::fs::File::open(config.storage.metrics_dir().join("metrics_seq.cbor")).unwrap();
        let sequences: HashMap<String, u64> = ciborium::from_reader(file).unwrap();
        // Both custom readings reached the aggregator during backpressure,
        // and OS metrics were collected on at least three consecutive ticks.
        assert_eq!(sequences["blocked_custom"], 2);
        assert!(sequences["uptime_ms"] >= 3);
    }
}
