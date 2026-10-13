//! Unix domain socket listener for custom application metrics.
//!
//! Any process on the same machine can publish a metric by connecting to the
//! socket (default: `/run/spotflow/metrics.sock`) and writing one or more
//! newline-delimited JSON messages:
//!
//! ```json
//! {"name": "queue_depth", "value": 42}
//! {"name": "job_duration_ms", "value": 183.5, "labels": {"worker": "main"}}
//! ```
//!
//! The connection is closed after sending. No response is sent back.
//! Received samples are fed into the same aggregator pipeline as OS metrics,
//! so they benefit from the configured aggregation window and are published
//! to Spotflow over MQTT.

use super::{MetricSample, MetricValue};
use crate::config::MetricsCustomConfig;
use crate::status::StatusHandle;
use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

#[derive(Deserialize)]
struct IncomingMetric {
    name: String,
    value: f64,
    #[serde(default)]
    labels: HashMap<String, String>,
}

pub async fn run(
    cfg: MetricsCustomConfig,
    tx: mpsc::Sender<MetricSample>,
    mut shutdown: watch::Receiver<bool>,
    status: StatusHandle,
) -> Result<()> {
    // Ensure the socket directory exists and is world-traversable.
    if let Some(parent) = cfg.socket_path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755));
        }
    }

    // Remove a stale socket file left by a previous run.
    let _ = std::fs::remove_file(&cfg.socket_path);

    let listener = UnixListener::bind(&cfg.socket_path)?;
    status.source_ready("custom_metrics");

    // Allow any local user to connect — the socket carries no authentication,
    // but it is local-only and the worst a malicious sender can do is inject
    // bogus metric values.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cfg.socket_path, std::fs::Permissions::from_mode(0o666))?;
    }

    info!(
        "custom metrics socket listening on {}",
        cfg.socket_path.display()
    );

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _)) => {
                        let tx2 = tx.clone();
                        let status = status.clone();
                        tokio::spawn(async move {
                            handle_connection(stream, tx2, status).await;
                        });
                    }
                    Err(e) => warn!("custom metrics socket accept error: {e}"),
                }
            }
            _ = shutdown.changed() => break,
        }
    }

    let _ = std::fs::remove_file(&cfg.socket_path);
    Ok(())
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    tx: mpsc::Sender<MetricSample>,
    status: StatusHandle,
) {
    let mut lines = BufReader::new(stream).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<IncomingMetric>(&line) {
            Ok(msg) => {
                debug!(
                    "custom metric received: name={} value={}",
                    msg.name, msg.value
                );
                // Sort labels so the aggregator generates a consistent stream key.
                let mut labels: Vec<(String, String)> = msg.labels.into_iter().collect();
                labels.sort_by(|(a, _), (b, _)| a.cmp(b));
                let sample = MetricSample {
                    name: msg.name,
                    value: MetricValue::Float(msg.value),
                    labels,
                    counter: false,
                    raw: false,
                };
                if tx.try_send(sample).is_err() {
                    // Channel is full — the metrics loop isn't keeping up.
                    warn!("custom metrics channel full, dropping metric");
                    status.record_drop("custom_metrics_channel_full", 1);
                    break;
                }
                status.source_collected("custom_metrics");
            }
            Err(e) => {
                status.record_drop("invalid_custom_metric", 1);
                warn!("invalid custom metric JSON ({e}): {line}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Option<IncomingMetric> {
        serde_json::from_str(line).ok()
    }

    #[test]
    fn parse_minimal() {
        let m = parse(r#"{"name":"queue_depth","value":42}"#).unwrap();
        assert_eq!(m.name, "queue_depth");
        assert_eq!(m.value, 42.0);
        assert!(m.labels.is_empty());
    }

    #[test]
    fn parse_with_labels() {
        let m = parse(r#"{"name":"latency_ms","value":8.5,"labels":{"worker":"main"}}"#).unwrap();
        assert_eq!(m.name, "latency_ms");
        assert_eq!(m.value, 8.5);
        assert_eq!(m.labels.get("worker").map(String::as_str), Some("main"));
    }

    #[test]
    fn parse_extra_fields_ignored() {
        // Future clients may add fields — they must not break parsing.
        let m = parse(r#"{"name":"x","value":1,"unit":"ms","unknown":true}"#).unwrap();
        assert_eq!(m.name, "x");
    }

    #[test]
    fn parse_invalid_returns_none() {
        assert!(parse("not json").is_none());
        assert!(parse(r#"{"value":1}"#).is_none()); // missing name
        assert!(parse(r#"{"name":"x"}"#).is_none()); // missing value
    }
}
