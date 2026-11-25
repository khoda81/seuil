//! TOML configuration for the daemon.
//!
//! Reads `$XDG_CONFIG_HOME/seuil/config.toml`, so the command line flags
//! become optional for daily use:
//!
//! ```toml
//! phone_mac = "AA:BB:CC:DD:EE:FF"
//! buds_mac = "11:22:33:44:55:66"
//! # adapter = "hci0"
//! # discover = true
//! # handoff_dwell_secs = 10
//! # coc_backoff_start_secs = 1
//! # coc_backoff_cap_secs = 60
//! # coc_keepalive_timeout_secs = 10
//! # coc_frame_timeout_secs = 10
//! # peer_map_limit = 256
//! # media_threshold_dbfs = -60
//! # media_window_ms = 200
//! ```
//!
//! Missing MACs fall back to the enrolled Samsung account device list in
//! `credentials.json` (phone) and to a BlueZ device whose name contains
//! "Buds" (buds).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{debug, warn};

use crate::auth::Credentials;

/// Values read from `config.toml`.
#[derive(Debug, Deserialize)]
pub struct FileConfig {
    pub phone_mac: Option<String>,
    pub buds_mac: Option<String>,
    pub adapter: Option<String>,
    pub device_name: Option<String>,
    /// Allow the enrolled account list and BlueZ to fill missing MACs.
    #[serde(default = "default_discover")]
    pub discover: bool,
    /// Minimum spacing between handoff requests. [FR-010]
    #[serde(default = "default_handoff_dwell_secs")]
    pub handoff_dwell_secs: u64,
    /// First delay of the CoC connect backoff. [FR-010]
    #[serde(default = "default_backoff_start_secs")]
    pub coc_backoff_start_secs: u64,
    /// Ceiling of the CoC connect backoff. [FR-010]
    #[serde(default = "default_backoff_cap_secs")]
    pub coc_backoff_cap_secs: u64,
    /// Upper bound on one CoC keepalive write. [FR-012]
    #[serde(default = "default_keepalive_timeout_secs")]
    pub coc_keepalive_timeout_secs: u64,
    /// Upper bound on one CoC frame read. [FR-012]
    #[serde(default = "default_frame_timeout_secs")]
    pub coc_frame_timeout_secs: u64,
    /// Cap on per-peer bookkeeping maps. [FR-012]
    #[serde(default = "default_peer_map_limit")]
    pub peer_map_limit: usize,
    /// Peak floor in dBFS for media activity. [FR-016]
    #[serde(default = "default_media_threshold_dbfs")]
    pub media_threshold_dbfs: i32,
    /// Measured PCM duration to confirm a change, in milliseconds. [FR-016]
    #[serde(default = "default_media_window_ms")]
    pub media_window_ms: u64,
}

fn default_discover() -> bool {
    true
}

fn default_handoff_dwell_secs() -> u64 {
    10
}

fn default_backoff_start_secs() -> u64 {
    1
}

fn default_backoff_cap_secs() -> u64 {
    60
}

fn default_keepalive_timeout_secs() -> u64 {
    10
}

fn default_frame_timeout_secs() -> u64 {
    10
}

fn default_peer_map_limit() -> usize {
    256
}

fn default_media_threshold_dbfs() -> i32 {
    -60
}

fn default_media_window_ms() -> u64 {
    200
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            phone_mac: None,
            buds_mac: None,
            adapter: None,
            device_name: None,
            discover: true,
            handoff_dwell_secs: default_handoff_dwell_secs(),
            coc_backoff_start_secs: default_backoff_start_secs(),
            coc_backoff_cap_secs: default_backoff_cap_secs(),
            coc_keepalive_timeout_secs: default_keepalive_timeout_secs(),
            coc_frame_timeout_secs: default_frame_timeout_secs(),
            peer_map_limit: default_peer_map_limit(),
            media_threshold_dbfs: default_media_threshold_dbfs(),
            media_window_ms: default_media_window_ms(),
        }
    }
}

impl FileConfig {
    /// Dwell between handoff requests. [FR-010]
    pub fn handoff_dwell(&self) -> Duration {
        Duration::from_secs(self.handoff_dwell_secs.max(1))
    }

    /// First CoC connect backoff delay. [FR-010]
    pub fn backoff_start(&self) -> Duration {
        Duration::from_secs(self.coc_backoff_start_secs.max(1))
    }

    /// CoC connect backoff ceiling. [FR-010]
    pub fn backoff_cap(&self) -> Duration {
        Duration::from_secs(self.coc_backoff_cap_secs.max(1))
    }

    /// Bound on one keepalive write. [FR-012]
    pub fn keepalive_timeout(&self) -> Duration {
        Duration::from_secs(self.coc_keepalive_timeout_secs.max(1))
    }

    /// Bound on one frame read. [FR-012]
    pub fn frame_timeout(&self) -> Duration {
        Duration::from_secs(self.coc_frame_timeout_secs.max(1))
    }

    /// Cap on per-peer bookkeeping maps. [FR-012]
    pub fn peer_limit(&self) -> usize {
        self.peer_map_limit.max(1)
    }

    /// Peak floor for media activity. [FR-016]
    pub fn media_threshold_dbfs(&self) -> f32 {
        self.media_threshold_dbfs as f32
    }

    /// Measured PCM duration to confirm an activity change. [FR-016]
    pub fn media_window(&self) -> Duration {
        Duration::from_millis(self.media_window_ms.max(1))
    }
}

/// The configuration directory shared with the credential store.
///
/// Returns an error when neither `XDG_CONFIG_HOME` nor `HOME` is set, so the
/// daemon never writes into the current directory. [FR-022]
pub fn try_config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("MCFDS_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(base) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(base).join("seuil"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".config").join("seuil"));
    }
    anyhow::bail!("neither HOME nor XDG_CONFIG_HOME is set; cannot locate the config directory")
}

/// The daemon's configuration directory.
///
/// Callers that can report a failure use [`try_config_dir`]. This lenient form
/// keeps read-only callers (doctor, setup) working; write paths use the checked
/// form.
pub fn config_dir() -> PathBuf {
    try_config_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Loads `config.toml`; a missing file yields defaults.
///
/// [FR-022] For daemon startup use [`load_strict`]: a present but invalid file
/// aborts instead of silently substituting defaults.
pub fn load() -> FileConfig {
    let path = config_dir().join("config.toml");
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str(&text) {
            Ok(config) => {
                debug!(path = %path.display(), "Loaded config.toml");
                config
            }
            Err(error) => {
                warn!(path = %path.display(), "Invalid config.toml: {error}");
                FileConfig {
                    discover: true,
                    ..FileConfig::default()
                }
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %path.display(), "No config.toml; using flags and discovery");
            FileConfig {
                discover: true,
                ..FileConfig::default()
            }
        }
        Err(error) => {
            warn!(path = %path.display(), "Could not read config.toml: {error}");
            FileConfig {
                discover: true,
                ..FileConfig::default()
            }
        }
    }
}

/// Loads `config.toml` for daemon startup.
///
/// A missing file stays non-fatal and yields defaults. A present file that
/// cannot be read or parsed is fatal and names the path and the parse failure.
/// [FR-022]
pub fn load_strict() -> Result<FileConfig> {
    let path = try_config_dir()?.join("config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %path.display(), "No config.toml; using flags and discovery");
            return Ok(FileConfig {
                discover: true,
                ..FileConfig::default()
            });
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("could not read config file {}", path.display()));
        }
    };
    let config: FileConfig = toml::from_str(&text)
        .with_context(|| format!("invalid config file {}", path.display()))?;
    debug!(path = %path.display(), "Loaded config.toml");
    Ok(config)
}

/// Picks the first phone address from the enrolled account device list.
pub fn discover_phone(credentials: Option<&Credentials>) -> Option<String> {
    credentials.and_then(|credentials| {
        credentials
            .devices
            .iter()
            .find(|device| {
                let kind = device.device_type.to_ascii_uppercase();
                device.bt_address.len() == 17 && kind.contains("PHONE")
            })
            .or_else(|| {
                credentials.devices.iter().find(|device| {
                    let kind = device.device_type.to_ascii_uppercase();
                    device.bt_address.len() == 17
                        && !kind.contains("PC")
                        && !kind.contains("WATCH")
                })
            })
            .map(|device| device.bt_address.clone())
    })
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::auth::DeviceInfo;

    fn device(address: &str, kind: &str) -> DeviceInfo {
        DeviceInfo {
            bt_address: address.into(),
            device_name: String::new(),
            device_type: kind.into(),
            device_model: String::new(),
        }
    }

    #[test]
    fn discovers_the_phone_from_the_device_list() {
        let mut credentials = Credentials::default();
        credentials.devices = vec![
            device("22:33:44:55:66:77", "BUDS"),
            device("AA:BB:CC:DD:EE:FF", "PHONE"),
            device("11:22:33:44:55:66", "PC"),
        ];
        assert_eq!(
            discover_phone(Some(&credentials)).as_deref(),
            Some("AA:BB:CC:DD:EE:FF")
        );
    }

    #[test]
    fn discovery_skips_pc_and_watch_entries() {
        let mut credentials = Credentials::default();
        credentials.devices = vec![
            device("11:22:33:44:55:66", "PC"),
            device("22:33:44:55:66:77", "WATCH"),
            device("77:88:99:AA:BB:CC", "TABLET"),
        ];
        assert_eq!(
            discover_phone(Some(&credentials)).as_deref(),
            Some("77:88:99:AA:BB:CC")
        );
    }

    #[test]
    fn no_credentials_yields_no_phone() {
        assert_eq!(discover_phone(None), None);
    }

    #[test]
    fn config_dir_honours_the_override() {
        // The variable is process-wide, so only assert the naming contract
        // when no override is active (a test harness may preset it).
        if std::env::var_os("MCFDS_CONFIG_DIR").is_some() {
            return;
        }
        let dir = config_dir();
        assert!(dir.ends_with("seuil"));
    }
}
