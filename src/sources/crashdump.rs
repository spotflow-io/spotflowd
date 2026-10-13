//! Kernel crash dump collection via pstore.
//!
//! After a kernel panic or oops the kernel writes the dmesg backtrace to
//! persistent storage (ramoops-backed reserved RAM, EFI variables, or ACPI
//! ERST). It survives the reboot and reappears as files under `/sys/fs/pstore`
//! (e.g. `dmesg-ramoops-0`, `console-ramoops-0`). This module scans those
//! records and uploads each as a Spotflow `CORE_DUMP_CHUNK` stream — the same
//! ingest path used for MCU coredumps.
//!
//! Because a panic implies a reboot, the startup scan is the important one; a
//! periodic rescan covers late-mounted pstore and repeat crashes.
//!
//! Unlike log entries, crash dumps are not buffered through the log spool.
//! pstore itself is the durable on-device buffer while offline: a record is
//! deleted after all its chunks are queued for best-effort QoS 0 publishing.
//! There is no broker delivery acknowledgement. A small state file records
//! queued `coreDumpId`s in `<storage.state_dir>/crashdump/crashdump_state.json`
//! so a failed delete never causes a re-send, and the
//! deterministic id (derived from the record identity) makes retries idempotent
//! on the platform.
//!
//! CBOR key assignments (Spotflow crash-report spec):
//!   0  = messageType (always 2 = CORE_DUMP_CHUNK)
//!   9  = coreDumpId
//!   10 = chunkOrdinal (zero-based)
//!   11 = content (raw chunk bytes, bstr)
//!   12 = isLastChunk (only sent on the final chunk)
//!   14 = buildId (kernel release, best effort)
//!   15 = os (always "Linux")

use crate::config::CrashdumpConfig;
use crate::mqtt::MqttPublisher;
use crate::status::StatusHandle;
use anyhow::{Context, Result};
use ciborium::value::Value as CborValue;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tokio::time::{sleep, Duration};
use tracing::{debug, info, warn};

const MESSAGE_TYPE_CORE_DUMP: u64 = 2;
const OS_LINUX: &str = "Linux";

/// coreDumpId validity on the platform. Delivered ids older than this are
/// pruned from the state file to keep it bounded.
const STATE_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;

const STATE_FILE_NAME: &str = "crashdump_state.json";

/// Backoff between scans when a crash dump is waiting but couldn't be sent yet
/// (MQTT not connected, or an upload failed). Kept short so a dump captured at
/// boot goes out seconds after the link comes up rather than after a full
/// `poll_interval_secs` — the startup capture is the whole point of the feature.
const RETRY_BACKOFF_SECS: u64 = 5;

/// Whether the next scan should follow the short backoff or the full poll.
enum ScanOutcome {
    /// Nothing pending — wait the full `poll_interval_secs`.
    Idle,
    /// A dump is waiting to be sent (offline) or an upload failed — retry soon.
    RetrySoon,
}

pub async fn run(
    cfg: CrashdumpConfig,
    state_dir: PathBuf,
    publisher: MqttPublisher,
    mut shutdown: watch::Receiver<bool>,
    status: StatusHandle,
) -> Result<()> {
    let state_path = state_dir.join(STATE_FILE_NAME);
    let mut state = State::load(&state_path);
    state.prune(now_secs());

    let build_id = read_kernel_build_id();
    if let Some(ref b) = build_id {
        debug!("crashdump: kernel buildId = {b}");
    }
    status.source_ready("crashdump");

    // Scan immediately (startup capture), then sleep between scans. The delay
    // shrinks to a short backoff whenever a dump is waiting to be delivered.
    loop {
        let outcome = match scan_and_publish(
            &cfg,
            &publisher,
            build_id.as_deref(),
            &mut state,
            &state_path,
        )
        .await
        {
            Ok(o) => {
                status.source_collected("crashdump");
                o
            }
            Err(e) => {
                warn!("crashdump scan failed: {e}");
                status.source_error("crashdump", &e);
                ScanOutcome::Idle
            }
        };

        let delay = match outcome {
            ScanOutcome::RetrySoon => RETRY_BACKOFF_SECS.min(cfg.poll_interval_secs),
            ScanOutcome::Idle => cfg.poll_interval_secs,
        };

        tokio::select! {
            _ = sleep(Duration::from_secs(delay)) => {}
            _ = shutdown.changed() => break,
        }
    }
    Ok(())
}

async fn scan_and_publish(
    cfg: &CrashdumpConfig,
    publisher: &MqttPublisher,
    build_id: Option<&str>,
    state: &mut State,
    state_path: &Path,
) -> Result<ScanOutcome> {
    let connected = publisher.is_connected();
    // Count of undelivered dumps we couldn't send this pass (offline).
    let mut deferred = 0usize;

    for dir in &cfg.paths {
        // A missing directory just means pstore isn't mounted here — not an error.
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let fname = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if !matches_kind(&fname, &cfg.kinds) {
                continue;
            }

            let meta = match entry.metadata() {
                Ok(m) if m.is_file() => m,
                _ => continue,
            };
            let mtime = mtime_secs(&meta);
            let id = core_dump_id(&fname, mtime);

            // Already delivered — just make sure the record is cleared.
            if state.contains(id) {
                maybe_delete(cfg, &path, &fname);
                continue;
            }

            // An undelivered dump exists but the link is down — remember it and
            // retry on the short backoff instead of the full poll interval.
            if !connected {
                deferred += 1;
                continue;
            }

            let content = match read_capped(&path, cfg.max_bytes) {
                Ok(c) => c,
                Err(e) => {
                    warn!("crashdump: failed to read {fname}: {e}");
                    continue;
                }
            };
            if content.is_empty() {
                continue;
            }

            match publish_core_dump(publisher, id, &content, cfg.chunk_bytes, build_id).await {
                Ok(chunks) => {
                    info!(
                        "crashdump: queued pstore record {fname} for MQTT ({} bytes, {chunks} chunk(s), coreDumpId={id})",
                        content.len()
                    );
                    // Persist the queued dump before deleting so a crash between the two
                    // never turns into a re-send.
                    state.insert(id, now_secs());
                    if let Err(e) = state.save(state_path) {
                        warn!("crashdump: failed to persist state: {e}");
                    }
                    maybe_delete(cfg, &path, &fname);
                }
                Err(e) => {
                    // Leave the record in place; pstore is the buffer, so we
                    // retry soon rather than waiting a full poll interval.
                    warn!("crashdump: upload failed for {fname}: {e} — will retry");
                    return Ok(ScanOutcome::RetrySoon);
                }
            }
        }
    }

    if deferred > 0 {
        debug!("crashdump: {deferred} dump(s) waiting, MQTT not connected — will retry");
        Ok(ScanOutcome::RetrySoon)
    } else {
        Ok(ScanOutcome::Idle)
    }
}

/// Publish a dump as one or more CORE_DUMP_CHUNK messages. Returns the chunk count.
async fn publish_core_dump(
    publisher: &MqttPublisher,
    id: u64,
    content: &[u8],
    chunk_bytes: usize,
    build_id: Option<&str>,
) -> Result<usize> {
    let chunks: Vec<&[u8]> = content.chunks(chunk_bytes.max(1)).collect();
    let n = chunks.len();
    for (i, chunk) in chunks.iter().enumerate() {
        let payload = encode_chunk(id, i as u64, chunk, i == n - 1, build_id)?;
        publisher.publish_payload(payload).await?;
    }
    Ok(n)
}

fn encode_chunk(
    id: u64,
    ordinal: u64,
    content: &[u8],
    is_last: bool,
    build_id: Option<&str>,
) -> Result<Vec<u8>> {
    let mut map = vec![
        (
            CborValue::Integer(0u64.into()),
            CborValue::Integer(MESSAGE_TYPE_CORE_DUMP.into()),
        ),
        (
            CborValue::Integer(9u64.into()),
            CborValue::Integer(id.into()),
        ),
        (
            CborValue::Integer(10u64.into()),
            CborValue::Integer(ordinal.into()),
        ),
        (
            CborValue::Integer(11u64.into()),
            CborValue::Bytes(content.to_vec()),
        ),
    ];
    if is_last {
        map.push((CborValue::Integer(12u64.into()), CborValue::Bool(true)));
    }
    if let Some(b) = build_id {
        map.push((
            CborValue::Integer(14u64.into()),
            CborValue::Text(b.to_string()),
        ));
    }
    map.push((
        CborValue::Integer(15u64.into()),
        CborValue::Text(OS_LINUX.to_string()),
    ));

    let mut buf = Vec::new();
    ciborium::into_writer(&CborValue::Map(map), &mut buf)
        .context("CBOR encode core dump chunk failed")?;
    Ok(buf)
}

fn maybe_delete(cfg: &CrashdumpConfig, path: &Path, fname: &str) {
    if cfg.delete_after_capture {
        if let Err(e) = std::fs::remove_file(path) {
            warn!("crashdump: failed to delete pstore record {fname}: {e}");
        }
    }
}

/// True when the filename starts with one of the configured kind prefixes,
/// e.g. kind "dmesg" matches "dmesg-ramoops-0" and "dmesg-efi-...".
fn matches_kind(fname: &str, kinds: &[String]) -> bool {
    kinds.iter().any(|k| fname.starts_with(k.as_str()))
}

fn read_capped(path: &Path, max_bytes: usize) -> Result<Vec<u8>> {
    let mut data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    if data.len() > max_bytes {
        data.truncate(max_bytes);
    }
    Ok(data)
}

/// Kernel release string (`uname -r`), used as a best-effort buildId to link
/// the dump to matching symbols. A GNU build-id from /sys/kernel/notes would be
/// more precise and is a possible future refinement.
fn read_kernel_build_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Deterministic coreDumpId from a record's stable identity (name + mtime).
/// Same record → same id, so retries after a failed upload are idempotent on
/// the platform. FNV-1a keeps it dependency-free; the top bit is masked off so
/// the value is always a positive uint.
fn core_dump_id(fname: &str, mtime: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |bytes: &[u8]| {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    mix(fname.as_bytes());
    mix(&mtime.to_le_bytes());
    h & 0x7fff_ffff_ffff_ffff
}

// ---------------------------------------------------------------------------
// Delivered-record state (dedup across restarts)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    delivered: Vec<DeliveredRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DeliveredRecord {
    id: u64,
    /// Epoch seconds when all dump chunks were accepted into the MQTT queue.
    ts: u64,
}

impl State {
    fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                warn!(
                    "crashdump: corrupt state file {}: {e} — starting fresh",
                    path.display()
                );
                State::default()
            }),
            Err(_) => State::default(),
        }
    }

    fn contains(&self, id: u64) -> bool {
        self.delivered.iter().any(|r| r.id == id)
    }

    fn insert(&mut self, id: u64, ts: u64) {
        if !self.contains(id) {
            self.delivered.push(DeliveredRecord { id, ts });
        }
    }

    fn prune(&mut self, now: u64) {
        let cutoff = now.saturating_sub(STATE_RETENTION_SECS);
        self.delivered.retain(|r| r.ts >= cutoff);
    }

    /// Atomic write-then-rename so a crash mid-write can't corrupt the file.
    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create state dir {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(self).context("serialize crashdump state")?;
        std::fs::write(&tmp, &bytes).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciborium::value::Value as V;

    // --- core_dump_id ---

    #[test]
    fn core_dump_id_is_deterministic() {
        // Same record identity → same id (so retries are idempotent).
        assert_eq!(
            core_dump_id("dmesg-ramoops-0", 1_700_000_000),
            core_dump_id("dmesg-ramoops-0", 1_700_000_000)
        );
    }

    #[test]
    fn core_dump_id_varies_by_name_and_mtime() {
        let base = core_dump_id("dmesg-ramoops-0", 1_700_000_000);
        assert_ne!(base, core_dump_id("dmesg-ramoops-1", 1_700_000_000));
        assert_ne!(base, core_dump_id("dmesg-ramoops-0", 1_700_000_001));
    }

    #[test]
    fn core_dump_id_is_always_positive_uint() {
        // Top bit masked off — safe as a signed or unsigned integer on the wire.
        assert_eq!(core_dump_id("console-efi-0", u64::MAX) >> 63, 0);
    }

    // --- matches_kind ---

    #[test]
    fn matches_kind_by_prefix() {
        let kinds = vec!["dmesg".to_string(), "console".to_string()];
        assert!(matches_kind("dmesg-ramoops-0", &kinds));
        assert!(matches_kind("dmesg-efi-140000000000000", &kinds));
        assert!(matches_kind("console-ramoops-0", &kinds));
        assert!(!matches_kind("pmsg-ramoops-0", &kinds));
        assert!(!matches_kind("ftrace-ramoops-0", &kinds));
    }

    // --- encode_chunk ---

    fn get(map: &[(V, V)], key: u64) -> Option<&V> {
        map.iter().find_map(|(k, v)| match k {
            V::Integer(i) if u64::try_from(*i).ok() == Some(key) => Some(v),
            _ => None,
        })
    }

    #[test]
    fn encode_chunk_has_required_keys() {
        let payload = encode_chunk(42, 0, b"panic backtrace", false, Some("6.1.0")).unwrap();
        let val: V = ciborium::from_reader(&payload[..]).unwrap();
        let map = match &val {
            V::Map(m) => m,
            _ => panic!("expected map"),
        };
        // messageType = 2 (CORE_DUMP_CHUNK)
        assert_eq!(get(map, 0), Some(&V::Integer(2.into())));
        // coreDumpId, chunkOrdinal
        assert_eq!(get(map, 9), Some(&V::Integer(42.into())));
        assert_eq!(get(map, 10), Some(&V::Integer(0.into())));
        // content is raw bytes (bstr), not text
        assert_eq!(get(map, 11), Some(&V::Bytes(b"panic backtrace".to_vec())));
        // os = "Linux"
        assert_eq!(get(map, 15), Some(&V::Text("Linux".to_string())));
        // buildId present
        assert_eq!(get(map, 14), Some(&V::Text("6.1.0".to_string())));
        // isLastChunk omitted on non-final chunk
        assert_eq!(get(map, 12), None);
    }

    #[test]
    fn encode_chunk_last_sets_is_last_and_omits_buildid_when_none() {
        let payload = encode_chunk(1, 3, b"x", true, None).unwrap();
        let val: V = ciborium::from_reader(&payload[..]).unwrap();
        let map = match &val {
            V::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert_eq!(get(map, 12), Some(&V::Bool(true)));
        assert_eq!(get(map, 14), None);
    }

    // --- State ---

    #[test]
    fn state_dedup_and_prune() {
        let mut s = State::default();
        s.insert(100, 1000);
        s.insert(100, 2000); // duplicate id ignored
        s.insert(200, 1000);
        assert!(s.contains(100));
        assert!(s.contains(200));
        assert_eq!(s.delivered.len(), 2);

        // prune drops anything older than the retention window.
        let now = 1000 + STATE_RETENTION_SECS + 1;
        s.prune(now);
        assert!(s.delivered.is_empty());
    }

    #[test]
    fn state_roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::config::StorageConfig {
            state_dir: dir.path().to_path_buf(),
        };
        let path = storage.crashdump_dir().join(STATE_FILE_NAME);
        let mut s = State::default();
        s.insert(7, 12345);
        s.save(&path).unwrap();

        let loaded = State::load(&path);
        assert!(loaded.contains(7));
        assert!(!storage.spool_dir().exists());
        assert!(!storage.metrics_dir().exists());
    }

    #[test]
    fn state_load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = State::load(&dir.path().join("does-not-exist.json"));
        assert!(loaded.delivered.is_empty());
    }
}
