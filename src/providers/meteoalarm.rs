use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use color_eyre::eyre::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task;

use crate::cache::{read_if_exists, write_atomic, write_log, FrontDirs};
use crate::config::MeteoAlarmConfig;
use crate::geo::{WorldPoint, lat_lon_to_world};
use crate::layers::{WarningFeature, WarningLayer};

// Location filter used by MeteoAlarm API. The API supports per-region
// queries; the default on startup is ALL (Europe-wide).
const LOCATION_ID_ALL: &str = "ALL";

/// MeteoAlarm weather-warnings provider.
#[derive(Debug, Clone)]
pub struct MeteoAlarmProvider {
    client: Client,
    dirs: FrontDirs,
    config: MeteoAlarmConfig,
    cancel: Arc<AtomicBool>,
}

impl MeteoAlarmProvider {
    pub fn new(client: Client, dirs: FrontDirs, config: MeteoAlarmConfig, cancel: Arc<AtomicBool>) -> Self {
        Self { client, dirs, config, cancel }
    }

    /// Fetch and decode active MeteoAlarm warnings for the configured area.
    /// Data is cached on disk for 5 minutes to reduce API calls.
    pub async fn warnings(&self) -> Result<WarningLayer> {
        // Cache location
        let log = &self.dirs.log_path;
        let path: PathBuf = self.dirs.cache_dir.join("meteoalarm/warnings.json");

        // Try cache first if fresh
        if let Ok(Some(bytes)) = read_if_exists(&path) {
            if let Ok(meta) = std::fs::metadata(&path) {
                if let Ok(mtime) = meta.modified() {
                    if let Ok(age) = SystemTime::now().duration_since(mtime) {
                        if age.as_secs() < 300 {
                            if let Ok(layer) = deserialize_cached_warning_layer(&bytes) {
                                write_log(log, "meteoalarm: cache hit");
                                return Ok(layer);
                            } else {
                                write_log(log, "meteoalarm: cache parse failed, refetching");
                            }
                        }
                    }
                }
            }
        }

        // Cache miss or stale: fetch from API
        let location = LOCATION_ID_ALL;
        let mut url = format!("{}/collections/warnings/locations/{}", self.config.api_endpoint, location);
        if !self.config.token.is_empty() {
            url.push_str(&format!("?token={}", self.config.token));
        }

        write_log(log, format!("meteoalarm: GET {url}"));
        // API call with error handling similar to MeteoGateProvider
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .wrap_err_with(|| format!("download MeteoAlarm: {url}"))?
            .error_for_status()
            .wrap_err_with(|| format!("MeteoAlarm response: {url}"))?;
        let json: Value = resp
            .json()
            .await
            .wrap_err("read MeteoAlarm JSON")?;

        // Decode GeoJSON.FeatureCollection in a blocking thread to keep
        // the async runtime responsive during CPU-heavy parsing
        let (out_features, now) = {
            let json_for_parse = json;
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
            let cancel = self.cancel.clone();
            let handle = task::spawn_blocking(move || {
                if cancel.load(Ordering::Relaxed) {
                    return (Vec::new(), now);
                }
                // Decode GeoJSON.FeatureCollection manually; no external crates for GeoJSON parsing
                let features = json_for_parse
                    .get("features")
                    .and_then(|f| f.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut parsed: Vec<WarningFeature> = Vec::new();
                for feat in features {
                    // Geometry
                    let geometry = feat.get("geometry");
                    let geom_type = geometry.and_then(|g| g.get("type").and_then(|t| t.as_str())).unwrap_or("");
                    if geom_type != "Polygon" {
                        continue;
                    }
                    // Coordinates: [[[lon, lat], ...]]
                    let coords = geometry
                        .and_then(|g| g.get("coordinates"))
                        .and_then(|c| c.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|ring| ring.as_array())
                        .unwrap_or(&Vec::new())
                        .clone();

                    let mut polygon: Vec<WorldPoint> = Vec::new();
                    for pt in coords {
                        if let Some(pair) = pt.as_array() {
                            if pair.len() >= 2 {
                                let lon = pair[0].as_f64().unwrap_or(0.0);
                                let lat = pair[1].as_f64().unwrap_or(0.0);
                                polygon.push(lat_lon_to_world(lat, lon));
                            }
                        }
                    }
                    if polygon.is_empty() {
                        continue;
                    }

                    // Properties
                    let empty_props = serde_json::Map::new();
                    let props = feat
                        .get("properties")
                        .and_then(|p| p.as_object())
                        .unwrap_or(&empty_props);
                    let country_code = props.get("countryCode").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let awareness_level = props
                        .get("awareness_level")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let event = props
                        .get("awareness_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    // Onset / expires as epoch seconds (if provided)
                    let onset = props
                        .get("onset")
                        .and_then(|v| v.as_str())
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.timestamp());
                    let expires = props
                        .get("expires")
                        .and_then(|v| v.as_str())
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.timestamp());

                    let wf = WarningFeature {
                        polygon,
                        awareness_level,
                        event,
                        country_code,
                        onset,
                        expires,
                    };
                    parsed.push(wf);
                }
                (parsed, now)
            });
            match handle.await {
                Ok((parsed, t)) => (parsed, t),
                Err(_) => (Vec::new(), now),
            }
        };

        // Prepare serializable cache representation
        #[derive(Serialize, Deserialize)]
        struct SerializableWarningFeature {
            polygon: Vec<WorldPoint>,
            awareness_level: String,
            event: String,
            country_code: String,
            onset: Option<i64>,
            expires: Option<i64>,
        }
        #[derive(Serialize, Deserialize)]
        struct SerializableWarningLayer {
            features: Vec<SerializableWarningFeature>,
            updated_at: Option<i64>,
        }

        let ser_features: Vec<SerializableWarningFeature> = out_features
            .iter()
            .map(|wf| SerializableWarningFeature {
                polygon: wf.polygon.clone(),
                awareness_level: wf.awareness_level.clone(),
                event: wf.event.clone(),
                country_code: wf.country_code.clone(),
                onset: wf.onset,
                expires: wf.expires,
            })
            .collect();
        let layer_to_cache = SerializableWarningLayer {
            features: ser_features,
            updated_at: Some(now),
        };

        let bytes = serde_json::to_vec(&layer_to_cache).unwrap_or_default();
        if !bytes.is_empty() {
            let _ = write_atomic(&path, &bytes);
        }

        Ok(WarningLayer { features: out_features, updated_at: Some(now) })
    }
}

// Helper to deserialize a previously cached struct (compat layer)
fn deserialize_cached_warning_layer(bytes: &[u8]) -> Result<WarningLayer> {
    // Define the same serializable shapes locally for deserialization
    #[derive(Deserialize, Serialize)]
    struct SerializableWarningFeature {
        polygon: Vec<WorldPoint>,
        awareness_level: String,
        event: String,
        country_code: String,
        onset: Option<i64>,
        expires: Option<i64>,
    }
    #[derive(Deserialize, Serialize)]
    struct SerializableWarningLayer {
        features: Vec<SerializableWarningFeature>,
        updated_at: Option<i64>,
    }

    let cached: SerializableWarningLayer = serde_json::from_slice(bytes)
        .map_err(|e| color_eyre::eyre::eyre!("deserialize cache failed: {e}"))?;
    let features: Vec<WarningFeature> = cached
        .features
        .into_iter()
        .map(|sf| WarningFeature {
            polygon: sf.polygon,
            awareness_level: sf.awareness_level,
            event: sf.event,
            country_code: sf.country_code,
            onset: sf.onset,
            expires: sf.expires,
        })
        .collect();
    Ok(WarningLayer { features, updated_at: cached.updated_at })
}
