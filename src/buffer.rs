//! Two-tier buffer: memory ring buffer → disk spool chunks.
//!
//! Write path:  log entries → memory buffer.
//!              When memory is full OR flush timer fires → serialize to a disk chunk.
//!
//! Read path:   memory buffer first (newest), then disk chunks newest-first.
//!              Disk chunks are deleted after successful publish.

use crate::config::BufferConfig;
use crate::log_entry::LogEntry;
use crate::status::{now_ms, QueueStatus, StatusHandle};
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Disk spool helpers
// ---------------------------------------------------------------------------

/// Returns all spool chunk paths sorted newest-first (highest sequence number first).
/// Only files with a purely numeric stem (e.g. `0000000001.cbor`) are included;
/// other files in the spool directory are ignored.
fn spool_files_newest_first(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| is_chunk_file(p))
        .collect();
    // Chunk files are named with a zero-padded sequence number → lexicographic
    // sort descending gives newest-first.
    paths.sort_unstable_by(|a, b| b.file_name().cmp(&a.file_name()));
    Ok(paths)
}

fn total_spool_bytes(dir: &Path) -> u64 {
    if !dir.exists() {
        return 0;
    }
    fs::read_dir(dir)
        .ok()
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| is_chunk_file(&e.path()))
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

fn next_chunk_path(dir: &Path) -> Result<PathBuf> {
    // Use max existing sequence number + 1 so deleting published chunks never
    // causes a collision with chunks still on disk.
    let max_seq = if dir.exists() {
        fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) != Some("cbor") {
                    return None;
                }
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<u64>().ok())
            })
            .max()
            .unwrap_or(0)
    } else {
        0
    };
    Ok(dir.join(format!("{:010}.cbor", max_seq + 1)))
}

fn drop_oldest_chunk(dir: &Path) -> Result<Option<u64>> {
    if !dir.exists() {
        return Ok(None);
    }
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| is_chunk_file(p))
        .collect();
    paths.sort_unstable_by(|a, b| a.file_name().cmp(&b.file_name()));
    if let Some(oldest) = paths.first() {
        warn!(
            "disk spool full, dropping oldest chunk: {}",
            oldest.display()
        );
        let records = Buffer::read_chunk(oldest)
            .map(|entries| entries.len() as u64)
            .unwrap_or(0);
        fs::remove_file(oldest)?;
        return Ok(Some(records));
    }
    Ok(None)
}

/// Returns true only for files that are spool chunks: `.cbor` extension and a
/// purely numeric stem (e.g. `0000000001.cbor`).
fn is_chunk_file(p: &Path) -> bool {
    p.extension().and_then(|s| s.to_str()) == Some("cbor")
        && p.file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.chars().all(|c| c.is_ascii_digit()))
            .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Buffer
// ---------------------------------------------------------------------------

pub struct Buffer {
    cfg: BufferConfig,
    spool_dir: PathBuf,
    memory: VecDeque<LogEntry>,
    /// Tracks whether disk chunks may exist, to avoid scanning the spool
    /// directory on every idle tick. Set when a chunk is written, cleared
    /// when `next_disk_chunk()` returns None.
    may_have_chunks: bool,
    memory_bytes: u64,
    disk_bytes: u64,
    disk_records: u64,
    disk_oldest_ms: Option<u64>,
    status: StatusHandle,
}

impl Buffer {
    pub fn new(cfg: BufferConfig, spool_dir: PathBuf, status: StatusHandle) -> Self {
        // Check on startup whether leftover chunks exist from a previous run.
        let may_have_chunks = spool_files_newest_first(&spool_dir)
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        let mut buffer = Self {
            cfg,
            spool_dir,
            memory: VecDeque::new(),
            may_have_chunks,
            memory_bytes: 0,
            disk_bytes: 0,
            disk_records: 0,
            disk_oldest_ms: None,
            status,
        };
        let _ = buffer.refresh_disk_stats();
        buffer.update_queue_status();
        buffer
    }

    /// Push a log entry into the memory buffer.
    /// When memory is full, flush the current contents to a disk chunk first.
    pub fn push(&mut self, mut entry: LogEntry) -> Result<()> {
        if self.memory.len() >= self.cfg.memory_max_entries {
            self.flush_memory_to_disk()?;
        }
        if entry.queued_at_ms == 0 {
            entry.queued_at_ms = now_ms();
        }
        self.memory_bytes += encoded_len(&entry);
        self.memory.push_back(entry);
        self.update_queue_status();
        Ok(())
    }

    /// Flush all current in-memory entries to disk, split into chunks of
    /// `disk_chunk_max_entries`. Called on memory overflow and graceful shutdown.
    pub fn flush_memory_to_disk(&mut self) -> Result<()> {
        if self.memory.is_empty() {
            return Ok(());
        }
        let entries: Vec<LogEntry> = self.memory.drain(..).collect();
        self.memory_bytes = 0;
        for (index, chunk) in entries.chunks(self.cfg.disk_chunk_max_entries).enumerate() {
            if let Err(error) = self.write_chunk(chunk) {
                let restore_from = index * self.cfg.disk_chunk_max_entries;
                for entry in &entries[restore_from..] {
                    self.memory_bytes += encoded_len(entry);
                    self.memory.push_back(entry.clone());
                }
                self.status.storage_failure("spool_write", &error);
                self.update_queue_status();
                return Err(error);
            }
        }
        self.status.storage_success("spool_write");
        self.update_queue_status();
        Ok(())
    }

    /// Write a slice of entries as a single CBOR chunk file on disk.
    /// Uses atomic write-then-rename so a crash mid-write never leaves a
    /// partially-written file that would be mistaken for a corrupt chunk.
    fn write_chunk(&mut self, entries: &[LogEntry]) -> Result<()> {
        let dir = &self.spool_dir;
        fs::create_dir_all(dir).with_context(|| format!("create spool dir {}", dir.display()))?;

        let path = next_chunk_path(dir)?;
        let mut buf = Vec::new();
        ciborium::into_writer(entries, &mut buf)
            .with_context(|| "CBOR serialization of chunk failed")?;
        let max_bytes = self.cfg.disk_max_size_mb * 1024 * 1024;
        if buf.len() as u64 > max_bytes {
            warn!(
                "encoded spool chunk is larger than the configured spool, dropping {} records",
                entries.len()
            );
            self.status
                .record_drop("disk_spool_chunk_too_large", entries.len() as u64);
            return Ok(());
        }

        while total_spool_bytes(dir) + buf.len() as u64 > max_bytes {
            let Some(dropped) = drop_oldest_chunk(dir)? else {
                break;
            };
            self.status.record_drop("disk_spool_full", dropped.max(1));
        }
        // B4: atomic write — write to .tmp then rename so a crash mid-write
        // never leaves a partially-written chunk file.
        let tmp = path.with_extension("cbor.tmp");
        fs::write(&tmp, &buf).with_context(|| format!("write chunk tmp {}", tmp.display()))?;
        fs::rename(&tmp, &path)
            .with_context(|| format!("rename chunk {} → {}", tmp.display(), path.display()))?;
        self.may_have_chunks = true;
        self.refresh_disk_stats()?;
        debug!(
            "flushed {} entries to disk chunk {}",
            entries.len(),
            path.display()
        );
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Read path (used by the MQTT publisher)
    // ---------------------------------------------------------------------------

    /// Drain all in-memory entries (newest batch — returned as a Vec so the
    /// caller can publish and, on success, simply discard).
    pub fn drain_memory(&mut self) -> Vec<LogEntry> {
        let entries = self.memory.drain(..).collect();
        self.memory_bytes = 0;
        self.update_queue_status();
        entries
    }

    /// Return the path to the newest disk chunk that has not yet been published,
    /// or `None` if the spool is empty.  Skips the directory scan entirely when
    /// no chunks are expected (reduces I/O on flash / SD cards).
    pub fn next_disk_chunk(&mut self) -> Result<Option<PathBuf>> {
        if !self.may_have_chunks {
            return Ok(None);
        }
        let files = match spool_files_newest_first(&self.spool_dir) {
            Ok(files) => {
                self.status.storage_success("spool_scan");
                files
            }
            Err(error) => {
                self.status.storage_failure("spool_scan", &error);
                return Err(error);
            }
        };
        let next = files.into_iter().next();
        if next.is_none() {
            self.may_have_chunks = false;
        }
        Ok(next)
    }

    /// Load and deserialize a chunk from disk.
    pub fn read_chunk(path: &Path) -> Result<Vec<LogEntry>> {
        let data = fs::read(path).with_context(|| format!("read chunk {}", path.display()))?;
        let entries: Vec<LogEntry> = ciborium::from_reader(data.as_slice())
            .with_context(|| format!("CBOR decode of chunk {}", path.display()))?;
        Ok(entries)
    }

    /// Delete a chunk after it has been successfully published.
    pub fn delete_chunk(&mut self, path: &Path) -> Result<()> {
        let operation = format!("spool_delete:{}", path.display());
        if let Err(error) =
            fs::remove_file(path).with_context(|| format!("delete chunk {}", path.display()))
        {
            self.status.storage_failure(&operation, &error);
            return Err(error);
        }
        self.status.storage_success(&operation);
        self.refresh_disk_stats()?;
        self.update_queue_status();
        debug!("deleted published chunk {}", path.display());
        Ok(())
    }

    pub fn memory_len(&self) -> usize {
        self.memory.len()
    }

    fn refresh_disk_stats(&mut self) -> Result<()> {
        let result = (|| {
            self.disk_bytes = 0;
            self.disk_records = 0;
            self.disk_oldest_ms = None;
            for path in spool_files_newest_first(&self.spool_dir)? {
                let metadata = fs::metadata(&path)?;
                self.disk_bytes += metadata.len();
                match Self::read_chunk(&path) {
                    Ok(entries) => {
                        self.disk_records += entries.len() as u64;
                        let fallback = metadata
                            .modified()
                            .ok()
                            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|duration| duration.as_millis() as u64);
                        for entry in entries {
                            let queued_at = if entry.queued_at_ms > 0 {
                                Some(entry.queued_at_ms)
                            } else {
                                entry.timestamp_ms.or(fallback)
                            };
                            self.disk_oldest_ms = oldest(self.disk_oldest_ms, queued_at);
                        }
                    }
                    Err(error) => warn!("cannot inspect spool chunk {}: {error}", path.display()),
                }
            }
            Ok(())
        })();
        match &result {
            Ok(()) => self.status.storage_success("spool_stats"),
            Err(error) => self.status.storage_failure("spool_stats", error),
        }
        result
    }

    fn update_queue_status(&self) {
        let memory_oldest = self
            .memory
            .iter()
            .map(|entry| entry.queued_at_ms)
            .filter(|value| *value > 0)
            .min();
        self.status.set_queue(QueueStatus {
            records: self.disk_records + self.memory.len() as u64,
            bytes: self.disk_bytes + self.memory_bytes,
            oldest_record_ms: oldest(self.disk_oldest_ms, memory_oldest),
        });
    }
}

fn oldest(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn encoded_len(entry: &LogEntry) -> u64 {
    let mut data = Vec::new();
    if ciborium::into_writer(entry, &mut data).is_ok() {
        data.len() as u64
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_entry::{LogEntry, Severity};
    use crate::status::StatusHandle;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn make_cfg(memory_max: usize, chunk_max: usize) -> BufferConfig {
        BufferConfig {
            memory_max_entries: memory_max,
            disk_max_size_mb: 64,
            disk_chunk_max_entries: chunk_max,
        }
    }

    fn entry(body: &str) -> LogEntry {
        LogEntry {
            body: body.to_string(),
            severity: None,
            timestamp_ms: None,
            uptime_ms: None,
            source: "test".to_string(),
            labels: HashMap::new(),
            queued_at_ms: 0,
        }
    }

    fn test_status(path: &Path) -> StatusHandle {
        let config: crate::config::Config = toml::from_str(
            "[device]\nid = \"test\"\ningest_key = \"test\"\n[logs]\njournald = false\nsyslog = false\n",
        )
        .unwrap();
        StatusHandle::new(path.join("config.toml"), &config)
    }

    #[test]
    fn log_spool_limits_and_cleanup_do_not_affect_other_state() {
        let temp = TempDir::new().unwrap();
        let storage = crate::config::StorageConfig {
            state_dir: temp.path().to_path_buf(),
        };
        fs::create_dir_all(storage.metrics_dir()).unwrap();
        fs::create_dir_all(storage.crashdump_dir()).unwrap();
        let sequences = storage.metrics_dir().join("metrics_seq.cbor");
        let crash_state = storage.crashdump_dir().join("crashdump_state.json");
        // Other persistent state can exceed the log spool's size limit.
        let metric_state = vec![1u8; 2 * 1024 * 1024];
        fs::write(&sequences, &metric_state).unwrap();
        fs::write(&crash_state, b"crash state").unwrap();
        let cfg = BufferConfig {
            disk_max_size_mb: 1,
            ..make_cfg(1, 1)
        };
        let status = test_status(temp.path());
        let mut buf = Buffer::new(cfg.clone(), storage.spool_dir(), status.clone());
        let large_entry = entry(&"x".repeat(700_000));
        for _ in 0..2 {
            buf.push(large_entry.clone()).unwrap();
            buf.flush_memory_to_disk().unwrap();
        }
        assert_eq!(status.snapshot().drops_by_reason["disk_spool_full"], 1);
        assert_eq!(status.snapshot().queue.records, 1);
        let chunk = buf.next_disk_chunk().unwrap().unwrap();
        assert_eq!(chunk.parent().unwrap(), storage.spool_dir());
        assert_eq!(
            status.snapshot().queue.bytes,
            fs::metadata(&chunk).unwrap().len()
        );
        buf.delete_chunk(&chunk).unwrap();
        assert!(buf.next_disk_chunk().unwrap().is_none());
        drop(buf);

        let mut restarted = Buffer::new(cfg, storage.spool_dir(), status.clone());
        assert!(restarted.next_disk_chunk().unwrap().is_none());
        assert_eq!(status.snapshot().queue.records, 0);
        assert_eq!(fs::read(sequences).unwrap(), metric_state);
        assert_eq!(fs::read(crash_state).unwrap(), b"crash state");
    }

    /// B1: sequence number must not reuse a number that was deleted.
    #[test]
    fn chunk_path_no_collision_after_delete() {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        fs::write(p.join("0000000001.cbor"), b"x").unwrap();
        fs::write(p.join("0000000002.cbor"), b"x").unwrap();
        fs::remove_file(p.join("0000000001.cbor")).unwrap();
        // Highest remaining = 2, so next must be 3.
        let next = next_chunk_path(p).unwrap();
        assert_eq!(next.file_name().unwrap(), "0000000003.cbor");
    }

    /// B2: flush must honour disk_chunk_max_entries.
    #[test]
    fn flush_splits_into_chunks() {
        let dir = TempDir::new().unwrap();
        // memory_max=100 so no auto-flush triggers; chunk_max=3
        let cfg = make_cfg(100, 3);
        let mut buf = Buffer::new(cfg, dir.path().to_path_buf(), test_status(dir.path()));
        for i in 0..7 {
            buf.memory.push_back(entry(&format!("msg{i}")));
        }
        buf.flush_memory_to_disk().unwrap();
        assert_eq!(buf.memory_len(), 0);

        let chunks = spool_files_newest_first(dir.path()).unwrap();
        // ceil(7/3) = 3 chunks
        assert_eq!(chunks.len(), 3);
        let c1 = Buffer::read_chunk(&dir.path().join("0000000001.cbor")).unwrap();
        assert_eq!(c1.len(), 3);
        let c2 = Buffer::read_chunk(&dir.path().join("0000000002.cbor")).unwrap();
        assert_eq!(c2.len(), 3);
        let c3 = Buffer::read_chunk(&dir.path().join("0000000003.cbor")).unwrap();
        assert_eq!(c3.len(), 1);
    }

    /// Memory overflow triggers a flush and new entries continue to accumulate.
    #[test]
    fn memory_overflow_flushes_to_disk() {
        let dir = TempDir::new().unwrap();
        let cfg = make_cfg(3, 10);
        let mut buf = Buffer::new(cfg, dir.path().to_path_buf(), test_status(dir.path()));
        buf.push(entry("a")).unwrap();
        buf.push(entry("b")).unwrap();
        buf.push(entry("c")).unwrap();
        // len == memory_max == 3; next push triggers flush
        buf.push(entry("d")).unwrap();

        assert_eq!(buf.memory_len(), 1); // only "d" in memory
        let chunk = buf.next_disk_chunk().unwrap().unwrap();
        let entries = Buffer::read_chunk(&chunk).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].body, "a");
        assert_eq!(entries[2].body, "c");
    }

    /// Drain memory returns all entries and leaves memory empty.
    #[test]
    fn drain_memory_empties_buffer() {
        let dir = TempDir::new().unwrap();
        let cfg = make_cfg(100, 10);
        let mut buf = Buffer::new(cfg, dir.path().to_path_buf(), test_status(dir.path()));
        buf.push(entry("x")).unwrap();
        buf.push(entry("y")).unwrap();
        let drained = buf.drain_memory();
        assert_eq!(drained.len(), 2);
        assert_eq!(buf.memory_len(), 0);
    }

    /// Severity is correctly round-tripped through the disk spool.
    #[test]
    fn chunk_round_trips_severity() {
        let dir = TempDir::new().unwrap();
        let cfg = make_cfg(100, 10);
        let mut buf = Buffer::new(cfg, dir.path().to_path_buf(), test_status(dir.path()));
        let mut e = entry("hello");
        e.severity = Some(Severity::Error);
        buf.push(e).unwrap();
        buf.flush_memory_to_disk().unwrap();
        let chunk = buf.next_disk_chunk().unwrap().unwrap();
        let entries = Buffer::read_chunk(&chunk).unwrap();
        assert_eq!(entries[0].severity, Some(Severity::Error));
    }
}
