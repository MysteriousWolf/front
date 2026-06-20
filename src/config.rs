use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::cache::write_atomic;
use crate::geo::{EUROPE_LAT, EUROPE_LON, EUROPE_ZOOM};
use crate::layers::LayerId;

/// Application-level configuration loaded from `~/.config/front/config.toml`.
///
/// All fields have safe defaults so the file is entirely optional.
/// If the file does not exist a complete default is written on first boot.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Config {
    /// Default viewport centre & zoom used when no saved state exists.
    #[serde(default)]
    pub viewport: ViewportConfig,
    /// MeteoGate radar data access.
    #[serde(default)]
    pub meteogate: MeteoGateConfig,
    /// MeteoAlarm weather warnings.
    #[serde(default)]
    pub meteoalarm: MeteoAlarmConfig,
    /// EUMETNET surface observation data.
    #[serde(default)]
    pub eumetnet: EumetnetConfig,
}

/// Default viewport position on first launch (no persisted state).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ViewportConfig {
    /// Latitude  (degrees, WGS‑84).
    #[serde(default = "default_viewport_lat")]
    pub lat: f64,
    /// Longitude (degrees, WGS‑84).
    #[serde(default = "default_viewport_lon")]
    pub lon: f64,
    /// Zoom level  (0 = whole world, 8 = city block).
    #[serde(default = "default_viewport_zoom")]
    pub zoom: f64,
}

impl Default for ViewportConfig {
    fn default() -> Self {
        Self {
            lat: default_viewport_lat(),
            lon: default_viewport_lon(),
            zoom: default_viewport_zoom(),
        }
    }
}

fn default_viewport_lat() -> f64 {
    EUROPE_LAT
}
fn default_viewport_lon() -> f64 {
    EUROPE_LON
}
fn default_viewport_zoom() -> f64 {
    EUROPE_ZOOM
}

/// MeteoGate (radar data from the Slovenian Weather Radar network).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MeteoGateConfig {
    /// Optional API key for higher rate limits on the ORD REST API.
    /// Get yours at https://devportal.meteogate.eu/
    /// The S3 bucket does not require authentication.
    /// Leave empty to use anonymous access.
    #[serde(default)]
    pub api_key: String,
    /// S3-compatible object store endpoint.
    #[serde(default = "default_s3_endpoint")]
    pub s3_endpoint: String,
    /// S3 bucket name holding the 24-hour radar data cache.
    #[serde(default = "default_s3_bucket")]
    pub s3_bucket: String,
}

/// MeteoAlarm (official European weather warnings).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MeteoAlarmConfig {
    /// Optional API token for authenticated access.
    /// Leave empty for anonymous access.
    #[serde(default)]
    pub token: String,
    /// EDR API endpoint.
    #[serde(default = "default_meteoalarm_endpoint")]
    pub api_endpoint: String,
    /// MQTT broker URL for live updates.
    #[serde(default = "default_meteoalarm_broker")]
    pub mqtt_broker: String,
}

/// EUMETNET surface observations via MeteoGate.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EumetnetConfig {
    /// Surface observations REST endpoint.
    #[serde(default = "default_surface_obs_endpoint")]
    pub surface_endpoint: String,
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

impl Default for MeteoAlarmConfig {
    fn default() -> Self {
        Self {
            token: String::new(),
            api_endpoint: default_meteoalarm_endpoint(),
            mqtt_broker: default_meteoalarm_broker(),
        }
    }
}

impl Default for MeteoGateConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            s3_endpoint: default_s3_endpoint(),
            s3_bucket: default_s3_bucket(),
        }
    }
}

impl Default for EumetnetConfig {
    fn default() -> Self {
        Self {
            surface_endpoint: default_surface_obs_endpoint(),
        }
    }
}

fn default_s3_endpoint() -> String {
    "https://s3.waw3-1.cloudferro.com".to_string()
}
fn default_s3_bucket() -> String {
    "openradar-24h".to_string()
}
fn default_meteoalarm_endpoint() -> String {
    "https://api.meteoalarm.org/edr/v1".to_string()
}
fn default_meteoalarm_broker() -> String {
    "mqtts://api.meteoalarm.org".to_string()
}
fn default_surface_obs_endpoint() -> String {
    "https://api.meteogate.eu/eu-eumetnet-surface-observations".to_string()
}

// ---------------------------------------------------------------------------
// Runtime state persisted as TOML  (replaces the old state.json)
// ---------------------------------------------------------------------------

/// Mutable runtime state persisted between sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateConfig {
    pub center_lat: f64,
    pub center_lon: f64,
    pub zoom: f64,
    pub enabled_layers: Vec<LayerId>,
    pub selected_layer: LayerId,
    pub braille_layer: Option<LayerId>,
    pub color_layer: Option<LayerId>,
    pub text_layer: Option<LayerId>,
}

// ---------------------------------------------------------------------------
// Config implementation
// ---------------------------------------------------------------------------

impl Config {
    /// Load configuration from the given TOML path.
    ///
    /// If the file does not exist, a complete default file is written to
    /// `path` so the user can inspect and edit all available settings.
    pub fn load(path: &Path) -> color_eyre::eyre::Result<Self> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cfg = Self::default();
                cfg.write_default(path)?;
                return Ok(cfg);
            }
            Err(e) => {
                return Err(color_eyre::eyre::eyre!(
                    "read config {}: {e}",
                    path.display()
                ))
            }
        };
        let config: Config = toml::from_str(&content)
            .map_err(|e| color_eyre::eyre::eyre!("parse config {}: {e}", path.display()))?;
        Ok(config)
    }

    /// Write a complete default configuration file with all keys
    /// documented via inline comments.
    pub fn write_default(&self, path: &Path) -> color_eyre::eyre::Result<()> {
        let raw = format!(
            r###"# ── front configuration ──────────────────────────────────────────
# This file was auto-generated with default values.
# Remove or adjust anything you do not need.
# API keys / tokens left empty — the program will use anonymous access.

[viewport]
# Default viewport centre when no saved state exists (degrees, WGS‑84).
lat = {lat}
lon = {lon}
zoom = {zoom}

[meteogate]
# API key for MeteoGate ORD REST API  (https://devportal.meteogate.eu/).
# Leave empty for anonymous access (S3 bucket does not need auth).
api_key = "{mg_key}"
# S3-compatible object store endpoint.
s3_endpoint = "{mg_s3}"
# S3 bucket for the 24‑hour radar data cache.
s3_bucket = "{mg_bucket}"

[meteoalarm]
# Authentication token for MeteoAlarm API.  Leave empty for anonymous.
token = "{ma_token}"
# EDR API base endpoint.
api_endpoint = "{ma_api}"
# MQTT broker URL for live warning updates.
mqtt_broker = "{ma_mqtt}"

[eumetnet]
# REST endpoint for surface observations.
surface_endpoint = "{eu_surface}"
"###,
            lat = self.viewport.lat,
            lon = self.viewport.lon,
            zoom = self.viewport.zoom,
            mg_key = self.meteogate.api_key,
            mg_s3 = self.meteogate.s3_endpoint,
            mg_bucket = self.meteogate.s3_bucket,
            ma_token = self.meteoalarm.token,
            ma_api = self.meteoalarm.api_endpoint,
            ma_mqtt = self.meteoalarm.mqtt_broker,
            eu_surface = self.eumetnet.surface_endpoint,
        );
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                color_eyre::eyre::eyre!("create config dir {}: {e}", parent.display())
            })?;
        }
        std::fs::write(path, raw)
            .map_err(|e| color_eyre::eyre::eyre!("write config {}: {e}", path.display()))?;
        Ok(())
    }
}

impl StateConfig {
    /// Load runtime state from a TOML file.
    pub fn load(path: &Path) -> color_eyre::eyre::Result<Option<Self>> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(color_eyre::eyre::eyre!(
                    "read state {}: {e}",
                    path.display()
                ))
            }
        };
        let state: StateConfig = toml::from_str(&content)
            .map_err(|e| color_eyre::eyre::eyre!("parse state {}: {e}", path.display()))?;
        Ok(Some(state))
    }

    /// Persist runtime state atomically.
    pub fn save(&self, path: &Path) -> color_eyre::eyre::Result<()> {
        let raw = toml::to_string_pretty(self)
            .map_err(|e| color_eyre::eyre::eyre!("serialize state: {e}"))?;
        write_atomic(path, raw.as_bytes())?;
        Ok(())
    }
}
