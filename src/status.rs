//! Offline local diagnostics served over a Unix domain socket.

use crate::config::Config;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceStatus {
    pub enabled: bool,
    pub health: String,
    pub last_successful_collection_ms: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionStatus {
    pub state: String,
    /// Last local MQTT publish event; QoS 0 has no delivery acknowledgement.
    pub last_publish_ms: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueStatus {
    pub records: u64,
    pub bytes: u64,
    pub oldest_record_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageStatus {
    pub healthy: bool,
    pub failure_count: u64,
    pub last_failure_ms: Option<u64>,
    pub last_error: Option<String>,
    pub last_recovery_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub agent_version: String,
    pub started_at_ms: u64,
    pub config_path: PathBuf,
    pub active_configuration: serde_json::Value,
    pub sources: BTreeMap<String, SourceStatus>,
    pub connection: ConnectionStatus,
    /// Local log backlog and pending metrics, excluding the MQTT transport queue.
    pub queue: QueueStatus,
    pub drops_by_reason: BTreeMap<String, u64>,
    pub storage: StorageStatus,
}

#[derive(Clone)]
pub struct StatusHandle {
    inner: Arc<Mutex<StatusSnapshot>>,
    queue: Arc<Mutex<QueueComponents>>,
    storage_errors: Arc<Mutex<BTreeMap<String, String>>>,
}

#[derive(Default)]
struct QueueComponents {
    local: QueueStatus,
    metrics: QueueStatus,
}

impl StatusHandle {
    pub fn new(config_path: PathBuf, cfg: &Config) -> Self {
        let mut sources = BTreeMap::new();
        sources.insert("journald".to_string(), source(cfg.logs.journald));
        sources.insert("syslog".to_string(), source(cfg.logs.syslog));
        sources.insert("os_metrics".to_string(), source(cfg.metrics.os.enabled));
        sources.insert(
            "custom_metrics".to_string(),
            source(cfg.metrics.custom.enabled),
        );
        sources.insert("crashdump".to_string(), source(cfg.crashdump.enabled));

        Self {
            inner: Arc::new(Mutex::new(StatusSnapshot {
                agent_version: env!("CARGO_PKG_VERSION").to_string(),
                started_at_ms: now_ms(),
                config_path,
                active_configuration: cfg.redacted_json(),
                sources,
                connection: ConnectionStatus {
                    state: "connecting".to_string(),
                    last_publish_ms: None,
                    last_error: None,
                },
                queue: QueueStatus::default(),
                drops_by_reason: BTreeMap::new(),
                storage: StorageStatus {
                    healthy: true,
                    failure_count: 0,
                    last_failure_ms: None,
                    last_error: None,
                    last_recovery_ms: None,
                },
            })),
            queue: Arc::new(Mutex::new(QueueComponents::default())),
            storage_errors: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn snapshot(&self) -> StatusSnapshot {
        self.inner.lock().expect("status lock poisoned").clone()
    }

    pub fn source_ready(&self, name: &str) {
        self.update_source(name, false, None);
    }

    pub fn source_collected(&self, name: &str) {
        self.update_source(name, true, None);
    }

    pub fn source_error(&self, name: &str, error: &dyn std::fmt::Display) {
        self.update_source(name, false, Some(error.to_string()));
    }

    fn update_source(&self, name: &str, collected: bool, error: Option<String>) {
        if let Some(source) = self
            .inner
            .lock()
            .expect("status lock poisoned")
            .sources
            .get_mut(name)
        {
            if !source.enabled {
                return;
            }
            if let Some(error) = error {
                source.health = "error".to_string();
                source.last_error = Some(error);
            } else {
                source.health = "healthy".to_string();
                source.last_error = None;
                if collected {
                    source.last_successful_collection_ms = Some(now_ms());
                }
            }
        }
    }

    pub fn connected(&self) {
        let mut status = self.inner.lock().expect("status lock poisoned");
        status.connection.state = "connected".to_string();
        status.connection.last_error = None;
    }

    pub fn publish_sent(&self) {
        self.inner
            .lock()
            .expect("status lock poisoned")
            .connection
            .last_publish_ms = Some(now_ms());
    }

    pub fn disconnected(&self, error: &dyn std::fmt::Display) {
        let mut status = self.inner.lock().expect("status lock poisoned");
        status.connection.state = "disconnected".to_string();
        status.connection.last_error = Some(error.to_string());
    }

    pub fn set_queue(&self, queue: QueueStatus) {
        let mut components = self.queue.lock().expect("status queue lock poisoned");
        components.local = queue;
        self.update_queue_snapshot(&components);
    }

    pub fn set_metrics_queue(&self, queue: QueueStatus) {
        let mut components = self.queue.lock().expect("status queue lock poisoned");
        components.metrics = queue;
        self.update_queue_snapshot(&components);
    }

    fn update_queue_snapshot(&self, components: &QueueComponents) {
        self.inner.lock().expect("status lock poisoned").queue = QueueStatus {
            records: components.local.records + components.metrics.records,
            bytes: components.local.bytes + components.metrics.bytes,
            oldest_record_ms: oldest(
                components.local.oldest_record_ms,
                components.metrics.oldest_record_ms,
            ),
        };
    }

    pub fn record_drop(&self, reason: &str, count: u64) {
        *self
            .inner
            .lock()
            .expect("status lock poisoned")
            .drops_by_reason
            .entry(reason.to_string())
            .or_default() += count;
    }

    pub fn storage_failure(&self, operation: &str, error: &dyn std::fmt::Display) {
        let error = error.to_string();
        self.storage_errors
            .lock()
            .expect("storage errors lock poisoned")
            .insert(operation.to_string(), error.clone());
        let mut status = self.inner.lock().expect("status lock poisoned");
        status.storage.healthy = false;
        status.storage.failure_count += 1;
        status.storage.last_failure_ms = Some(now_ms());
        status.storage.last_error = Some(error);
    }

    pub fn storage_success(&self, operation: &str) {
        let mut errors = self
            .storage_errors
            .lock()
            .expect("storage errors lock poisoned");
        if errors.remove(operation).is_none() {
            return;
        }
        let mut status = self.inner.lock().expect("status lock poisoned");
        if errors.is_empty() {
            status.storage.healthy = true;
            status.storage.last_error = None;
            status.storage.last_recovery_ms = Some(now_ms());
        } else {
            status.storage.last_error = errors.values().next().cloned();
        }
    }
}

fn source(enabled: bool) -> SourceStatus {
    SourceStatus {
        enabled,
        health: if enabled { "starting" } else { "disabled" }.to_string(),
        last_successful_collection_ms: None,
        last_error: None,
    }
}

fn oldest(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

pub fn start(
    path: PathBuf,
    status: StatusHandle,
    mut shutdown: watch::Receiver<bool>,
) -> Result<tokio::task::JoinHandle<()>> {
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            anyhow::bail!("status socket is already in use: {}", path.display());
        }
        std::fs::remove_file(&path)
            .with_context(|| format!("remove stale status socket {}", path.display()))?;
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind status socket {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660))?;
    info!("status socket listening on {}", path.display());

    Ok(tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((mut stream, _)) = accepted else { continue };
                    let Ok(payload) = serde_json::to_vec(&status.snapshot()) else { continue };
                    tokio::spawn(async move {
                        let _ = stream.write_all(&payload).await;
                        let _ = stream.shutdown().await;
                    });
                }
                _ = shutdown.changed() => break,
            }
        }
        drop(listener);
        let _ = std::fs::remove_file(path);
    }))
}

pub async fn fetch(path: &Path) -> Result<StatusSnapshot> {
    let mut stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("cannot connect to status socket {}", path.display()))?;
    let mut data = Vec::new();
    stream.read_to_end(&mut data).await?;
    serde_json::from_slice(&data).context("invalid response from status socket")
}

pub fn print_human(status: &StatusSnapshot) {
    println!("Agent: spotflowd {}", status.agent_version);
    println!("Config: {}", status.config_path.display());
    println!("Started: {}", display_time(Some(status.started_at_ms)));
    println!(
        "Connection: {} (last local MQTT publish: {})",
        status.connection.state,
        display_time(status.connection.last_publish_ms)
    );
    if let Some(error) = &status.connection.last_error {
        println!("Connection error: {error}");
    }
    println!(
        "Queue: {} records, {} bytes, oldest: {}",
        status.queue.records,
        status.queue.bytes,
        display_time(status.queue.oldest_record_ms)
    );
    println!("Sources:");
    for (name, source) in &status.sources {
        println!(
            "  {name}: {} (last collection: {})",
            source.health,
            display_time(source.last_successful_collection_ms)
        );
        if let Some(error) = &source.last_error {
            println!("    error: {error}");
        }
    }
    println!(
        "Storage: {} (failures: {}, last failure: {}, last recovery: {})",
        if status.storage.healthy {
            "healthy"
        } else {
            "failed"
        },
        status.storage.failure_count,
        display_time(status.storage.last_failure_ms),
        display_time(status.storage.last_recovery_ms)
    );
    println!("Drops:");
    if status.drops_by_reason.is_empty() {
        println!("  none");
    } else {
        for (reason, count) in &status.drops_by_reason {
            println!("  {reason}: {count}");
        }
    }
    println!("Active configuration:");
    println!(
        "{}",
        serde_json::to_string_pretty(&status.active_configuration).unwrap_or_default()
    );
}

fn display_time(value: Option<u64>) -> String {
    value
        .and_then(|ms| DateTime::<Utc>::from_timestamp_millis(ms as i64))
        .map(|time| time.to_rfc3339())
        .unwrap_or_else(|| "never".to_string())
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> StatusHandle {
        let config: Config = toml::from_str(
            "[device]\nid = \"test\"\ningest_key = \"secret\"\n[logs]\njournald = false\nsyslog = false\n",
        )
        .unwrap();
        StatusHandle::new(PathBuf::from("/tmp/config.toml"), &config)
    }

    #[test]
    fn queue_combines_local_logs_and_pending_metrics() {
        let status = handle();
        status.set_queue(QueueStatus {
            records: 2,
            bytes: 100,
            oldest_record_ms: Some(200),
        });
        status.set_metrics_queue(QueueStatus {
            records: 1,
            bytes: 20,
            oldest_record_ms: Some(100),
        });

        let snapshot = status.snapshot();
        assert_eq!(snapshot.queue.records, 3);
        assert_eq!(snapshot.queue.bytes, 120);
        assert_eq!(snapshot.queue.oldest_record_ms, Some(100));

        status.set_metrics_queue(QueueStatus::default());
        let snapshot = status.snapshot();
        assert_eq!(snapshot.queue.records, 2);
        assert_eq!(snapshot.queue.bytes, 100);
        assert_eq!(snapshot.queue.oldest_record_ms, Some(200));
    }

    #[test]
    fn connection_changes_do_not_imply_a_publish() {
        let status = handle();
        status.connected();
        assert!(status.snapshot().connection.last_publish_ms.is_none());

        status.publish_sent();
        let published_at = status.snapshot().connection.last_publish_ms;
        assert!(published_at.is_some());
        status.disconnected(&"network lost");
        status.connected();
        assert_eq!(status.snapshot().connection.last_publish_ms, published_at);
    }

    #[test]
    fn storage_recovery_preserves_failure_history() {
        let status = handle();
        status.storage_failure("spool_write", &"disk full");
        status.storage_success("spool_write");

        let snapshot = status.snapshot();
        assert!(snapshot.storage.healthy);
        assert_eq!(snapshot.storage.failure_count, 1);
        assert!(snapshot.storage.last_failure_ms.is_some());
        assert!(snapshot.storage.last_recovery_ms.is_some());
    }

    #[test]
    fn unrelated_storage_success_does_not_clear_active_failure() {
        let status = handle();
        status.storage_failure("spool_read", &"corrupt chunk");
        status.storage_failure("metrics_sequence", &"disk full");

        status.storage_success("metrics_sequence");
        let snapshot = status.snapshot();
        assert!(!snapshot.storage.healthy);
        assert_eq!(
            snapshot.storage.last_error.as_deref(),
            Some("corrupt chunk")
        );
        assert!(snapshot.storage.last_recovery_ms.is_none());

        status.storage_success("spool_read");
        let snapshot = status.snapshot();
        assert!(snapshot.storage.healthy);
        assert_eq!(snapshot.storage.failure_count, 2);
        assert!(snapshot.storage.last_recovery_ms.is_some());
    }
}
