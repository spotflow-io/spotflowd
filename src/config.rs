use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/spotflow/spotflowd.toml";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub device: DeviceConfig,
    #[serde(default)]
    pub mqtt: MqttConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub logs: LogsConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub crashdump: CrashdumpConfig,
    #[serde(default)]
    pub status: StatusConfig,
}

// ---------------------------------------------------------------------------
// Shared persistent storage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// Root for persistent state: log spool, metric sequences, and crash-dump state.
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            state_dir: default_state_dir(),
        }
    }
}

impl StorageConfig {
    pub fn spool_dir(&self) -> PathBuf {
        self.state_dir.join("spool")
    }

    pub fn metrics_dir(&self) -> PathBuf {
        self.state_dir.join("metrics")
    }

    pub fn crashdump_dir(&self) -> PathBuf {
        self.state_dir.join("crashdump")
    }
}

// ---------------------------------------------------------------------------
// Crash dumps (kernel panics via pstore/ramoops)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CrashdumpConfig {
    /// Set to true to collect kernel crash dumps from pstore and upload them
    /// as CORE_DUMP_CHUNK messages. Requires the daemon to run as root
    /// (reading and clearing /sys/fs/pstore needs root).
    #[serde(default)]
    pub enabled: bool,
    /// Directories to scan for pstore records. Default: /sys/fs/pstore.
    /// Add /var/lib/systemd/pstore when systemd-pstore.service archives records.
    #[serde(default = "default_pstore_paths")]
    pub paths: Vec<PathBuf>,
    /// pstore record kinds (filename prefixes) to collect. Default: dmesg, console.
    #[serde(default = "default_crashdump_kinds")]
    pub kinds: Vec<String>,
    /// Delete a pstore record after its dump has been fully published, freeing
    /// the ramoops ring buffer for the next crash. Default: true.
    #[serde(default = "default_true")]
    pub delete_after_capture: bool,
    /// How often to rescan the pstore directories (seconds). A startup scan
    /// always runs immediately regardless of this value.
    #[serde(default = "default_crashdump_poll_interval")]
    pub poll_interval_secs: u64,
    /// Maximum bytes read from a single record (larger records are truncated).
    #[serde(default = "default_crashdump_max_bytes")]
    pub max_bytes: usize,
    /// Payload size of each CORE_DUMP_CHUNK (bytes of dump content per message).
    #[serde(default = "default_crashdump_chunk_bytes")]
    pub chunk_bytes: usize,
}

impl Default for CrashdumpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            paths: default_pstore_paths(),
            kinds: default_crashdump_kinds(),
            delete_after_capture: true,
            poll_interval_secs: default_crashdump_poll_interval(),
            max_bytes: default_crashdump_max_bytes(),
            chunk_bytes: default_crashdump_chunk_bytes(),
        }
    }
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    /// Shared OS and custom metrics upload window: "none" | "1m" | "1h" | "1d".
    /// "none" → publish each raw sample immediately (no aggregation).
    /// "1m"   → accumulate samples for one minute, then publish sum/count/min/max.
    #[serde(default = "default_aggregation_interval")]
    pub aggregation_interval: String,
    /// OS metrics collection, independent of custom application metrics.
    #[serde(default)]
    pub os: MetricsOsConfig,
    /// Custom application metrics via Unix domain socket.
    #[serde(default)]
    pub custom: MetricsCustomConfig,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            aggregation_interval: default_aggregation_interval(),
            os: MetricsOsConfig::default(),
            custom: MetricsCustomConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsOsConfig {
    /// Set to true to enable OS metrics collection (cpu, memory, disk, network, system).
    #[serde(default)]
    pub enabled: bool,
    /// How often to read /proc and /sys (seconds).
    #[serde(default = "default_collection_interval")]
    pub collection_interval_secs: u64,
    #[serde(default)]
    pub groups: MetricsGroupsConfig,
    #[serde(default)]
    pub disk: MetricsDiskConfig,
    #[serde(default)]
    pub network: MetricsNetworkConfig,
}

impl Default for MetricsOsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            collection_interval_secs: default_collection_interval(),
            groups: MetricsGroupsConfig::default(),
            disk: MetricsDiskConfig::default(),
            network: MetricsNetworkConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsCustomConfig {
    /// Set to true to open the Unix socket and accept custom metrics.
    /// Independent of `metrics.os.enabled` — custom metrics work without OS metrics.
    #[serde(default)]
    pub enabled: bool,
    /// Path to the Unix domain socket. Default: /run/spotflow/metrics.sock
    #[serde(default = "default_custom_socket_path")]
    pub socket_path: PathBuf,
}

impl Default for MetricsCustomConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            socket_path: default_custom_socket_path(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsGroupsConfig {
    #[serde(default = "default_true")]
    pub cpu: bool,
    #[serde(default = "default_true")]
    pub memory: bool,
    #[serde(default = "default_true")]
    pub disk: bool,
    #[serde(default = "default_true")]
    pub network: bool,
    #[serde(default = "default_true")]
    pub system: bool,
}

impl Default for MetricsGroupsConfig {
    fn default() -> Self {
        Self {
            cpu: true,
            memory: true,
            disk: true,
            network: true,
            system: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsDiskConfig {
    /// Mount points to report disk space for. Defaults to root only.
    #[serde(default = "default_mount_points")]
    pub mount_points: Vec<String>,
}

impl Default for MetricsDiskConfig {
    fn default() -> Self {
        Self {
            mount_points: default_mount_points(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsNetworkConfig {
    /// Interfaces to report. Empty list = auto-detect all non-loopback interfaces.
    #[serde(default)]
    pub interfaces: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    pub id: String,
    pub ingest_key: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MqttConfig {
    #[serde(default = "default_broker")]
    pub broker: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_keepalive_secs")]
    pub keepalive_secs: u64,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            broker: default_broker(),
            port: default_port(),
            keepalive_secs: default_keepalive_secs(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogsConfig {
    #[cfg_attr(not(feature = "journald"), allow(dead_code))]
    #[serde(default = "default_journald")]
    pub journald: bool,
    #[cfg_attr(not(feature = "journald"), allow(dead_code))]
    #[serde(default)]
    pub journald_scope: JournaldScope,
    #[serde(default = "default_syslog")]
    pub syslog: bool,
    #[serde(default = "default_syslog_path")]
    pub syslog_path: PathBuf,
    /// Maximum existing current-boot records replayed per source at startup.
    /// The newest records are replayed chronologically. Zero disables replay.
    #[serde(default = "default_startup_max_entries")]
    pub startup_max_entries: usize,
    #[serde(default)]
    pub buffer: BufferConfig,
}

impl Default for LogsConfig {
    fn default() -> Self {
        Self {
            journald: default_journald(),
            journald_scope: JournaldScope::default(),
            syslog: default_syslog(),
            syslog_path: default_syslog_path(),
            startup_max_entries: default_startup_max_entries(),
            buffer: BufferConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JournaldScope {
    System,
    #[default]
    All,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BufferConfig {
    /// Maximum number of log entries held in memory before flushing to disk.
    #[serde(default = "default_memory_max_entries")]
    pub memory_max_entries: usize,

    /// Maximum total size of log spool chunks in MiB; excludes other persistent state.
    #[serde(default = "default_disk_max_size_mb")]
    pub disk_max_size_mb: u64,

    /// Number of log entries per disk chunk file.
    #[serde(default = "default_disk_chunk_max_entries")]
    pub disk_chunk_max_entries: usize,
}

impl Default for BufferConfig {
    fn default() -> Self {
        Self {
            memory_max_entries: default_memory_max_entries(),
            disk_max_size_mb: default_disk_max_size_mb(),
            disk_chunk_max_entries: default_disk_chunk_max_entries(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StatusConfig {
    /// Local-only Unix socket used by `spotflowd status`.
    #[serde(default = "default_status_socket_path")]
    pub socket_path: PathBuf,
}

impl Default for StatusConfig {
    fn default() -> Self {
        Self {
            socket_path: default_status_socket_path(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file: {}", path.display()))?;
        let config: Config =
            toml::from_str(&content).with_context(|| "failed to parse config file")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.device.id.is_empty() {
            anyhow::bail!("device.id must not be empty");
        }
        if self.device.ingest_key.is_empty() {
            anyhow::bail!("device.ingest_key must not be empty");
        }
        if self.mqtt.broker.trim().is_empty() {
            anyhow::bail!("mqtt.broker must not be empty");
        }
        if self.mqtt.port == 0 {
            anyhow::bail!("mqtt.port must be > 0");
        }
        if !(5..=86_400).contains(&self.mqtt.keepalive_secs) {
            anyhow::bail!("mqtt.keepalive_secs must be between 5 and 86400");
        }
        if !self.storage.state_dir.is_absolute() {
            anyhow::bail!("storage.state_dir must be an absolute path");
        }
        #[cfg(not(feature = "journald"))]
        if self.logs.journald {
            anyhow::bail!(
                "logs.journald is enabled, but this binary was compiled without the journald feature"
            );
        }
        if self.logs.startup_max_entries > 1_000_000 {
            anyhow::bail!("logs.startup_max_entries must be <= 1000000");
        }
        if self.logs.buffer.memory_max_entries == 0 {
            anyhow::bail!("logs.buffer.memory_max_entries must be > 0");
        }
        if self.logs.buffer.memory_max_entries > 1_000_000 {
            anyhow::bail!("logs.buffer.memory_max_entries must be <= 1000000");
        }
        if self.logs.buffer.disk_chunk_max_entries == 0 {
            anyhow::bail!("logs.buffer.disk_chunk_max_entries must be > 0");
        }
        if self.logs.buffer.disk_chunk_max_entries > 100_000 {
            anyhow::bail!("logs.buffer.disk_chunk_max_entries must be <= 100000");
        }
        if self.logs.buffer.disk_max_size_mb == 0 {
            anyhow::bail!("logs.buffer.disk_max_size_mb must be > 0");
        }
        if self.logs.buffer.disk_max_size_mb > 1_048_576 {
            anyhow::bail!("logs.buffer.disk_max_size_mb must be <= 1048576");
        }
        if !(1..=86_400).contains(&self.metrics.os.collection_interval_secs) {
            anyhow::bail!("metrics.os.collection_interval_secs must be between 1 and 86400");
        }
        if !matches!(
            self.metrics.aggregation_interval.as_str(),
            "none" | "1m" | "1h" | "1d"
        ) {
            anyhow::bail!("metrics.aggregation_interval must be one of: none, 1m, 1h, 1d");
        }
        if self.crashdump.enabled {
            if self.crashdump.paths.is_empty() {
                anyhow::bail!("crashdump.paths must not be empty");
            }
            if self.crashdump.kinds.is_empty() {
                anyhow::bail!("crashdump.kinds must not be empty");
            }
        }
        if !(1..=604_800).contains(&self.crashdump.poll_interval_secs) {
            anyhow::bail!("crashdump.poll_interval_secs must be between 1 and 604800");
        }
        if !(1..=1_073_741_824).contains(&self.crashdump.max_bytes) {
            anyhow::bail!("crashdump.max_bytes must be between 1 and 1073741824");
        }
        if self.crashdump.chunk_bytes == 0 {
            anyhow::bail!("crashdump.chunk_bytes must be > 0");
        }
        if self.crashdump.chunk_bytes > self.crashdump.max_bytes {
            anyhow::bail!("crashdump.chunk_bytes must be <= crashdump.max_bytes");
        }
        validate_socket_path(&self.status.socket_path, "status.socket_path")?;
        validate_socket_path(
            &self.metrics.custom.socket_path,
            "metrics.custom.socket_path",
        )?;
        if self.metrics.custom.socket_path == self.status.socket_path {
            anyhow::bail!("metrics.custom.socket_path and status.socket_path must be different");
        }
        Ok(())
    }

    /// Validate resources using the identity that will run the daemon.
    pub fn check_accessibility(&self) -> Result<()> {
        check_writable_directory(&self.storage.state_dir, "storage.state_dir")?;
        // The spool is also read on startup to deliver any existing log backlog.
        check_writable_directory(&self.storage.spool_dir(), "log spool directory")?;
        if self.metrics.os.enabled || self.metrics.custom.enabled {
            check_writable_directory(&self.storage.metrics_dir(), "metrics state directory")?;
        }
        if self.crashdump.enabled {
            check_writable_directory(&self.storage.crashdump_dir(), "crashdump state directory")?;
        }

        if self.logs.syslog {
            OpenOptions::new()
                .read(true)
                .open(&self.logs.syslog_path)
                .with_context(|| {
                    format!(
                        "logs.syslog_path is not readable: {}",
                        self.logs.syslog_path.display()
                    )
                })?;
        }

        check_socket_access(&self.status.socket_path, "status.socket_path")?;
        if self.metrics.custom.enabled {
            check_socket_access(
                &self.metrics.custom.socket_path,
                "metrics.custom.socket_path",
            )?;
        }
        Ok(())
    }

    pub fn redacted_json(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        if let Some(device) = value.get_mut("device").and_then(|v| v.as_object_mut()) {
            device.insert(
                "ingest_key".to_string(),
                serde_json::Value::String("[redacted]".to_string()),
            );
        }
        value
    }
}

fn validate_socket_path(path: &Path, name: &str) -> Result<()> {
    if !path.is_absolute() {
        anyhow::bail!("{name} must be an absolute path");
    }
    if path.as_os_str().as_encoded_bytes().len() > 100 {
        anyhow::bail!("{name} is too long for a Unix socket");
    }
    if path.parent().is_none() || path.file_name().is_none() {
        anyhow::bail!("{name} must include a parent directory and file name");
    }
    Ok(())
}

fn check_socket_access(path: &Path, name: &str) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};

    let parent = path.parent().expect("validated socket path has a parent");
    check_writable_directory(parent, &format!("parent directory of {name}"))?;
    if path.exists() {
        if !std::fs::symlink_metadata(path)?.file_type().is_socket() {
            anyhow::bail!("{name} exists but is not a Unix socket: {}", path.display());
        }
        match UnixStream::connect(path) {
            Ok(_) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                return Ok(())
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot access {name}: {}", path.display()))
            }
        }
    }

    let listener = UnixListener::bind(path)
        .with_context(|| format!("cannot create {name}: {}", path.display()))?;
    let permission_result = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660));
    drop(listener);
    let _ = std::fs::remove_file(path);
    permission_result.with_context(|| format!("cannot set permissions on {name}"))?;
    Ok(())
}

fn check_writable_directory(path: &Path, name: &str) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("cannot create {name}: {}", path.display()))?;
    if !std::fs::metadata(path)?.is_dir() {
        anyhow::bail!("{name} is not a directory: {}", path.display());
    }

    let probe = path.join(format!(
        ".spotflowd-write-check-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)?;
        file.write_all(b"ok")?;
        file.sync_all()?;
        std::fs::remove_file(&probe)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&probe);
    }
    result.with_context(|| format!("{name} is not writable: {}", path.display()))
}

fn default_broker() -> String {
    "mqtt.spotflow.io".to_string()
}
fn default_port() -> u16 {
    8883
}
fn default_keepalive_secs() -> u64 {
    60
}
fn default_true() -> bool {
    true
}
fn default_journald() -> bool {
    cfg!(feature = "journald")
}
fn default_syslog() -> bool {
    // On systemd systems the journald source already captures everything rsyslog writes.
    // Default to disabled to avoid duplicate entries. Users on non-systemd targets
    // (Yocto, no journald feature) get syslog enabled by default.
    cfg!(not(feature = "journald"))
}
fn default_syslog_path() -> PathBuf {
    // Try /var/log/syslog first (Debian/Ubuntu); fall back to /var/log/messages (RHEL/Yocto).
    let debian = Path::new("/var/log/syslog");
    if debian.exists() {
        debian.to_path_buf()
    } else {
        PathBuf::from("/var/log/messages")
    }
}
fn default_memory_max_entries() -> usize {
    1000
}
fn default_startup_max_entries() -> usize {
    1000
}
fn default_state_dir() -> PathBuf {
    PathBuf::from("/var/lib/spotflow")
}
fn default_disk_max_size_mb() -> u64 {
    64
}
fn default_disk_chunk_max_entries() -> usize {
    200
}
fn default_collection_interval() -> u64 {
    10
}
fn default_aggregation_interval() -> String {
    "1m".to_string()
}
fn default_mount_points() -> Vec<String> {
    vec!["/".to_string()]
}
fn default_custom_socket_path() -> PathBuf {
    PathBuf::from("/run/spotflow/metrics.sock")
}
fn default_status_socket_path() -> PathBuf {
    PathBuf::from("/run/spotflow/status.sock")
}
fn default_pstore_paths() -> Vec<PathBuf> {
    vec![PathBuf::from("/sys/fs/pstore")]
}
fn default_crashdump_kinds() -> Vec<String> {
    vec!["dmesg".to_string(), "console".to_string()]
}
fn default_crashdump_poll_interval() -> u64 {
    300
}
fn default_crashdump_max_bytes() -> usize {
    262_144
}
fn default_crashdump_chunk_bytes() -> usize {
    8_192
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_config(extra: &str) -> Config {
        toml::from_str(&format!(
            "[device]\nid = \"test\"\ningest_key = \"test\"\n{extra}"
        ))
        .unwrap()
    }

    #[test]
    fn os_and_custom_metrics_can_be_enabled_independently() {
        let defaults = parse_config("");
        assert!(!defaults.metrics.os.enabled);
        assert!(!defaults.metrics.custom.enabled);

        for os_enabled in [false, true] {
            for custom_enabled in [false, true] {
                let config = parse_config(&format!(
                    "[metrics.os]\nenabled = {os_enabled}\n\
                     [metrics.custom]\nenabled = {custom_enabled}\n"
                ));
                config.validate().unwrap();
                assert_eq!(config.metrics.os.enabled, os_enabled);
                assert_eq!(config.metrics.custom.enabled, custom_enabled);
            }
        }
    }

    #[test]
    fn metrics_os_settings_use_nested_configuration_and_status_shape() {
        let config = parse_config(
            "[metrics]\naggregation_interval = '1h'\n\
             [metrics.os]\nenabled = true\ncollection_interval_secs = 30\n\
             [metrics.os.groups]\ncpu = false\n\
             [metrics.os.disk]\nmount_points = ['/data']\n\
             [metrics.os.network]\ninterfaces = ['eth0']\n",
        );
        config.validate().unwrap();
        assert_eq!(config.metrics.aggregation_interval, "1h");
        assert_eq!(config.metrics.os.collection_interval_secs, 30);
        assert!(!config.metrics.os.groups.cpu);
        assert!(config.metrics.os.groups.memory);
        assert_eq!(config.metrics.os.disk.mount_points, ["/data"]);
        assert_eq!(config.metrics.os.network.interfaces, ["eth0"]);

        let metrics = config.redacted_json()["metrics"].clone();
        assert_eq!(metrics["os"]["enabled"], true);
        assert_eq!(metrics["os"]["collection_interval_secs"], 30);
        for key in [
            "enabled",
            "collection_interval_secs",
            "groups",
            "disk",
            "network",
        ] {
            assert!(metrics.get(key).is_none(), "legacy metrics key: {key}");
        }
    }

    #[test]
    fn legacy_os_metrics_settings_are_rejected() {
        for extra in [
            "[metrics]\nenabled = true\n",
            "[metrics]\ncollection_interval_secs = 30\n",
            "[metrics.groups]\ncpu = false\n",
            "[metrics.disk]\nmount_points = ['/data']\n",
            "[metrics.network]\ninterfaces = ['eth0']\n",
        ] {
            let result = toml::from_str::<Config>(&format!(
                "[device]\nid = 'test'\ningest_key = 'test'\n{extra}"
            ));
            assert!(result.is_err(), "accepted legacy configuration: {extra}");
        }
    }

    #[test]
    fn validates_os_collection_interval_with_nested_error_path() {
        for interval in [0, 86_401] {
            let config = parse_config(&format!(
                "[metrics.os]\ncollection_interval_secs = {interval}\n"
            ));
            assert_eq!(
                config.validate().unwrap_err().to_string(),
                "metrics.os.collection_interval_secs must be between 1 and 86400"
            );
        }
    }

    #[test]
    fn journald_scope_defaults_to_all() {
        let config = parse_config("");
        assert_eq!(config.logs.journald_scope, JournaldScope::All);
    }

    #[test]
    fn journald_scope_accepts_supported_values() {
        let system = parse_config("[logs]\njournald_scope = \"system\"\n");
        assert_eq!(system.logs.journald_scope, JournaldScope::System);

        let all = parse_config("[logs]\njournald_scope = \"all\"\n");
        assert_eq!(all.logs.journald_scope, JournaldScope::All);
    }

    #[test]
    fn journald_scope_rejects_unknown_values() {
        let result = toml::from_str::<Config>(
            "[device]\nid = \"test\"\ningest_key = \"test\"\n[logs]\njournald_scope = \"user\"\n",
        );
        assert!(result.is_err());
    }

    #[test]
    fn startup_replay_defaults_to_one_thousand_entries() {
        let config = parse_config("");
        assert_eq!(config.logs.startup_max_entries, 1000);
        assert_eq!(LogsConfig::default().startup_max_entries, 1000);
    }

    #[test]
    fn startup_replay_accepts_custom_limit_and_can_be_disabled() {
        for limit in [0, 25, 1_000_000] {
            let config = parse_config(&format!("[logs]\nstartup_max_entries = {limit}\n"));
            config.validate().unwrap();
            assert_eq!(config.logs.startup_max_entries, limit);
        }
        let config = parse_config("[logs]\nstartup_max_entries = 1000001\n");
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_unknown_keys_at_every_level() {
        let top = toml::from_str::<Config>(
            "unknown = true\n[device]\nid = \"test\"\ningest_key = \"test\"\n",
        );
        assert!(top.is_err());

        let nested = toml::from_str::<Config>(
            "[device]\nid = \"test\"\ningest_key = \"test\"\n[logs.buffer]\ndisk_max_size_mb = 4\nunknown = true\n",
        );
        assert!(nested.is_err());
    }

    #[test]
    fn catches_option_uncommented_under_wrong_header() {
        let result = toml::from_str::<Config>(
            "[device]\nid = \"test\"\ningest_key = \"test\"\nenabled = true\n",
        );
        assert!(result.is_err());
    }

    #[test]
    fn validates_numeric_bounds() {
        let config = parse_config("[mqtt]\nkeepalive_secs = 0\n");
        assert!(config.validate().is_err());

        let config =
            parse_config("[crashdump]\nenabled = true\nchunk_bytes = 20\nmax_bytes = 10\n");
        assert!(config.validate().is_err());
    }

    #[test]
    fn redacts_ingest_key_from_reported_configuration() {
        let config = parse_config("");
        let value = config.redacted_json();
        assert_eq!(value["device"]["ingest_key"], "[redacted]");
        assert_ne!(value["device"]["ingest_key"], "test");
    }

    #[test]
    fn checks_storage_and_socket_directory_access() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = parse_config("[logs]\njournald = false\nsyslog = false\n");
        config.storage.state_dir = temp.path().join("state");
        config.status.socket_path = temp.path().join("run/status.sock");

        config.check_accessibility().unwrap();
        assert!(config.storage.state_dir.is_dir());
        assert!(config.storage.spool_dir().is_dir());
        assert!(config.status.socket_path.parent().unwrap().is_dir());
    }

    #[test]
    fn storage_state_dir_is_shared_and_log_buffer_has_no_path_setting() {
        let defaults = parse_config("");
        assert_eq!(defaults.storage.state_dir, Path::new("/var/lib/spotflow"));

        let config = parse_config("[storage]\nstate_dir = '/data/spotflow'\n");
        config.validate().unwrap();
        assert_eq!(
            config.storage.spool_dir(),
            Path::new("/data/spotflow/spool")
        );
        assert_eq!(
            config.storage.metrics_dir(),
            Path::new("/data/spotflow/metrics")
        );
        assert_eq!(
            config.storage.crashdump_dir(),
            Path::new("/data/spotflow/crashdump")
        );
        let reported = config.redacted_json();
        assert_eq!(reported["storage"]["state_dir"], "/data/spotflow");
        assert!(reported["logs"]["buffer"].get("disk_path").is_none());

        let legacy = toml::from_str::<Config>(
            "[device]\nid = 'test'\ningest_key = 'test'\n\
             [logs.buffer]\ndisk_path = '/data/spotflow'\n",
        );
        assert!(legacy.is_err());
    }

    #[test]
    fn storage_state_dir_must_be_absolute() {
        for path in ["", "state", "./state"] {
            let config = parse_config(&format!("[storage]\nstate_dir = '{path}'\n"));
            assert_eq!(
                config.validate().unwrap_err().to_string(),
                "storage.state_dir must be an absolute path"
            );
        }
    }

    #[test]
    fn checks_state_subdirectories_for_enabled_features() {
        for (os, custom, crashdump) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (false, false, true),
            (true, true, true),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let mut config = parse_config("[logs]\njournald = false\nsyslog = false\n");
            config.storage.state_dir = temp.path().join("state");
            config.status.socket_path = temp.path().join("run/status.sock");
            config.metrics.custom.socket_path = temp.path().join("run/metrics.sock");
            config.metrics.os.enabled = os;
            config.metrics.custom.enabled = custom;
            config.crashdump.enabled = crashdump;
            config.validate().unwrap();
            config.check_accessibility().unwrap();
            assert!(config.storage.spool_dir().is_dir());
            assert_eq!(config.storage.metrics_dir().is_dir(), os || custom);
            assert_eq!(config.storage.crashdump_dir().is_dir(), crashdump);

            if os || custom {
                std::fs::remove_dir(config.storage.metrics_dir()).unwrap();
                std::fs::write(config.storage.metrics_dir(), b"not a directory").unwrap();
                let error = config.check_accessibility().unwrap_err();
                assert!(error.to_string().contains("metrics state directory"));
            } else if crashdump {
                std::fs::remove_dir(config.storage.crashdump_dir()).unwrap();
                std::fs::write(config.storage.crashdump_dir(), b"not a directory").unwrap();
                let error = config.check_accessibility().unwrap_err();
                assert!(error.to_string().contains("crashdump state directory"));
            }
        }
    }

    #[cfg(not(feature = "journald"))]
    #[test]
    fn rejects_compiled_out_journald_source() {
        let config = parse_config("[logs]\njournald = true\n");
        assert!(config.validate().is_err());
    }
}
