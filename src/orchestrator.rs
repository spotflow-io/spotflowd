//! Orchestrator — wires sources → buffer → MQTT publisher.

use crate::buffer::Buffer;
use crate::config::Config;
use crate::log_entry::LogEntry;
use crate::mqtt::MqttPublisher;
use crate::status::StatusHandle;
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

const PUBLISH_BATCH_SIZE: usize = 200;
const IDLE_SLEEP_MS: u64 = 200;

pub async fn run(
    cfg: Config,
    publisher: MqttPublisher,
    shutdown: tokio::sync::watch::Receiver<bool>,
    status: StatusHandle,
) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<LogEntry>(4096);

    // Shared flag passed into spawn_blocking threads so they exit immediately
    // on shutdown, before the Tokio runtime tries to wait for them.
    let shutdown_threads = Arc::new(AtomicBool::new(false));

    // -----------------------------------------------------------------------
    // Start sources
    // -----------------------------------------------------------------------

    #[cfg(feature = "journald")]
    if cfg.logs.journald {
        let tx2 = tx.clone();
        let flag = shutdown_threads.clone();
        let scope = cfg.logs.journald_scope;
        let startup_max_entries = cfg.logs.startup_max_entries;
        let source_status = status.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::sources::journald::run(
                tx2,
                flag,
                scope,
                startup_max_entries,
                source_status.clone(),
            )
            .await
            {
                source_status.source_error("journald", &e);
                error!("journald source failed: {e}");
            }
        });
    }

    if cfg.logs.syslog {
        let tx2 = tx.clone();
        let path = cfg.logs.syslog_path.clone();
        let flag = shutdown_threads.clone();
        let startup_max_entries = cfg.logs.startup_max_entries;
        let source_status = status.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::sources::syslog::run(
                path,
                tx2,
                flag,
                startup_max_entries,
                source_status.clone(),
            )
            .await
            {
                source_status.source_error("syslog", &e);
                error!("syslog source failed: {e}");
            }
        });
    }

    if cfg.metrics.os.enabled || cfg.metrics.custom.enabled {
        let metrics_cfg = cfg.metrics.clone();
        let seq_dir = cfg.storage.metrics_dir();
        let publisher_clone = publisher.clone();
        let shutdown_rx = shutdown.clone();
        let metrics_status = status.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::metrics::run(
                metrics_cfg,
                seq_dir,
                publisher_clone,
                shutdown_rx,
                metrics_status.clone(),
            )
            .await
            {
                metrics_status.source_error("os_metrics", &e);
                error!("metrics task failed: {e}");
            }
        });
    }

    if cfg.crashdump.enabled {
        let cd_cfg = cfg.crashdump.clone();
        let state_dir = cfg.storage.crashdump_dir();
        let publisher_clone = publisher.clone();
        let shutdown_rx = shutdown.clone();
        let crashdump_status = status.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::sources::crashdump::run(
                cd_cfg,
                state_dir,
                publisher_clone,
                shutdown_rx,
                crashdump_status.clone(),
            )
            .await
            {
                crashdump_status.source_error("crashdump", &e);
                error!("crashdump task failed: {e}");
            }
        });
    }

    drop(tx);

    // -----------------------------------------------------------------------
    // Buffer
    // -----------------------------------------------------------------------
    let buffer = Arc::new(Mutex::new(Buffer::new(
        cfg.logs.buffer.clone(),
        cfg.storage.spool_dir(),
        status.clone(),
    )));

    // -----------------------------------------------------------------------
    // Ingestion task
    // -----------------------------------------------------------------------
    {
        let buffer = buffer.clone();
        let mut shutdown_rx = shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    entry = rx.recv() => {
                        match entry {
                            Some(e) => {
                                let mut buf = buffer.lock().await;
                                if let Err(err) = buf.push(e) {
                                    error!("buffer push error: {err}");
                                }
                            }
                            None => break,
                        }
                    }
                    _ = shutdown_rx.changed() => break,
                }
            }
        });
    }

    // -----------------------------------------------------------------------
    // Publish loop
    // -----------------------------------------------------------------------
    let mut shutdown_rx = shutdown.clone();

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                info!("shutdown signal received, flushing buffer to disk...");
                // Tell blocking source threads to exit before the runtime drops.
                shutdown_threads.store(true, Ordering::Relaxed);
                let mut buf = buffer.lock().await;
                if let Err(e) = buf.flush_memory_to_disk() {
                    warn!("flush on shutdown failed: {e}");
                }
                info!("buffer flushed, exiting");
                break;
            }
            _ = publish_tick(&publisher, &buffer, &status) => {}
        }
    }

    Ok(())
}

async fn publish_tick(
    publisher: &MqttPublisher,
    buffer: &Arc<Mutex<Buffer>>,
    status: &StatusHandle,
) {
    if !publisher.is_connected() {
        sleep(Duration::from_millis(IDLE_SLEEP_MS)).await;
        return;
    }

    // Memory first (newest data).
    let memory_entries = {
        let mut buf = buffer.lock().await;
        if buf.memory_len() > 0 {
            buf.drain_memory()
        } else {
            vec![]
        }
    };

    if !memory_entries.is_empty() {
        for chunk in memory_entries.chunks(PUBLISH_BATCH_SIZE) {
            if let Err(e) = publisher.publish_batch(chunk).await {
                warn!("publish error (memory batch): {e}");
                let mut buf = buffer.lock().await;
                for entry in chunk {
                    let _ = buf.push(entry.clone());
                }
                sleep(Duration::from_millis(IDLE_SLEEP_MS)).await;
                return;
            }
        }
        return;
    }

    // Disk next (older data, newest chunk first).
    let next_chunk = {
        let mut buf = buffer.lock().await;
        match buf.next_disk_chunk() {
            Ok(p) => p,
            Err(e) => {
                warn!("error reading spool directory: {e}");
                None
            }
        }
    };

    if let Some(chunk_path) = next_chunk {
        let read_operation = format!("spool_read:{}", chunk_path.display());
        match Buffer::read_chunk(&chunk_path) {
            Ok(entries) => {
                status.storage_success(&read_operation);
                let mut published = 0usize;
                for chunk in entries.chunks(PUBLISH_BATCH_SIZE) {
                    if let Err(e) = publisher.publish_batch(chunk).await {
                        warn!("publish error (disk chunk): {e}");
                        break;
                    }
                    published += chunk.len();
                }
                if published == entries.len() {
                    // All entries published — delete the chunk.
                    let mut buf = buffer.lock().await;
                    if let Err(e) = buf.delete_chunk(&chunk_path) {
                        warn!("failed to delete published chunk: {e}");
                    }
                } else if published > 0 {
                    // B1: partial publish — rewrite chunk with only the
                    // unpublished tail so already-sent entries aren't resent.
                    let remaining = &entries[published..];
                    let mut buf_lock = buffer.lock().await;
                    for entry in remaining.iter().rev() {
                        let _ = buf_lock.push(entry.clone());
                    }
                    if let Err(e) = buf_lock.delete_chunk(&chunk_path) {
                        warn!("failed to delete partially-published chunk: {e}");
                    }
                }
                // published == 0: leave chunk on disk for retry next tick.
            }
            Err(e) => {
                warn!(
                    "failed to read chunk {}: {e} — deleting corrupt chunk",
                    chunk_path.display()
                );
                status.storage_failure(&read_operation, &e);
                status.record_drop("corrupt_spool_chunk", 1);
                let mut buf = buffer.lock().await;
                if buf.delete_chunk(&chunk_path).is_ok() {
                    status.storage_success(&read_operation);
                }
            }
        }
        return;
    }

    sleep(Duration::from_millis(IDLE_SLEEP_MS)).await;
}
