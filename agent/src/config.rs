//! `/etc/rascar/agent.json` — every field has a default, so an empty `{}`
//! (or no file at all) gives the standard behaviour.
//!
//! The update server itself (URLs and the car's account) lives in a separate,
//! root-only file, `server_json`. Without it the agent stays idle.

use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const DEFAULT_CONFIG: &str = "/etc/rascar/agent.json";
/// Where the update server connection settings are read from by default.
pub const DEFAULT_SERVER_JSON: &str = "/etc/rascar/update-server.json";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentConfig {
    /// Update server settings (URLs, optional pinned certificate, car account),
    /// see [`crate::server::ServerJson`]. Missing file = no server: the agent
    /// neither uploads logs nor checks for updates.
    pub server_json: PathBuf,
    /// Durable agent state, spool and downloads (on the NVMe, never zram).
    pub state_dir: PathBuf,
    pub journal: JournalConfig,
    pub files: Vec<FileSource>,
    pub upload: UploadConfig,
    pub ota: OtaConfig,
    /// Seconds between two status reports to the update server.
    pub status_interval_secs: u64,
    /// Network drivers whose uplink is metered (4G modem, phone tethering).
    pub metered_drivers: Vec<String>,
    /// The dashboard's settings.json (`~<dashboard user>/.config/rascar/settings.json`,
    /// written by setup/install.sh): its `cell_data` holds the driver's
    /// "OTA over 4G" / "logs over 4G" choices (see [`DashboardPolicy`]).
    /// Unset = both allowed.
    pub dashboard_settings: Option<PathBuf>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            server_json: DEFAULT_SERVER_JSON.into(),
            state_dir: "/var/lib/rascar-agent".into(),
            journal: JournalConfig::default(),
            files: vec![
                // fsynced copies of every boot's first 90 s (kmsg + journal):
                // the only trace of a boot that died before the agent ran.
                FileSource::new("/var/lib/rascar-debug", "earlyboot/*.kmsg"),
                FileSource::new("/var/lib/rascar-debug", "earlyboot/*.journal"),
                FileSource::new("/var/lib/rascar-debug", "earlyboot/*.progress"),
                // OBD raw logs (written by rascar-app); any file in the folder.
                FileSource::new("/var/lib/rascar-debug", "obd/*"),
                // CarPlay attempt logs (one per attempt, wire trace included).
                FileSource { max_bytes_per_file: 5 * 1024 * 1024, metered: false, ..FileSource::new("/var/lib/rascar-debug", "boot-*/runs/*.log") },
            ],
            upload: UploadConfig::default(),
            ota: OtaConfig::default(),
            status_interval_secs: 900,
            metered_drivers: ["qmi_wwan", "cdc_mbim", "option", "cdc_ether", "rndis_host", "ipheth"]
                .map(String::from)
                .to_vec(),
            dashboard_settings: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JournalConfig {
    pub enabled: bool,
    /// Units whose messages are kept (`_SYSTEMD_UNIT`, or `UNIT` for systemd's
    /// own lines about them: starts, stops, crashes). Glob patterns.
    pub units: Vec<String>,
    /// Keep kernel messages.
    pub kernel: bool,
    /// Also keep anything at this priority or more severe (3 = err) from
    /// any other source. `null` disables.
    pub any_unit_max_priority: Option<u8>,
    /// Seconds between two journal reads into the spool. Bounds what a power
    /// cut can lose (the journal itself lives in zram).
    pub interval_secs: u64,
    /// Entries read per cycle at most (the rest waits for the next cycle).
    pub max_entries_per_cycle: usize,
}

impl Default for JournalConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            units: ["rascar-*", "hostapd@*", "bluetooth.service", "systemd-networkd.service", "NetworkManager.service", "avahi-daemon.service", "user@*"]
                .map(String::from)
                .to_vec(),
            kernel: true,
            any_unit_max_priority: Some(3),
            interval_secs: 10,
            max_entries_per_cycle: 20_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FileSource {
    /// Directory the glob is relative to; the stream name keeps that relative path.
    pub root: PathBuf,
    pub glob: String,
    /// Bytes of one file shipped at most (the rest is skipped, with a marker).
    pub max_bytes_per_file: u64,
    /// Files not modified for this many days are ignored (backlog cap).
    pub max_age_days: u64,
    /// Ship this source over a metered uplink too.
    pub metered: bool,
}

impl FileSource {
    pub fn new(root: &str, glob: &str) -> Self {
        Self { root: root.into(), glob: glob.into(), ..Self::default() }
    }
}

impl Default for FileSource {
    fn default() -> Self {
        Self { root: PathBuf::new(), glob: String::new(), max_bytes_per_file: 50 * 1024 * 1024, max_age_days: 14, metered: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UploadConfig {
    /// Upload cadence on an unmetered uplink (LAN, home Wi-Fi).
    pub interval_secs: u64,
    /// Upload cadence on 4G / tethering: fewer, bigger batches.
    pub metered_interval_secs: u64,
    /// Compressed bytes per day allowed over a metered uplink (logs only).
    pub metered_daily_budget_bytes: u64,
    /// Raw bytes per batch (gzip shrinks text ~8x).
    pub batch_bytes: u64,
    /// Local journal spool cap; the oldest boots go first, uploaded or not.
    pub spool_max_bytes: u64,
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            interval_secs: 60,
            metered_interval_secs: 900,
            metered_daily_budget_bytes: 30 * 1024 * 1024,
            batch_bytes: 512 * 1024,
            spool_max_bytes: 512 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OtaConfig {
    pub enabled: bool,
    /// Seconds between two "is there a newer release?" questions.
    pub check_interval_secs: u64,
    /// Download bundles over 4G / tethering. On by default: the Pi only has
    /// power while the car runs, so it is rarely on Wi-Fi and 4G is the
    /// normal way a release reaches it (~14 MB each). Off: wait for LAN/Wi-Fi.
    pub download_on_metered: bool,
    /// Hex ed25519 public key file of the release signer (board-only).
    pub pubkey_file: PathBuf,
    /// The path every unit and script uses. Must be a symlink to a release.
    pub install_link: PathBuf,
    /// Where release trees live (same filesystem as `install_link`).
    pub releases_dir: PathBuf,
    /// Release trees kept (current + previous at least).
    pub keep_releases: usize,
    /// Boots a new release may take to prove itself before the boot guard
    /// (`rascar-ota-boot`) rolls it back.
    pub max_trial_boots: u32,
    /// Seconds after the agent starts before judging a trial release.
    pub health_delay_secs: u64,
    /// Paths of the current tree NOT carried into a new release (build
    /// leftovers that nothing reads at runtime).
    pub carry_exclude: Vec<String>,
    /// Device name a manifest must target.
    pub target: String,
}

impl Default for OtaConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_interval_secs: 3600,
            download_on_metered: true,
            pubkey_file: "/etc/rascar/ota.pub".into(),
            install_link: "/opt/orangecar".into(),
            releases_dir: "/opt/orangecar-releases".into(),
            keep_releases: 3,
            max_trial_boots: 3,
            health_delay_secs: 30,
            carry_exclude: [
                ".git",
                ".ota",
                "target/debug",
                "target/release/deps",
                "target/release/build",
                "target/release/incremental",
                "target/release/.fingerprint",
                "target/aarch64-unknown-linux-gnu",
            ]
            .map(String::from)
            .to_vec(),
            target: "orangecar".into(),
        }
    }
}

impl AgentConfig {
    /// Load the config; a missing file means all defaults, a broken one is an error.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read(path) {
            Ok(b) => Ok(serde_json::from_slice(&b).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_gives_defaults_and_fields_override() {
        let c: AgentConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(c.ota.install_link, PathBuf::from("/opt/orangecar"));
        assert!(c.files.iter().any(|f| f.glob == "obd/*"));
        assert!(c.ota.download_on_metered, "releases must reach the car over 4G by default");
        let c: AgentConfig =
            serde_json::from_str(r#"{"ota":{"downloadOnMetered":false},"upload":{"batchBytes":1000}}"#).unwrap();
        assert!(!c.ota.download_on_metered);
        assert_eq!(c.ota.max_trial_boots, 3);
        assert_eq!(c.upload.batch_bytes, 1000);
        assert_eq!(c.upload.interval_secs, 60);
        assert_eq!(c.server_json, PathBuf::from(DEFAULT_SERVER_JSON));
        assert!(c.dashboard_settings.is_none());
    }

    #[test]
    fn server_json_key_overrides_the_default() {
        let c: AgentConfig = serde_json::from_str(r#"{"serverJson":"/etc/rascar/other.json"}"#).unwrap();
        assert_eq!(c.server_json, PathBuf::from("/etc/rascar/other.json"));
    }

    #[test]
    fn missing_file_is_defaults_broken_file_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(AgentConfig::load(&d.path().join("none.json")).is_ok());
        std::fs::write(d.path().join("bad.json"), "{").unwrap();
        assert!(AgentConfig::load(&d.path().join("bad.json")).is_err());
    }
}

/// What the driver allowed over 4G from the dashboard's 4G panel
/// (rascar-app `datausage.rs`, settings.json → `cell_data`). Read every round,
/// so a toggle applies without restarting the agent. A missing or unreadable
/// file allows both: that is the behaviour before the panel existed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DashboardPolicy {
    pub ota_over_4g: bool,
    pub logs_over_4g: bool,
}

impl DashboardPolicy {
    pub fn read(path: Option<&std::path::Path>) -> Self {
        let cell = path
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v.get("cell_data").cloned());
        let flag = |k: &str| cell.as_ref().and_then(|c| c.get(k)).and_then(|v| v.as_bool()).unwrap_or(true);
        Self { ota_over_4g: flag("ota_over_4g"), logs_over_4g: flag("logs_over_4g") }
    }
}

#[cfg(test)]
mod dashboard_policy_tests {
    use super::DashboardPolicy;

    #[test]
    fn reads_the_panel_flags_and_defaults_to_allowed() {
        let dir = std::env::temp_dir().join(format!("rascar-agent-policy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("settings.json");
        assert_eq!(DashboardPolicy::read(Some(&p)), DashboardPolicy { ota_over_4g: true, logs_over_4g: true });
        assert_eq!(DashboardPolicy::read(None), DashboardPolicy { ota_over_4g: true, logs_over_4g: true });
        std::fs::write(&p, r#"{"theme":"night","cell_data":{"ota_over_4g":false,"logs_over_4g":true}}"#).unwrap();
        assert_eq!(DashboardPolicy::read(Some(&p)), DashboardPolicy { ota_over_4g: false, logs_over_4g: true });
        std::fs::write(&p, "not json").unwrap();
        assert_eq!(DashboardPolicy::read(Some(&p)), DashboardPolicy { ota_over_4g: true, logs_over_4g: true });
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
