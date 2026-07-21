use std::path::Path;

use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Table};

use crate::cache::write_atomic;
use crate::geo::{EUROPE_LAT, EUROPE_LON, EUROPE_ZOOM};
use crate::layers::{LayerId, RenderMode};

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
    /// Location acquisition.
    #[serde(default)]
    pub location: LocationConfig,
    /// Place-name search (the `/` prompt).
    #[serde(default)]
    pub geocode: GeocodeConfig,
}

/// Place-name search via OpenStreetMap Nominatim.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GeocodeConfig {
    /// Nominatim search endpoint. Point this at your own instance if you make
    /// heavy use of search — the public one is a donated service with a strict
    /// usage policy.
    #[serde(default = "default_geocode_endpoint")]
    pub endpoint: String,
}

/// How the app figures out where you are.
///
/// The OS location service (GeoClue / Windows Geolocator / CoreLocation) needs
/// no configuration and is always used when available. These settings only
/// govern the IP-address fallback, which is the one source that leaves the
/// machine unprompted.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LocationConfig {
    /// Look up a coarse position from this machine's IP address when the OS
    /// location service is unavailable or still converging.
    ///
    /// This discloses your IP address to `ip_endpoint`. Set to `false` to keep
    /// location strictly on-device; `--no-location` disables every source.
    #[serde(default = "default_ip_fallback")]
    pub ip_fallback: bool,
    /// Service queried for IP-based geolocation. Must return JSON with
    /// `lat`/`lon` (or `latitude`/`longitude`) fields.
    #[serde(default = "default_ip_endpoint")]
    pub ip_endpoint: String,
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
    /// Get yours at <https://devportal.meteogate.eu/>
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
    /// Optional MeteoGate API key.  Register one at
    /// <https://devportal.meteogate.eu/>.
    ///
    /// This is the only knob that meaningfully raises the observation budget:
    /// the gateway allows anonymous callers 50 requests/hour, but 500/hour once
    /// a key identifies you.  Leave empty for anonymous access.
    #[serde(default)]
    pub api_key: String,
}

impl EumetnetConfig {
    /// Requests allowed per hour, per the MeteoGate gateway's published
    /// quotas.  Used to size the client-side budget so we throttle ourselves
    /// instead of discovering the limit as a 429.
    pub fn hourly_quota(&self) -> u32 {
        if self.api_key.trim().is_empty() {
            ANON_HOURLY_QUOTA
        } else {
            AUTH_HOURLY_QUOTA
        }
    }
}

/// MeteoGate gateway default quota for unauthenticated callers: 50 requests
/// per 3600 s (`ratelimitAnon.quota` in the EUMETNET onboarding examples).
pub const ANON_HOURLY_QUOTA: u32 = 50;

/// Quota once an API key identifies the caller: 500 requests per 3600 s.
pub const AUTH_HOURLY_QUOTA: u32 = 500;

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
            api_key: String::new(),
        }
    }
}

impl Default for LocationConfig {
    fn default() -> Self {
        Self {
            ip_fallback: default_ip_fallback(),
            ip_endpoint: default_ip_endpoint(),
        }
    }
}

impl Default for GeocodeConfig {
    fn default() -> Self {
        Self {
            endpoint: default_geocode_endpoint(),
        }
    }
}

fn default_geocode_endpoint() -> String {
    "https://nominatim.openstreetmap.org/search".to_string()
}

fn default_ip_fallback() -> bool {
    true
}
fn default_ip_endpoint() -> String {
    "https://ipwho.is/".to_string()
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

/// A single render-mode assignment stored in `StateConfig`.
/// Replaces the old per-mode scalar fields so new option types can be
/// added as additional `Vec` entries without a schema change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerRenderMode {
    pub layer: LayerId,
    pub mode: RenderMode,
}

/// Mutable runtime state persisted between sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateConfig {
    pub center_lat: f64,
    pub center_lon: f64,
    pub zoom: f64,
    pub enabled_layers: Vec<LayerId>,
    pub selected_layer: LayerId,

    /// Every layer that existed when this file was written, enabled or not.
    ///
    /// Without this, `enabled_layers` is ambiguous: a layer missing from it
    /// could mean "the user turned it off" or "it did not exist yet", and
    /// treating the second as the first silently disables every newly added
    /// layer for existing users. Absent in files written before this field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_layers: Vec<LayerId>,

    /// Render-mode assignments (replaces the scalar braille/color/text fields).
    /// Each entry records which layer owns which render mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub render_modes: Vec<LayerRenderMode>,

    // Legacy scalar fields kept for loading old state.toml files.
    // Absent from new files (skip_serializing_if = is_none).
    // When `render_modes` is non-empty these are ignored on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub braille_layer: Option<LayerId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_layer: Option<LayerId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_layer: Option<LayerId>,

    /// Lightning trail duration in minutes (1–30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lightning_trail_minutes: Option<u8>,

    /// Radar history depth in hours (3 / 6 / 12 / 24).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_hours: Option<u8>,
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
#
# ── MeteoGate API key ────────────────────────────────────────────
# Surface observations go through the MeteoGate API gateway, which enforces a
# request quota per client.  The published defaults are:
#
#     anonymous       10 req/s (burst 20),  {anon_q} requests per hour
#     with an API key 60 req/s (burst 100), {auth_q} requests per hour
#
# front budgets itself against whichever quota applies, so anonymous use works
# — observations simply refresh less often and prioritise the area you are
# looking at.  A key lifts that considerably.
#
# To get one:
#   1. Register an account at https://devportal.meteogate.eu/
#   2. Create an API key from the developer portal
#   3. Paste it below and restart front
#
# The key is sent as an `apikey` header.  Leave empty for anonymous access.
api_key = "{eu_key}"
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
            eu_key = self.eumetnet.api_key,
            anon_q = ANON_HOURLY_QUOTA,
            auth_q = AUTH_HOURLY_QUOTA,
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

// ---------------------------------------------------------------------------
// Surgical config write-back (settings editor persistence primitive)
// ---------------------------------------------------------------------------

/// A single key edit for [`apply_config_edits`]: a dotted key path
/// (`"meteogate.api_key"`) and the new value.
#[derive(Debug, Clone)]
pub struct ConfigEdit {
    pub key: String,
    pub value: ConfigEditValue,
}

/// The value types the settings editor writes: secret strings and bool
/// preferences. Extend as new field types are added.
#[derive(Debug, Clone)]
pub enum ConfigEditValue {
    Str(String),
    Bool(bool),
}

/// Surgically update named keys in an existing `config.toml`, in place.
///
/// Unlike [`Config::write_default`], this never regenerates the file: it
/// parses the existing document, mutates only the given dotted keys
/// (creating missing tables/keys as needed), and writes the result back
/// through the atomic-write path. Comments, key ordering, and any
/// hand-added keys the user wrote elsewhere in the file are left untouched.
///
/// Refuses to save — returns an error — if the existing file cannot be
/// parsed as TOML, rather than silently clobbering it with a fresh default.
pub fn apply_config_edits(path: &Path, edits: &[ConfigEdit]) -> color_eyre::eyre::Result<()> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(color_eyre::eyre::eyre!(
                "read config {}: {e}",
                path.display()
            ))
        }
    };
    let mut doc: DocumentMut = content
        .parse()
        .map_err(|e| color_eyre::eyre::eyre!("parse config {}: {e}", path.display()))?;

    for edit in edits {
        let mut segments = edit.key.split('.').peekable();
        let mut table: &mut Table = doc.as_table_mut();
        while let Some(segment) = segments.next() {
            if segments.peek().is_some() {
                let entry = table
                    .entry(segment)
                    .or_insert_with(|| Item::Table(Table::new()));
                if entry.as_table().is_none() {
                    *entry = Item::Table(Table::new());
                }
                table = entry
                    .as_table_mut()
                    .expect("entry was just forced to Item::Table above");
            } else {
                match &edit.value {
                    ConfigEditValue::Str(s) => table[segment] = toml_edit::value(s.as_str()),
                    ConfigEditValue::Bool(b) => table[segment] = toml_edit::value(*b),
                }
            }
        }
    }

    write_atomic(path, doc.to_string().as_bytes())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Existing installs have a config.toml written before `[location]`
    /// existed; it must keep loading with the section defaulted in.
    #[test]
    fn test_config_without_location_section_still_loads() {
        let cfg: Config = toml::from_str(
            r#"
            [viewport]
            lat = 46.05
            lon = 14.51
            zoom = 5.0
        "#,
        )
        .unwrap();
        assert_eq!(cfg.viewport.lat, 46.05);
        assert!(cfg.location.ip_fallback);
        assert!(!cfg.location.ip_endpoint.is_empty());
    }

    #[test]
    fn test_config_without_geocode_section_still_loads() {
        let cfg: Config = toml::from_str("[viewport]\nlat = 46.05\n").unwrap();
        assert!(cfg.geocode.endpoint.contains("nominatim"));
    }

    #[test]
    fn test_config_geocode_section_roundtrips() {
        let mut original = Config::default();
        original.geocode.endpoint = "https://nominatim.example.invalid/search".to_string();
        let text = toml::to_string_pretty(&original).unwrap();
        let loaded: Config = toml::from_str(&text).unwrap();
        assert_eq!(
            loaded.geocode.endpoint,
            "https://nominatim.example.invalid/search"
        );
    }

    #[test]
    fn test_config_location_section_roundtrips() {
        let mut original = Config::default();
        original.location.ip_fallback = false;
        original.location.ip_endpoint = "https://example.invalid/json".to_string();
        let text = toml::to_string_pretty(&original).unwrap();
        let loaded: Config = toml::from_str(&text).unwrap();
        assert!(!loaded.location.ip_fallback);
        assert_eq!(loaded.location.ip_endpoint, "https://example.invalid/json");
    }

    #[test]
    fn test_config_default_viewport_matches_constants() {
        let cfg = Config::default();
        assert_eq!(cfg.viewport.lat, EUROPE_LAT);
        assert_eq!(cfg.viewport.lon, EUROPE_LON);
        assert_eq!(cfg.viewport.zoom, EUROPE_ZOOM);
    }

    #[test]
    fn test_state_config_toml_roundtrip() {
        let original = StateConfig {
            center_lat: 46.05,
            center_lon: 14.51,
            zoom: 5.0,
            enabled_layers: vec![LayerId::Radar, LayerId::MapBorders],
            known_layers: vec![LayerId::Radar, LayerId::MapBorders],
            selected_layer: LayerId::Radar,
            render_modes: vec![LayerRenderMode {
                layer: LayerId::Radar,
                mode: RenderMode::Braille,
            }],
            braille_layer: None,
            color_layer: None,
            text_layer: None,
            lightning_trail_minutes: Some(10),
            history_hours: Some(6),
        };
        let toml_str = toml::to_string_pretty(&original).expect("serialize");
        let loaded: StateConfig = toml::from_str(&toml_str).expect("deserialize");
        assert!((loaded.center_lat - original.center_lat).abs() < 1e-9);
        assert!((loaded.center_lon - original.center_lon).abs() < 1e-9);
        assert_eq!(loaded.selected_layer, original.selected_layer);
        assert_eq!(loaded.lightning_trail_minutes, Some(10));
        assert_eq!(loaded.history_hours, Some(6));
        assert_eq!(loaded.render_modes.len(), 1);
        assert_eq!(loaded.render_modes[0].layer, LayerId::Radar);
        assert_eq!(loaded.render_modes[0].mode, RenderMode::Braille);
    }

    #[test]
    fn test_state_config_legacy_scalar_fields_roundtrip() {
        let toml_str = r#"
            center_lat = 50.0
            center_lon = 10.0
            zoom = 4.0
            enabled_layers = ["Radar"]
            selected_layer = "Radar"
            braille_layer = "Radar"
            color_layer = "MeteoAlarm"
        "#;
        let loaded: StateConfig = toml::from_str(toml_str).expect("legacy fields must parse");
        assert_eq!(loaded.braille_layer, Some(LayerId::Radar));
        assert_eq!(loaded.color_layer, Some(LayerId::MeteoAlarm));
        assert_eq!(loaded.text_layer, None);
        assert!(
            loaded.render_modes.is_empty(),
            "no render_modes in legacy TOML"
        );
    }

    /// Editing one key must preserve surrounding comments and a hand-added
    /// extra key the user wrote outside the generated schema, and the edited
    /// key must round-trip to the new value on re-read.
    #[test]
    fn test_apply_config_edits_preserves_comments_and_extra_keys() {
        let dir = std::env::temp_dir().join(format!(
            "front-config-edit-test-{}-{}",
            std::process::id(),
            "preserve"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"# top-of-file comment
[meteogate]
# a comment right above the key
api_key = "old-key"
my_extra_hand_written_setting = "keep-me"
s3_endpoint = "https://s3.example.invalid"
"#,
        )
        .unwrap();

        apply_config_edits(
            &path,
            &[ConfigEdit {
                key: "meteogate.api_key".to_string(),
                value: ConfigEditValue::Str("new-key".to_string()),
            }],
        )
        .unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("# top-of-file comment"),
            "top-of-file comment must survive: {written}"
        );
        assert!(
            written.contains("# a comment right above the key"),
            "comment above edited key must survive: {written}"
        );
        assert!(
            written.contains("my_extra_hand_written_setting = \"keep-me\""),
            "hand-added extra key must survive: {written}"
        );

        let reloaded = Config::load(&path).unwrap();
        assert_eq!(reloaded.meteogate.api_key, "new-key");
        assert_eq!(reloaded.meteogate.s3_endpoint, "https://s3.example.invalid");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_apply_config_edits_bool_and_string_and_missing_section() {
        let dir = std::env::temp_dir().join(format!(
            "front-config-edit-test-{}-{}",
            std::process::id(),
            "missing-section"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        // No [location] section at all -- predates that field, per the spec risk.
        std::fs::write(&path, "[viewport]\nlat = 46.05\n").unwrap();

        apply_config_edits(
            &path,
            &[
                ConfigEdit {
                    key: "location.ip_fallback".to_string(),
                    value: ConfigEditValue::Bool(false),
                },
                ConfigEdit {
                    key: "eumetnet.api_key".to_string(),
                    value: ConfigEditValue::Str("euk".to_string()),
                },
            ],
        )
        .unwrap();

        let reloaded = Config::load(&path).unwrap();
        assert!(!reloaded.location.ip_fallback);
        assert_eq!(reloaded.eumetnet.api_key, "euk");
        assert_eq!(reloaded.viewport.lat, 46.05);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_apply_config_edits_refuses_malformed_file() {
        let dir = std::env::temp_dir().join(format!(
            "front-config-edit-test-{}-{}",
            std::process::id(),
            "malformed"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "this is [ not valid toml").unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let result = apply_config_edits(
            &path,
            &[ConfigEdit {
                key: "meteogate.api_key".to_string(),
                value: ConfigEditValue::Str("new-key".to_string()),
            }],
        );

        assert!(result.is_err(), "malformed file must be refused");
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "malformed file must be left untouched");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_state_config_lightning_trail_absent_deserializes_to_none() {
        let toml_str = r#"
            center_lat = 46.0
            center_lon = 14.5
            zoom = 4.0
            enabled_layers = []
            selected_layer = "Radar"
        "#;
        let loaded: StateConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(loaded.lightning_trail_minutes, None);
    }
}
