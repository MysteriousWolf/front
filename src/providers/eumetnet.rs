use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use chrono::{Duration as ChronoDuration, Utc};
use reqwest::Client;
use serde_json::{self, Value};

use crate::cache::{read_if_exists, write_atomic, write_log, FrontDirs};
use crate::config::EumetnetConfig;
use crate::geo::{
    lat_lon_to_world, world_to_lat_lon, Bounds, GeoPoint, WorldPoint, EUROPEAN_CAPITALS,
    EUROPEAN_MAJOR_CITIES,
};
use crate::layers::{ObservationLayer, ObservationPoint};

// ---------------------------------------------------------------------------
// Disk-cache helpers
// ---------------------------------------------------------------------------

/// Full layer (positions + values) cached for 5 min; serves pans/zooms.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DiskCacheEntry {
    bounds: Option<Bounds>,
    layer: ObservationLayer,
}

/// Station positions cached for 24 h; used to show placeholders on startup
/// without waiting for the slow /locations network call.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StationListEntry {
    bounds: Option<Bounds>,
    stations: Vec<StationInfo>,
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StationInfo {
    wigos_id: String,
    name: String,
    lon: f64,
    lat: f64,
}

/// One station parsed straight out of an `area` CoverageCollection: its WIGOS
/// id, position (from the coverage domain), and the values we care about.
struct AreaStation {
    wigos_id: String,
    lon: f64,
    lat: f64,
    values: HashMap<String, f64>,
}

/// Outcome of a single `area` (bbox) value fetch.
enum AreaFetch {
    /// Every reporting station inside the queried polygon.
    Ok(Vec<AreaStation>),
    /// Request completed but yielded nothing usable (parse error, no data,
    /// non-429 HTTP error, timeout).
    Empty,
    /// The gateway returned HTTP 429.  Carries the `X-RateLimit-Reset` value
    /// (seconds until the budget resets) when present.
    RateLimited(Option<u64>),
}

/// Default back-off when the gateway rate-limits us but sends no reset hint.
const RATE_LIMIT_DEFAULT_COOLDOWN: Duration = Duration::from_secs(60);
/// Cap on the back-off so a huge `X-RateLimit-Reset` can't freeze values for
/// an unreasonable stretch.
const RATE_LIMIT_MAX_COOLDOWN: Duration = Duration::from_secs(20 * 60);

/// How long the station-position list is valid on disk.  Positions rarely
/// change; caching for 24 h means the slow /locations fetch only happens
/// once per day.
const STATION_LIST_TTL: Duration = Duration::from_secs(24 * 3600);

/// Zoom below which only capital-city stations are fetched.
/// Must match `ui.rs` MAJOR_CITIES_ZOOM_CUTOFF.
const CAPITALS_ZOOM_CUTOFF: f64 = 5.5;

/// Half-side in degrees of the bbox queried around each capital/city.
/// 1.0° ≈ 111 km — matches the 100 km `CITY_MATCH_KM` display radius so
/// the fetch always covers every station the renderer might want to show.
const CAPITAL_BOX_DEG: f64 = 1.0;

/// How long a per-capital bbox result is reused before re-fetching.
/// Independent of viewport; shared across all zoom levels.
const CAPITAL_DATA_TTL: Duration = Duration::from_secs(5 * 60);

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// EUMETNET weather observations provider.
///
/// Uses the MeteoGate OGC EDR API to fetch near‑real‑time surface
/// observations (collection `observations`).
///
/// Fetch flow:
/// 1. Check in-memory layer cache (5 min TTL, handles pans/zooms).
/// 2. Check disk layer cache (5 min TTL, survives app restarts).
/// 3. Load the station list from disk (24 h TTL) for human-readable names,
///    fetching from `/locations` and saving if stale.
/// 4. A single `/area` bbox query returns every station reporting inside the
///    viewport, with positions and values, in one request.  The full set is
///    cached; `clip_layer_by_density` thins it to a readable number of labels
///    for the current zoom before rendering.
///
/// The gateway enforces a shared, unauthenticated request budget and answers
/// an exceeded budget with HTTP 429 and an HTML page.  When that happens the
/// fetch aborts and `rate_limited_until` is set, so subsequent refreshes skip
/// the query until the budget recovers instead of re-exhausting it.
type ObsCache = Arc<tokio::sync::Mutex<HashMap<(u64, u64), (Vec<ObservationPoint>, Instant)>>>;

#[derive(Debug, Clone)]
pub struct EumetnetProvider {
    client: Client,
    dirs: FrontDirs,
    config: EumetnetConfig,
    /// Viewport-level layer cache (5 min TTL).  Serves pans/zooms without
    /// hitting the API; keyed by collection id.
    mem_cache: Arc<tokio::sync::Mutex<HashMap<String, MemCacheEntry>>>,
    /// When set and in the future, the gateway has rate-limited us.
    rate_limited_until: Arc<tokio::sync::Mutex<Option<Instant>>>,
    /// Per-capital observation cache.  Key: (lat.to_bits(), lon.to_bits()).
    /// Value: points from that location's bbox query + the fetch instant.
    capital_cache: ObsCache,
    /// Same structure for major-city bbox queries (Phase 2a).
    city_cache: ObsCache,
}

#[derive(Debug, Clone)]
struct MemCacheEntry {
    fetched_at: Instant,
    bounds: Option<Bounds>,
    layer: ObservationLayer,
}

const MEM_CACHE_TTL: Duration = Duration::from_secs(300);

impl EumetnetProvider {
    pub fn new(client: Client, dirs: FrontDirs, config: EumetnetConfig) -> Self {
        Self {
            client,
            dirs,
            config,
            mem_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            rate_limited_until: Arc::new(tokio::sync::Mutex::new(None)),
            capital_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            city_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    // ------------------------------------------------------------------
    // Public API
    // ------------------------------------------------------------------

    pub async fn observations(
        &self,
        zoom: f64,
        bounds: Option<Bounds>,
        point_tx: tokio::sync::mpsc::UnboundedSender<ObservationPoint>,
        flush_tx: tokio::sync::mpsc::UnboundedSender<()>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let cache_path = self.dirs.cache_dir.join("eumetnet/surface-v2.json");
        let stations_path = self.dirs.cache_dir.join("eumetnet/surface-stations.json");
        self.fetch_observations(
            &cache_path,
            &stations_path,
            &self.config.surface_endpoint,
            "observations",
            zoom,
            bounds,
            point_tx,
            flush_tx,
        )
        .await
    }

    // ------------------------------------------------------------------
    // Core fetch logic
    // ------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    async fn fetch_observations(
        &self,
        cache_path: &Path,
        stations_cache_path: &Path,
        endpoint: &str,
        collection_id: &str,
        zoom: f64,
        bounds: Option<Bounds>,
        point_tx: tokio::sync::mpsc::UnboundedSender<ObservationPoint>,
        flush_tx: tokio::sync::mpsc::UnboundedSender<()>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let log = &self.dirs.log_path;
        let t_total = Instant::now();

        // Station names (24 h disk cache).
        let all_stations = self
            .fetch_station_list(stations_cache_path, endpoint, collection_id, log)
            .await;
        let names: HashMap<String, String> = all_stations
            .into_iter()
            .map(|s| (s.wigos_id, s.name))
            .collect();

        // `seen_wigos` deduplicates stations across all three phases.
        let mut seen_wigos: HashSet<String> = HashSet::new();

        // ── Phase 1: all European capitals (always, per-capital 5-min cache) ──
        self.fetch_location_batch(
            endpoint,
            collection_id,
            EUROPEAN_CAPITALS,
            "capitals",
            &names,
            log,
            &point_tx,
            &self.capital_cache,
            &mut seen_wigos,
        )
        .await;
        // Commit Phase 1 to the UI immediately so capitals appear fast.
        let _ = flush_tx.send(());

        // ── Phase 2a: major cities (always, per-city 5-min cache) ──────────
        // Provides more uniform station coverage across all zoom levels without
        // the slowness of a full continent-wide area query.
        self.fetch_location_batch(
            endpoint,
            collection_id,
            EUROPEAN_MAJOR_CITIES,
            "cities",
            &names,
            log,
            &point_tx,
            &self.city_cache,
            &mut seen_wigos,
        )
        .await;
        // Commit Phase 2a so cities appear before the full viewport loads.
        let _ = flush_tx.send(());

        // ── Phase 2b: full viewport (only when zoomed in enough) ───────────
        if zoom >= CAPITALS_ZOOM_CUTOFF {
            let expanded_bounds = bounds.map(|b| b.expanded(0.5));

            let cache_hit = {
                let cache = self.mem_cache.lock().await;
                cache.get(collection_id).and_then(|e| {
                    if e.fetched_at.elapsed() < MEM_CACHE_TTL
                        && bounds_covered(e.bounds, expanded_bounds)
                    {
                        Some(e.layer.points.clone())
                    } else {
                        None
                    }
                })
            };

            let viewport_points = if let Some(pts) = cache_hit {
                write_log(
                    log,
                    format!("eumetnet: viewport mem cache hit ({} pts)", pts.len()),
                );
                pts
            } else {
                let disk_hit =
                    Self::load_disk_cache::<DiskCacheEntry>(cache_path, MEM_CACHE_TTL.as_secs())
                        .and_then(|e| {
                            if bounds_covered(e.bounds, expanded_bounds) {
                                Some(e.layer.points)
                            } else {
                                None
                            }
                        });
                if let Some(pts) = disk_hit {
                    write_log(
                        log,
                        format!("eumetnet: viewport disk cache hit ({} pts)", pts.len()),
                    );
                    pts
                } else {
                    let polygon = bounds_polygon(expanded_bounds);
                    let pts = Self::fetch_area_points(
                        &self.client,
                        endpoint,
                        collection_id,
                        &polygon,
                        &names,
                        Arc::clone(&self.rate_limited_until),
                        log,
                    )
                    .await;
                    if !pts.is_empty() {
                        let layer = ObservationLayer {
                            points: pts.clone(),
                            updated_at: Some(
                                SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs() as i64)
                                    .unwrap_or(0),
                            ),
                        };
                        if let Ok(json) = serde_json::to_vec(&DiskCacheEntry {
                            bounds: expanded_bounds,
                            layer: layer.clone(),
                        }) {
                            let _ = write_atomic(cache_path, &json);
                        }
                        let mut cache = self.mem_cache.lock().await;
                        cache.insert(
                            collection_id.to_string(),
                            MemCacheEntry {
                                fetched_at: Instant::now(),
                                bounds: expanded_bounds,
                                layer,
                            },
                        );
                    }
                    pts
                }
            };

            for pt in viewport_points {
                if seen_wigos.insert(pt.station_id.clone()) {
                    let _ = point_tx.send(pt);
                }
            }
        }

        write_log(
            log,
            format!(
                "eumetnet: done ({collection_id}, zoom={zoom:.1}, {:.2}s)",
                t_total.elapsed().as_secs_f64()
            ),
        );
        Ok(())
    }

    /// Fetch observation data for a list of named geographic positions (capitals
    /// or major cities) using per-position 5-min caching.  Fresh entries are
    /// served from `cache` immediately; stale ones are queried concurrently via
    /// individual small bbox area requests.  Results are streamed to `point_tx`
    /// and deduplicated against `seen_wigos`.
    #[allow(clippy::too_many_arguments)]
    async fn fetch_location_batch(
        &self,
        endpoint: &str,
        collection_id: &str,
        locations: &[(f64, f64)],
        label: &str,
        names: &HashMap<String, String>,
        log: &Path,
        point_tx: &tokio::sync::mpsc::UnboundedSender<ObservationPoint>,
        cache: &ObsCache,
        seen_wigos: &mut HashSet<String>,
    ) {
        // Rate-limit pre-check: serve stale cache rather than hammering the API.
        {
            let until = self.rate_limited_until.lock().await;
            if let Some(t) = *until {
                if Instant::now() < t {
                    write_log(
                        log,
                        format!("eumetnet: {label} — rate-limited, serving cached"),
                    );
                    let cache = cache.lock().await;
                    for &(lat, lon) in locations {
                        let key = (lat.to_bits(), lon.to_bits());
                        if let Some((pts, _)) = cache.get(&key) {
                            for pt in pts {
                                if seen_wigos.insert(pt.station_id.clone()) {
                                    let _ = point_tx.send(pt.clone());
                                }
                            }
                        }
                    }
                    return;
                }
            }
        }

        let fetch_instant = Instant::now();
        let mut stale: Vec<(f64, f64)> = Vec::new();

        // Stream fresh cached entries immediately; collect stale positions.
        {
            let cache = cache.lock().await;
            for &(lat, lon) in locations {
                let key = (lat.to_bits(), lon.to_bits());
                match cache.get(&key) {
                    Some((pts, t)) if t.elapsed() < CAPITAL_DATA_TTL => {
                        for pt in pts {
                            if seen_wigos.insert(pt.station_id.clone()) {
                                let _ = point_tx.send(pt.clone());
                            }
                        }
                    }
                    _ => stale.push((lat, lon)),
                }
            }
        }

        if stale.is_empty() {
            return;
        }

        write_log(
            log,
            format!(
                "eumetnet: {label} — fetching {}/{} stale",
                stale.len(),
                locations.len()
            ),
        );

        let now_utc = Utc::now();
        let datetime = format!(
            "{}/{}",
            (now_utc - ChronoDuration::hours(1)).format("%Y-%m-%dT%H:%M:%SZ"),
            now_utc.format("%Y-%m-%dT%H:%M:%SZ"),
        );

        let polygons: Vec<String> = stale.iter().map(|&(lat, lon)| {
            let (lat0, lat1) = (lat - CAPITAL_BOX_DEG, lat + CAPITAL_BOX_DEG);
            let (lon0, lon1) = (lon - CAPITAL_BOX_DEG, lon + CAPITAL_BOX_DEG);
            format!("POLYGON(({lon0} {lat0},{lon1} {lat0},{lon1} {lat1},{lon0} {lat1},{lon0} {lat0}))")
        }).collect();

        let futs = polygons.iter().map(|poly| {
            Self::fetch_area_values(&self.client, endpoint, collection_id, &datetime, poly, log)
        });
        let results = futures::future::join_all(futs).await;

        let mut rate_limit_cooldown: Option<Duration> = None;

        {
            let mut cache = cache.lock().await;
            for (fetch, &(clat, clon)) in results.into_iter().zip(stale.iter()) {
                let key = (clat.to_bits(), clon.to_bits());
                match fetch {
                    AreaFetch::Ok(stations) => {
                        let pts: Vec<ObservationPoint> = stations
                            .into_iter()
                            .map(|s| ObservationPoint {
                                station_id: names
                                    .get(&s.wigos_id)
                                    .cloned()
                                    .unwrap_or_else(|| s.wigos_id.clone()),
                                point: GeoPoint::new(s.lon, s.lat),
                                world: lat_lon_to_world(s.lat, s.lon),
                                temperature: s.values.get("air_temperature").copied(),
                                wind_speed: s.values.get("wind_speed").copied(),
                                wind_direction: s.values.get("wind_from_direction").copied(),
                                humidity: s.values.get("relative_humidity").copied(),
                                pressure: s.values.get("air_pressure_at_mean_sea_level").copied(),
                            })
                            .collect();
                        cache.insert(key, (pts.clone(), fetch_instant));
                        for pt in pts {
                            if seen_wigos.insert(pt.station_id.clone()) {
                                let _ = point_tx.send(pt);
                            }
                        }
                    }
                    AreaFetch::Empty => {
                        cache.insert(key, (Vec::new(), fetch_instant));
                    }
                    AreaFetch::RateLimited(reset) => {
                        let cooldown = reset
                            .map(Duration::from_secs)
                            .unwrap_or(RATE_LIMIT_DEFAULT_COOLDOWN)
                            .min(RATE_LIMIT_MAX_COOLDOWN);
                        rate_limit_cooldown = Some(cooldown);
                    }
                }
            }
        }

        if let Some(cooldown) = rate_limit_cooldown {
            write_log(
                log,
                format!(
                    "eumetnet: rate limited on {label} — backing off {}s",
                    cooldown.as_secs()
                ),
            );
            let mut until = self.rate_limited_until.lock().await;
            *until = Some(Instant::now() + cooldown);
        }
    }

    // ------------------------------------------------------------------
    // Station list (positions) with 24 h disk cache
    // ------------------------------------------------------------------

    /// Return the full station list for this collection, using a 24 h disk
    /// cache.  No bbox is sent so the API always returns every station —
    /// viewport filtering is done client-side in `fetch_observations`.
    async fn fetch_station_list(
        &self,
        stations_cache_path: &Path,
        endpoint: &str,
        collection_id: &str,
        log: &Path,
    ) -> Vec<StationInfo> {
        // Try 24 h disk cache first — skip entries with zero stations (poisoned).
        let stale_cache: Option<Vec<StationInfo>> =
            Self::load_disk_cache::<StationListEntry>(stations_cache_path, u64::MAX)
                .filter(|e| !e.stations.is_empty())
                .map(|e| e.stations);

        // Cache hit: any non-empty entry saved with bounds = None is global.
        if let Some(entry) = Self::load_disk_cache::<StationListEntry>(
            stations_cache_path,
            STATION_LIST_TTL.as_secs(),
        ) {
            if !entry.stations.is_empty() && entry.bounds.is_none() {
                write_log(
                    log,
                    format!(
                        "eumetnet: station list cache hit ({} stations)",
                        entry.stations.len()
                    ),
                );
                return entry.stations;
            }
        }

        // Cache miss or stale bounds-scoped entry — fetch the full collection.
        let url = format!("{endpoint}/collections/{collection_id}/locations");
        let t = Instant::now();
        write_log(log, format!("eumetnet: fetching station list {url}"));

        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                write_log(log, format!("eumetnet: locations request error: {e}"));
                return stale_cache.unwrap_or_default();
            }
        };

        if !resp.status().is_success() {
            write_log(log, format!("eumetnet: locations HTTP {}", resp.status()));
            return stale_cache.unwrap_or_default();
        }

        let text = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                write_log(log, format!("eumetnet: locations body error: {e}"));
                return stale_cache.unwrap_or_default();
            }
        };

        write_log(
            log,
            format!(
                "eumetnet: station list fetched in {:.2}s ({} bytes)",
                t.elapsed().as_secs_f64(),
                text.len()
            ),
        );

        let stations = match Self::parse_locations(&text, log) {
            Some(s) if !s.is_empty() => s,
            Some(_) => {
                write_log(
                    log,
                    format!(
                        "eumetnet: parse_locations returned 0 stations (body prefix: {})",
                        &text[..text.len().min(200)]
                    ),
                );
                return stale_cache.unwrap_or_default();
            }
            None => {
                write_log(
                    log,
                    format!(
                        "eumetnet: parse_locations failed (body prefix: {})",
                        &text[..text.len().min(200)]
                    ),
                );
                return stale_cache.unwrap_or_default();
            }
        };

        // Persist with bounds = None — valid for any viewport.
        if let Ok(json) = serde_json::to_vec(&StationListEntry {
            bounds: None,
            stations: stations.clone(),
        }) {
            let _ = write_atomic(stations_cache_path, &json);
        }

        stations
    }

    // ------------------------------------------------------------------
    // Disk cache helper
    // ------------------------------------------------------------------

    fn load_disk_cache<T: serde::de::DeserializeOwned>(
        path: &Path,
        max_age_secs: u64,
    ) -> Option<T> {
        let bytes = read_if_exists(path).ok()??;
        let age = std::fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| SystemTime::now().duration_since(t).ok())?;
        if age.as_secs() >= max_age_secs {
            return None;
        }
        serde_json::from_slice(&bytes).ok()
    }

    // ------------------------------------------------------------------
    // Locations parser
    // ------------------------------------------------------------------

    /// Parse the `/locations` GeoJSON FeatureCollection into a station list.
    ///
    /// OGC EDR feature shape:
    ///   { "id": "<wigos_id>", "geometry": { "type": "Point", … },
    ///     "properties": { "name": "<human name>", … } }
    fn parse_locations(text: &str, log: &Path) -> Option<Vec<StationInfo>> {
        if text.trim().is_empty() {
            return Some(Vec::new());
        }
        let v: Value = serde_json::from_str(text).ok()?;

        if let Some(first) = v
            .get("features")
            .and_then(|f| f.as_array())
            .and_then(|a| a.first())
        {
            write_log(
                log,
                format!(
                    "eumetnet: location feature keys: {:?}",
                    first.as_object().map(|o| o.keys().collect::<Vec<_>>())
                ),
            );
            if let Some(props) = first.get("properties").and_then(|p| p.as_object()) {
                write_log(
                    log,
                    format!(
                        "eumetnet: location properties keys: {:?}",
                        props.keys().collect::<Vec<_>>()
                    ),
                );
            }
        }

        let features = v.get("features")?.as_array()?;
        let mut stations = Vec::with_capacity(features.len());

        for feat in features {
            let props = feat.get("properties").and_then(|p| p.as_object());

            // wigos_id: prefer top-level "id", fall back to properties.platform
            let wigos_id = feat.get("id").and_then(|v| v.as_str()).or_else(|| {
                props
                    .and_then(|p| p.get("platform"))
                    .and_then(|v| v.as_str())
            });
            let wigos_id = match wigos_id {
                Some(s) => s,
                None => continue,
            };

            // Human-readable name: "name" key (MeteoGate /locations), then
            // "title" and "platform_name" as fallbacks.
            let name = props
                .and_then(|p| {
                    p.get("name")
                        .or_else(|| p.get("title"))
                        .or_else(|| p.get("platform_name"))
                        .and_then(|v| v.as_str())
                })
                .unwrap_or(wigos_id);

            let geom = match feat.get("geometry") {
                Some(g) => g,
                None => continue,
            };
            if geom.get("type").and_then(|t| t.as_str()) != Some("Point") {
                continue;
            }
            let coords = match geom.get("coordinates").and_then(|c| c.as_array()) {
                Some(c) if c.len() >= 2 => c,
                _ => continue,
            };

            stations.push(StationInfo {
                wigos_id: wigos_id.to_string(),
                name: normalize_station_name(name),
                lon: coords[0].as_f64().unwrap_or(0.0),
                lat: coords[1].as_f64().unwrap_or(0.0),
            });
        }

        Some(stations)
    }

    // ------------------------------------------------------------------
    // Observation values via a single `area` query
    // ------------------------------------------------------------------

    /// Fetch every station reporting inside `polygon` in one `area` query and
    /// turn each into an `ObservationPoint` (position and values from the
    /// response, name looked up in `names`).  Returns the full set; the caller
    /// caches it and density-clips for display.
    async fn fetch_area_points(
        client: &Client,
        endpoint: &str,
        collection_id: &str,
        polygon: &str,
        names: &HashMap<String, String>,
        rate_limited_until: Arc<tokio::sync::Mutex<Option<Instant>>>,
        log: &Path,
    ) -> Vec<ObservationPoint> {
        // If the gateway recently rate-limited us, skip the query entirely so
        // the shared budget can recover instead of being re-exhausted.
        {
            let until = rate_limited_until.lock().await;
            if let Some(t) = *until {
                let now = Instant::now();
                if now < t {
                    write_log(
                        log,
                        format!(
                            "eumetnet: skipping area query — rate-limited for {}s more",
                            (t - now).as_secs(),
                        ),
                    );
                    return Vec::new();
                }
            }
        }

        // Use a *closed* datetime interval covering the last hour — never the
        // open-ended `.../..`. The gateway federates each `area` query out to
        // national sources and an open end makes several of them choke (Latvia
        // computes the span as millions of days and 413s, Poland rejects the
        // format), so only the one or two sources that tolerate it answer —
        // which left most of the continent blank.
        //
        // A 1h window is the sweet spot: long enough to catch the latest reading
        // from even hourly-reporting (synoptic) stations, short enough to keep
        // the payload manageable. There is no "latest only" mode — a single
        // instant 404s and an unbounded range 413s — and we keep only the most
        // recent value per parameter anyway (see `extract_param_values`).
        let now = Utc::now();
        let datetime = format!(
            "{}/{}",
            (now - ChronoDuration::hours(1)).format("%Y-%m-%dT%H:%M:%SZ"),
            now.format("%Y-%m-%dT%H:%M:%SZ"),
        );
        let stations =
            match Self::fetch_area_values(client, endpoint, collection_id, &datetime, polygon, log)
                .await
            {
                AreaFetch::Ok(stations) => stations,
                AreaFetch::Empty => Vec::new(),
                AreaFetch::RateLimited(reset) => {
                    let cooldown = reset
                        .map(Duration::from_secs)
                        .unwrap_or(RATE_LIMIT_DEFAULT_COOLDOWN)
                        .min(RATE_LIMIT_MAX_COOLDOWN);
                    write_log(
                        log,
                        format!(
                            "eumetnet: rate limited (HTTP 429) — backing off {}s",
                            cooldown.as_secs(),
                        ),
                    );
                    let mut until = rate_limited_until.lock().await;
                    *until = Some(Instant::now() + cooldown);
                    return Vec::new();
                }
            };

        stations
            .into_iter()
            .map(|s| ObservationPoint {
                point: GeoPoint::new(s.lon, s.lat),
                world: lat_lon_to_world(s.lat, s.lon),
                station_id: names.get(&s.wigos_id).cloned().unwrap_or(s.wigos_id),
                temperature: s.values.get("air_temperature").copied(),
                wind_speed: s.values.get("wind_speed").copied(),
                wind_direction: s.values.get("wind_from_direction").copied(),
                humidity: s.values.get("relative_humidity").copied(),
                pressure: s.values.get("air_pressure_at_mean_sea_level").copied(),
            })
            .collect()
    }

    /// Fetch observation values for every station inside `polygon` in a single
    /// EDR `area` query.
    async fn fetch_area_values(
        client: &Client,
        endpoint: &str,
        collection_id: &str,
        datetime: &str,
        polygon: &str,
        log: &Path,
    ) -> AreaFetch {
        let url = format!("{endpoint}/collections/{collection_id}/area");
        // A continent-wide surface `area` response can be tens of MB and take the
        // gateway ~40s to assemble, so allow generous headroom while still staying
        // under the 120s global client timeout.
        let fetch = async {
            let r = client
                .get(&url)
                .query(&[("coords", polygon), ("datetime", datetime)])
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = r.status();
            let reset = r
                .headers()
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            let text = r.text().await.map_err(|e| e.to_string())?;
            Ok::<_, String>((status, reset, text))
        };
        let (status, reset, text) = match tokio::time::timeout(Duration::from_secs(90), fetch).await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                write_log(log, format!("eumetnet: area request error: {e}"));
                return AreaFetch::Empty;
            }
            Err(_) => {
                write_log(log, "eumetnet: area request timed out".to_string());
                return AreaFetch::Empty;
            }
        };
        // The gateway answers an exceeded budget with an HTML rate-limit page,
        // not JSON.  Detect it explicitly so we back off instead of misreporting
        // it as a parse error and re-hammering on the next refresh.
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return AreaFetch::RateLimited(reset);
        }
        if !status.is_success() {
            write_log(
                log,
                format!(
                    "eumetnet: area HTTP {} (body: {})",
                    status,
                    &text[..text.len().min(200)]
                ),
            );
            return AreaFetch::Empty;
        }
        let data: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                write_log(log, format!("eumetnet: area parse error: {e}"));
                return AreaFetch::Empty;
            }
        };
        AreaFetch::Ok(parse_area_values(&data))
    }
}

// ---------------------------------------------------------------------------
// CoverageJSON value extraction
// ---------------------------------------------------------------------------

/// Parse an EDR `area` CoverageCollection into one `AreaStation` per coverage.
/// Each coverage is a station: its id is `metocean:wigosId` (which matches the
/// `locations` feature id), its position is the single point in the coverage
/// domain `x`/`y` axes, and its values come from the `ranges`.  Coverages with
/// no id, no position, or no usable values are skipped.
fn parse_area_values(data: &Value) -> Vec<AreaStation> {
    let mut out: Vec<AreaStation> = Vec::new();
    let Some(coverages) = data.get("coverages").and_then(|c| c.as_array()) else {
        return out;
    };
    for cov in coverages {
        let Some(id) = cov.get("metocean:wigosId").and_then(|v| v.as_str()) else {
            continue;
        };
        let axes = cov.get("domain").and_then(|d| d.get("axes"));
        let lon = axes
            .and_then(|a| a.get("x"))
            .and_then(|x| x.get("values"))
            .and_then(|v| v.as_array())
            .and_then(|v| v.first())
            .and_then(|v| v.as_f64());
        let lat = axes
            .and_then(|a| a.get("y"))
            .and_then(|y| y.get("values"))
            .and_then(|v| v.as_array())
            .and_then(|v| v.first())
            .and_then(|v| v.as_f64());
        let (Some(lon), Some(lat)) = (lon, lat) else {
            continue;
        };
        let Some(ranges) = cov.get("ranges").and_then(|r| r.as_object()) else {
            continue;
        };
        let mut values = HashMap::new();
        extract_param_values(ranges, &mut values);
        if !values.is_empty() {
            out.push(AreaStation {
                wigos_id: id.to_string(),
                lon,
                lat,
                values,
            });
        }
    }
    out
}

/// Build a WKT `POLYGON` (lon/lat) for the `coords` parameter of an `area`
/// query covering `bounds` (world coordinates).  Falls back to a Europe-wide
/// box when no viewport bounds are given.
fn bounds_polygon(bounds: Option<Bounds>) -> String {
    let Some(b) = bounds else {
        return "POLYGON((-30 30,45 30,45 72,-30 72,-30 30))".to_string();
    };
    // World y increases southward, so min_y is the northern edge.
    let nw = world_to_lat_lon(WorldPoint {
        x: b.min_x,
        y: b.min_y,
    });
    let se = world_to_lat_lon(WorldPoint {
        x: b.max_x,
        y: b.max_y,
    });
    let (lon_min, lon_max) = (nw.lon, se.lon);
    let (lat_min, lat_max) = (se.lat, nw.lat);
    format!(
        "POLYGON(({lon_min} {lat_min},{lon_max} {lat_min},{lon_max} {lat_max},{lon_min} {lat_max},{lon_min} {lat_min}))"
    )
}

fn extract_param_values(ranges: &serde_json::Map<String, Value>, out: &mut HashMap<String, f64>) {
    for (param_key, range) in ranges {
        let std_name = match param_key.split(':').next() {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };
        let values = match range.get("values").and_then(|v| v.as_array()) {
            Some(v) if !v.is_empty() => v,
            _ => continue,
        };
        let latest = match values.last().and_then(|v| v.as_f64()) {
            Some(v) => v,
            None => continue,
        };
        if std_name == "wind_speed" {
            let is_mean = param_key.contains(":mean:");
            if out.get(std_name).is_none() || is_mean {
                out.insert(std_name.to_string(), latest);
            }
        } else {
            out.entry(std_name.to_string()).or_insert(latest);
        }
    }
}

// ---------------------------------------------------------------------------
// Bounds coverage helper
// ---------------------------------------------------------------------------

fn bounds_covered(have: Option<Bounds>, want: Option<Bounds>) -> bool {
    match (have, want) {
        (None, _) => true,
        (Some(h), Some(w)) => h.contains(w),
        (Some(_), None) => false,
    }
}

/// Normalize a raw API station name for display.
///
/// Replaces underscores with spaces. Converts ALL_CAPS names to title case
/// so Austrian/German identifiers like "BAD_TATZMANNSDORF" render as
/// "Bad Tatzmannsdorf" while mixed-case names (e.g. Hungarian "Szombathely")
/// are left unchanged.
fn normalize_station_name(name: &str) -> String {
    let spaced = name.replace('_', " ");
    let has_lower = spaced.chars().any(|c| c.is_lowercase());
    if has_lower {
        return spaced;
    }
    spaced
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => {
                    let upper: String = first.to_uppercase().collect();
                    let rest: String = chars.collect::<String>().to_lowercase();
                    upper + &rest
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── normalize_station_name ─────────────────────────────────────────

    #[test]
    fn test_normalize_station_name_all_caps_to_title_case() {
        assert_eq!(
            normalize_station_name("BAD_TATZMANNSDORF"),
            "Bad Tatzmannsdorf"
        );
    }

    #[test]
    fn test_normalize_station_name_mixed_case_unchanged() {
        assert_eq!(normalize_station_name("Szombathely"), "Szombathely");
    }

    #[test]
    fn test_normalize_station_name_underscores_replaced_when_mixed_case() {
        assert_eq!(normalize_station_name("Wien_Hohe_Warte"), "Wien Hohe Warte");
    }

    #[test]
    fn test_normalize_station_name_single_word_all_caps() {
        assert_eq!(normalize_station_name("BERLIN"), "Berlin");
    }

    #[test]
    fn test_normalize_station_name_empty() {
        assert_eq!(normalize_station_name(""), "");
    }

    // ── bounds_covered ─────────────────────────────────────────────────

    #[test]
    fn test_bounds_covered_none_have_always_true() {
        // When we have no stored bounds (None = global), anything is covered.
        let want = Some(Bounds {
            min_x: 0.1,
            max_x: 0.9,
            min_y: 0.1,
            max_y: 0.9,
        });
        assert!(bounds_covered(None, want));
        assert!(bounds_covered(None, None));
    }

    #[test]
    fn test_bounds_covered_have_subset_of_want_is_false() {
        let have = Some(Bounds {
            min_x: 0.2,
            max_x: 0.8,
            min_y: 0.2,
            max_y: 0.8,
        });
        let want = Some(Bounds {
            min_x: 0.1,
            max_x: 0.9,
            min_y: 0.1,
            max_y: 0.9,
        });
        // `have` does not fully contain `want` → not covered.
        assert!(!bounds_covered(have, want));
    }

    #[test]
    fn test_bounds_covered_have_superset_of_want_is_true() {
        let have = Some(Bounds {
            min_x: 0.0,
            max_x: 1.0,
            min_y: 0.0,
            max_y: 1.0,
        });
        let want = Some(Bounds {
            min_x: 0.2,
            max_x: 0.8,
            min_y: 0.2,
            max_y: 0.8,
        });
        assert!(bounds_covered(have, want));
    }

    #[test]
    fn test_bounds_covered_have_some_want_none_is_false() {
        let have = Some(Bounds {
            min_x: 0.0,
            max_x: 1.0,
            min_y: 0.0,
            max_y: 1.0,
        });
        // A bounded cache cannot satisfy a "no bounds = global" request.
        assert!(!bounds_covered(have, None));
    }

    // ── bounds_polygon ─────────────────────────────────────────────────

    #[test]
    fn test_bounds_polygon_none_returns_europe_fallback() {
        let poly = bounds_polygon(None);
        assert!(poly.starts_with("POLYGON"), "must be a WKT polygon");
        assert!(poly.contains("-30"), "Europe fallback spans to -30°");
    }

    #[test]
    fn test_bounds_polygon_some_bounds_contains_lon_lat() {
        // World centre (0.5, 0.5) in world coords → (0°N, 0°E) in lat/lon.
        let b = Bounds {
            min_x: 0.4,
            max_x: 0.6,
            min_y: 0.4,
            max_y: 0.6,
        };
        let poly = bounds_polygon(Some(b));
        assert!(poly.starts_with("POLYGON"), "must be a WKT polygon");
        // The polygon must close (last point == first point).
        let inner = poly.trim_start_matches("POLYGON((").trim_end_matches("))");
        let points: Vec<&str> = inner.split(',').collect();
        assert_eq!(
            points.first().unwrap().trim(),
            points.last().unwrap().trim(),
            "polygon must be closed"
        );
    }

    // ── extract_param_values ───────────────────────────────────────────

    #[test]
    fn test_extract_param_values_picks_latest_value() {
        let ranges = json!({
            "air_temperature": { "values": [10.0, 12.0, 15.0] }
        });
        let mut out = HashMap::new();
        extract_param_values(ranges.as_object().unwrap(), &mut out);
        assert!((out["air_temperature"] - 15.0).abs() < 1e-9);
    }

    #[test]
    fn test_extract_param_values_wind_speed_prefers_mean() {
        // Two wind_speed entries: one plain, one :mean: — the mean wins.
        let ranges = json!({
            "wind_speed:max:PT10M": { "values": [5.0] },
            "wind_speed:mean:PT10M": { "values": [3.0] }
        });
        let mut out = HashMap::new();
        extract_param_values(ranges.as_object().unwrap(), &mut out);
        assert!((out["wind_speed"] - 3.0).abs() < 1e-9, "mean should win");
    }

    #[test]
    fn test_extract_param_values_skips_null_values() {
        let ranges = json!({
            "air_temperature": { "values": [null] }
        });
        let mut out = HashMap::new();
        extract_param_values(ranges.as_object().unwrap(), &mut out);
        assert!(
            !out.contains_key("air_temperature"),
            "null value must be skipped"
        );
    }

    // ── parse_area_values ─────────────────────────────────────────────

    #[test]
    fn test_parse_area_values_basic_coverage() {
        let data = json!({
            "coverages": [{
                "metocean:wigosId": "0-20000-0-11035",
                "domain": {
                    "axes": {
                        "x": { "values": [14.51] },
                        "y": { "values": [46.05] }
                    }
                },
                "ranges": {
                    "air_temperature": { "values": [22.5] }
                }
            }]
        });
        let result = parse_area_values(&data);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].wigos_id, "0-20000-0-11035");
        assert!((result[0].lon - 14.51).abs() < 1e-9);
        assert!((result[0].lat - 46.05).abs() < 1e-9);
        assert!((result[0].values["air_temperature"] - 22.5).abs() < 1e-9);
    }

    #[test]
    fn test_parse_area_values_skips_coverage_without_id() {
        let data = json!({
            "coverages": [{
                "domain": { "axes": { "x": { "values": [0.0] }, "y": { "values": [0.0] } } },
                "ranges": { "air_temperature": { "values": [10.0] } }
            }]
        });
        assert!(parse_area_values(&data).is_empty());
    }

    #[test]
    fn test_parse_area_values_skips_empty_values() {
        let data = json!({
            "coverages": [{
                "metocean:wigosId": "test",
                "domain": { "axes": { "x": { "values": [1.0] }, "y": { "values": [2.0] } } },
                "ranges": { "air_temperature": { "values": [] } }
            }]
        });
        // Empty values → no usable params → station skipped.
        assert!(parse_area_values(&data).is_empty());
    }

    #[test]
    fn test_parse_area_values_no_coverages_key() {
        let data = json!({ "type": "CoverageCollection" });
        assert!(parse_area_values(&data).is_empty());
    }

    // ── parse_locations ────────────────────────────────────────────────

    #[test]
    fn test_parse_locations_empty_text_returns_empty_vec() {
        let result = EumetnetProvider::parse_locations("", std::path::Path::new("/dev/null"));
        assert!(result.is_some_and(|v| v.is_empty()));
    }

    #[test]
    fn test_parse_locations_basic_feature() {
        let json = r#"{
          "features": [{
            "id": "0-20000-0-11035",
            "geometry": { "type": "Point", "coordinates": [14.51, 46.05] },
            "properties": { "name": "Ljubljana" }
          }]
        }"#;
        let result =
            EumetnetProvider::parse_locations(json, std::path::Path::new("/dev/null")).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].wigos_id, "0-20000-0-11035");
        assert_eq!(result[0].name, "Ljubljana");
        assert!((result[0].lon - 14.51).abs() < 1e-9);
        assert!((result[0].lat - 46.05).abs() < 1e-9);
    }

    #[test]
    fn test_parse_locations_falls_back_to_platform_id() {
        let json = r#"{
          "features": [{
            "geometry": { "type": "Point", "coordinates": [0.0, 0.0] },
            "properties": { "platform": "fallback-id", "name": "Test" }
          }]
        }"#;
        let result =
            EumetnetProvider::parse_locations(json, std::path::Path::new("/dev/null")).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].wigos_id, "fallback-id");
    }

    #[test]
    fn test_parse_locations_skips_non_point_geometry() {
        let json = r#"{
          "features": [{
            "id": "poly-station",
            "geometry": { "type": "Polygon", "coordinates": [] },
            "properties": {}
          }]
        }"#;
        let result =
            EumetnetProvider::parse_locations(json, std::path::Path::new("/dev/null")).unwrap();
        assert!(result.is_empty(), "non-Point geometry must be skipped");
    }

    #[test]
    fn test_parse_locations_skips_feature_without_id() {
        let json = r#"{
          "features": [{
            "geometry": { "type": "Point", "coordinates": [0.0, 0.0] },
            "properties": {}
          }]
        }"#;
        let result =
            EumetnetProvider::parse_locations(json, std::path::Path::new("/dev/null")).unwrap();
        assert!(result.is_empty(), "feature without id must be skipped");
    }

    #[test]
    fn test_parse_locations_name_title_platform_name_fallback_chain() {
        // No "name" key — should fall back to "title".
        let json = r#"{
          "features": [{
            "id": "S1",
            "geometry": { "type": "Point", "coordinates": [1.0, 2.0] },
            "properties": { "title": "My Title" }
          }]
        }"#;
        let result =
            EumetnetProvider::parse_locations(json, std::path::Path::new("/dev/null")).unwrap();
        assert_eq!(result[0].name, "My Title");
    }
}
