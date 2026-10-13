//! journald log source.
//!
//! Replays a bounded tail of the current boot in the configured journal scope,
//! then follows new entries and sends them to the shared channel.
//! Uses `spawn_blocking` because the systemd journal API is synchronous.

use super::strip_ansi;
use crate::config::JournaldScope;
use crate::log_entry::{LabelValue, LogEntry, Severity};
use crate::status::StatusHandle;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub async fn run(
    tx: mpsc::Sender<LogEntry>,
    shutdown: Arc<AtomicBool>,
    scope: JournaldScope,
    startup_max_entries: usize,
    status: StatusHandle,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        run_blocking(tx, shutdown, scope, startup_max_entries, status)
    })
    .await??;
    Ok(())
}

fn run_blocking(
    tx: mpsc::Sender<LogEntry>,
    shutdown: Arc<AtomicBool>,
    scope: JournaldScope,
    startup_max_entries: usize,
    status: StatusHandle,
) -> Result<()> {
    use systemd::journal::OpenOptions;

    let mut options = OpenOptions::default();
    match scope {
        JournaldScope::System => {
            options.system(true);
        }
        JournaldScope::All => {
            options.local_only(true);
        }
    }

    let mut journal = options
        .open()
        .map_err(|e| anyhow::anyhow!("failed to open journald: {e}"))?;

    let boot_id = systemd::id128::Id128::from_boot().context("read current boot ID")?;
    position_for_startup(&mut journal, &boot_id.to_string(), startup_max_entries)?;
    status.source_ready("journald");

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        match journal.next_entry() {
            Ok(Some(entry)) => {
                if let Some(mut log_entry) = entry_to_log(&entry) {
                    // Timestamps are journal metadata, not enumerated entry fields.
                    // Preserve event time when replayed logs are uploaded later.
                    log_entry.timestamp_ms = journal.timestamp_usec().ok().map(|us| us / 1000);
                    log_entry.uptime_ms =
                        journal.monotonic_timestamp().ok().map(|(us, _)| us / 1000);
                    debug!("journald entry: {:?}", log_entry.body);
                    if tx.blocking_send(log_entry).is_err() {
                        break;
                    }
                    status.source_collected("journald");
                }
            }
            Ok(None) => {
                let _ = journal.wait(Some(Duration::from_millis(200)));
            }
            Err(e) => {
                warn!("journald read error: {e}");
                status.source_error("journald", &e);
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }

    Ok(())
}

/// Filter to this boot and position before its most recent bounded records.
fn position_for_startup(
    journal: &mut systemd::journal::JournalRef,
    boot_id: &str,
    startup_max_entries: usize,
) -> Result<()> {
    journal
        .match_add("_BOOT_ID", boot_id)
        .context("filter journal to current boot")?;
    journal
        .seek_tail()
        .map_err(|e| anyhow::anyhow!("journal seek failed: {e}"))?;
    // Leave the cursor one record before the replay window: next_entry() moves
    // forward before reading. With zero replay this anchors at the existing tail.
    let skipped = journal
        .previous_skip(startup_max_entries as u64 + 1)
        .map_err(|e| anyhow::anyhow!("journal tail positioning failed: {e}"))?;
    if skipped <= startup_max_entries {
        // At or before the first record, seek explicitly before it so the oldest
        // available boot record is included, including when the journal is empty.
        journal.seek_head().context("position at journal head")?;
    }
    Ok(())
}

fn entry_to_log(entry: &std::collections::BTreeMap<String, String>) -> Option<LogEntry> {
    // Collecting our own diagnostics would create a feedback loop.
    if entry.get("_SYSTEMD_UNIT").map(String::as_str) == Some("spotflowd.service") {
        return None;
    }
    let body = strip_ansi(entry.get("MESSAGE")?);
    if body.is_empty() {
        return None;
    }

    let severity = entry
        .get("PRIORITY")
        .and_then(|p| p.parse::<u8>().ok())
        .map(Severity::from_syslog_priority);

    let mut labels: HashMap<String, LabelValue> = HashMap::new();
    labels.insert("source".into(), LabelValue::Str("journald".into()));

    if let Some(h) = entry.get("_HOSTNAME") {
        labels.insert("hostname".into(), LabelValue::Str(h.clone()));
    }
    // Prefer SYSLOG_IDENTIFIER (e.g. "kernel"), fall back to _COMM (executable name).
    let process = entry
        .get("SYSLOG_IDENTIFIER")
        .or_else(|| entry.get("_COMM"))
        .cloned();
    if let Some(p) = process {
        labels.insert("process".into(), LabelValue::Str(p));
    }
    if let Some(pid) = entry.get("_PID").and_then(|v| v.parse::<i64>().ok()) {
        labels.insert("pid".into(), LabelValue::Int(pid));
    }
    if let Some(unit) = entry.get("_SYSTEMD_UNIT") {
        labels.insert("unit".into(), LabelValue::Str(unit.clone()));
    }

    Some(LogEntry {
        body,
        severity,
        timestamp_ms: None,
        uptime_ms: None,
        source: "journald".to_string(),
        labels,
        queued_at_ms: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use systemd::journal::{OpenDirectoryOptions, OpenFilesOptions};

    const BOOT_ID: &str = "00000000000000000000000000000001";
    const PREVIOUS_BOOT_ID: &str = "00000000000000000000000000000002";

    #[test]
    fn empty_journal_can_replay_and_follow() {
        let directory = tempfile::tempdir().unwrap();
        for limit in [0, 1000] {
            let mut journal = OpenDirectoryOptions::default()
                .open_directory(directory.path().to_str().unwrap())
                .unwrap();
            position_for_startup(&mut journal, BOOT_ID, limit).unwrap();
            assert!(journal.next_entry().unwrap().is_none());
        }
    }

    fn append_records(path: &Path, records: &[(&str, &str, u64)]) {
        let remote = std::env::var("SPOTFLOWD_TEST_JOURNAL_REMOTE")
            .unwrap_or_else(|_| "/usr/lib/systemd/systemd-journal-remote".to_string());
        let mut child = Command::new(remote)
            .arg("--split-mode=none")
            .arg("--compress=no")
            .arg(format!("--output={}", path.display()))
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("install systemd-journal-remote or set SPOTFLOWD_TEST_JOURNAL_REMOTE");
        let mut input = child.stdin.take().unwrap();
        for (boot_id, message, ordinal) in records {
            writeln!(
                input,
                "__REALTIME_TIMESTAMP={}\n__MONOTONIC_TIMESTAMP={}\n_BOOT_ID={boot_id}\nMESSAGE={message}\n",
                1_700_000_000_000_000 + ordinal * 1000,
                ordinal * 1000,
            )
            .unwrap();
        }
        drop(input);
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "journal import failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "requires systemd-journal-remote; run in CI with --include-ignored"]
    fn startup_replay_filters_boot_caps_records_and_follows_appends() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replay.journal");
        // A previous boot record deliberately sorts among current-boot records:
        // the boot-ID filter must exclude it independently of wall-clock time.
        append_records(
            &path,
            &[
                (BOOT_ID, "first boot log", 1),
                (PREVIOUS_BOOT_ID, "previous boot", 2),
                (BOOT_ID, "recovery log", 3),
                (BOOT_ID, "last boot log", 4),
            ],
        );
        let current_boot = ["first boot log", "recovery log", "last boot log"];
        for limit in [0, 1, 2, 3, 4, 1000] {
            let mut journal = OpenFilesOptions::default()
                .open_files([path.to_str().unwrap()])
                .unwrap();
            position_for_startup(&mut journal, BOOT_ID, limit).unwrap();
            let mut replay = Vec::new();
            while let Some(entry) = journal.next_entry().unwrap() {
                replay.push(entry["MESSAGE"].clone());
                assert_eq!(entry["_BOOT_ID"], BOOT_ID);
                assert!(journal.timestamp_usec().unwrap() >= 1_700_000_000_001_000);
                assert!(journal.monotonic_timestamp().unwrap().0 > 0);
            }
            assert_eq!(replay, current_boot[3 - limit.min(3)..]);
        }

        let mut journal = OpenFilesOptions::default()
            .open_files([path.to_str().unwrap()])
            .unwrap();
        position_for_startup(&mut journal, BOOT_ID, 2).unwrap();
        assert_eq!(
            journal.next_entry().unwrap().unwrap()["MESSAGE"],
            "recovery log"
        );
        assert_eq!(
            journal.next_entry().unwrap().unwrap()["MESSAGE"],
            "last boot log"
        );
        assert!(journal.next_entry().unwrap().is_none());
        append_records(&path, &[(BOOT_ID, "live log", 5)]);
        journal.wait(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(
            journal.next_entry().unwrap().unwrap()["MESSAGE"],
            "live log"
        );
        assert!(journal.next_entry().unwrap().is_none());
    }
}
