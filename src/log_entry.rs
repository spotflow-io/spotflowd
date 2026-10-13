use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Severity levels matching the Spotflow MQTT CBOR encoding (key 4).
/// Integer values align with the Spotflow platform spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum Severity {
    Debug = 30,
    Info = 40,
    Warning = 50,
    Error = 60,
    Critical = 70,
}

impl Severity {
    pub fn from_syslog_priority(priority: u8) -> Self {
        // Syslog severity (RFC 3164): 0=Emergency, 1=Alert, 2=Critical, 3=Error,
        // 4=Warning, 5=Notice, 6=Info, 7=Debug
        match priority & 0x07 {
            0..=2 => Severity::Critical,
            3 => Severity::Error,
            4 => Severity::Warning,
            5 | 6 => Severity::Info,
            _ => Severity::Debug,
        }
    }
}

/// A label value (per Spotflow MQTT spec, key 5 values).
/// Float and Bool are part of the spec but no current source produces them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LabelValue {
    Str(String),
    Int(i64),
}

/// A single log entry as it flows through the daemon pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// Human-readable log message.
    pub body: String,
    /// Log severity level. None when the source format does not carry priority.
    pub severity: Option<Severity>,
    /// UNIX epoch milliseconds (device wall clock, if available).
    pub timestamp_ms: Option<u64>,
    /// Milliseconds since device boot (if available).
    pub uptime_ms: Option<u64>,
    /// Source identifier, e.g. "journald" or "syslog".
    pub source: String,
    /// Structured key/value labels (CBOR key 5).
    #[serde(default)]
    pub labels: HashMap<String, LabelValue>,
    /// Time this record entered the local upload queue.
    pub queued_at_ms: u64,
}
