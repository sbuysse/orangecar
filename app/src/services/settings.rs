//! Settings persistence — load/save ~/.config/rascar/settings.json

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::RwLock;
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)] // a missing key keeps its default instead of discarding the whole file
pub struct AppSettings {
    pub speed_alarm_kmh: u32,
    pub units: String,
    pub screensaver_timeout: u32,
    /// ICS calendar feeds, merged with `--ics-uris` at startup.
    pub calendar_ics_uris: Vec<String>,
    /// Obsolete: the path of the former ytmusicapi auth file. YouTube Music
    /// now uses the car browser's session (ytbridge.rs) and no longer reads
    /// it; kept so settings.json files that carry it still parse.
    #[serde(default)]
    pub ytmusic_auth_path: Option<String>,
    /// Optional flag to enable/disable YouTube Music. Optional for the same
    /// backward-compatibility reason.
    #[serde(default)]
    pub ytmusic_enabled: Option<bool>,
    /// Dashboard theme id (see `THEMES`); "night" is the default look.
    pub theme: String,
    /// Favourite radio stations / TV channels, by name (shown first).
    #[serde(default)]
    pub radio_favorites: Vec<String>,
    #[serde(default)]
    pub tv_favorites: Vec<String>,
    /// Bluetooth OBD adapter chosen in Settings (MAC + name shown), and its PIN
    /// when it is not 1234/0000. Read by the root service `rascar-obd-link`.
    #[serde(default)]
    pub obd_device: Option<String>,
    #[serde(default)]
    pub obd_device_name: Option<String>,
    #[serde(default)]
    pub obd_pin: Option<String>,
    /// Calendar feeds hidden on the Calendar page, by `calendar::source_id`.
    #[serde(default)]
    pub calendar_hidden: Vec<String>,
    /// Home for the navigation page's "time to home" chip, set from the nav
    /// search ("Set as home"). None = the optional home server's home area
    /// (home-server.json), else no home.
    #[serde(default)]
    pub home: Option<HomePlace>,
    /// Favourite destinations of the navigation page. Hand-edited in
    /// settings.json (not editable from the page).
    #[serde(default)]
    pub nav_favorites: Vec<NavFavorite>,
    /// Usable fuel tank capacity, litres: turns the OBD fuel level (%) into
    /// litres for the refill estimate.
    pub fuel_tank_l: f64,
    /// Price per litre used to cost trips until a refill with a plausible
    /// price has been entered.
    pub fuel_price_eur_l: f64,
    /// Consumption used for a trip segment when the car reports neither fuel
    /// rate (PID 5E) nor air flow (PID 10).
    pub fuel_l100: f64,
    /// Fields this version doesn't know (e.g. `quick_contacts`), kept so a save
    /// from the page never deletes what someone put in settings.json by hand.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HomePlace {
    pub lat: f64,
    pub lon: f64,
    #[serde(default)]
    pub label: String,
}

/// One favourite destination (`nav_favorites`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NavFavorite {
    pub name: String,
    /// Text handed to Google Maps / Waze, which geocode it themselves.
    #[serde(default)]
    pub address: String,
    /// Font Awesome icon class (e.g. "fa-house"); a star when empty.
    #[serde(default)]
    pub icon: String,
    /// Coordinates for the offline router.
    pub lat: f64,
    pub lon: f64,
}

/// Theme ids the frontend defines (`[data-theme]` blocks in dashboard.css).
pub const THEMES: &[&str] = &["night", "glass", "amber", "day"];

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            speed_alarm_kmh: 0,
            units: "metric".to_string(),
            screensaver_timeout: 300,
            calendar_ics_uris: Vec::new(),
            ytmusic_auth_path: None,
            ytmusic_enabled: None,
            theme: "night".to_string(),
            radio_favorites: Vec::new(),
            tv_favorites: Vec::new(),
            obd_device: None,
            obd_device_name: None,
            obd_pin: None,
            calendar_hidden: Vec::new(),
            home: None,
            nav_favorites: Vec::new(),
            fuel_tank_l: 63.0,
            fuel_price_eur_l: 1.80,
            fuel_l100: 9.5,
            extra: serde_json::Map::new(),
        }
    }
}

pub struct SettingsService {
    pub settings: Arc<RwLock<AppSettings>>,
    path: PathBuf,
}

impl SettingsService {
    pub fn new() -> Self {
        let path = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/root"))
            .join(".config/rascar/settings.json");

        let settings = match std::fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_else(|e| {
                warn!("failed to parse settings.json: {e} — using defaults");
                AppSettings::default()
            }),
            Err(_) => {
                info!("settings.json not found — using defaults");
                AppSettings::default()
            }
        };

        // Not the whole struct: calendar_ics_uris are private feed URLs with tokens.
        info!(
            "settings loaded: units={} speed_alarm_kmh={} screensaver_timeout={} calendar_sources={}",
            settings.units, settings.speed_alarm_kmh, settings.screensaver_timeout,
            settings.calendar_ics_uris.len()
        );

        Self {
            settings: Arc::new(RwLock::new(settings)),
            path,
        }
    }

    pub async fn save(&self, new_settings: AppSettings) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let data = serde_json::to_string_pretty(&new_settings)?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || write_durably(&path, data.as_bytes())).await??;
        *self.settings.write().await = new_settings;
        info!("settings saved to {:?}", self.path);
        Ok(())
    }
}

/// Replace `path` so that a power cut at any moment leaves either the old or the
/// new content, never a truncated file (the car cuts power without warning):
/// temp file in the same directory, fsync, rename over, fsync the directory.
/// Keeps the existing file's mode (settings hold private calendar links),
/// owner-only for a new file.
pub(crate) fn write_durably(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let mode = std::fs::metadata(path).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o600);
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod durable_write_tests {
    use super::write_durably;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn replaces_content_and_keeps_owner_only_mode() {
        let dir = std::env::temp_dir().join(format!("rascar-settings-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        write_durably(&path, b"{\"a\":1}").unwrap();
        write_durably(&path, b"{\"a\":2}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":2}");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(!path.with_extension("json.tmp").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
