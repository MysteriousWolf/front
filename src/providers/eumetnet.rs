use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use chrono::{Duration as ChronoDuration, Utc};
use futures::stream::{FuturesUnordered, StreamExt};
use reqwest::Client;
use serde_json::{self, Value};
use tokio::sync::Semaphore;

use crate::cache::{read_if_exists, write_atomic, write_log, FrontDirs};
use crate::config::EumetnetConfig;
use crate::geo::{
    lat_lon_to_world, world_to_lat_lon, Bounds, GeoPoint, WorldPoint, EUROPEAN_CAPITALS,
    EUROPEAN_MAJOR_CITIES, EUROPE_LAT, EUROPE_LON, OBS_TIER_ZOOM_CUTOFF,
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
    /// Not sent at all — our own hourly budget is spent.  Distinct from
    /// `Empty` so the caller does not cache an absence it never verified.
    OverBudget,
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

/// Side length of the regional cells the capital/city list is clustered into
/// before fetching.
///
/// The layer used to issue one 1° box per capital and major city: 113 requests
/// for a full refresh, against a gateway quota of 50/hour.  It could never fit,
/// and the budget would be gone before the first dozen cities were covered —
/// which is what made observations appear at random.
///
/// Snapping those 113 points onto a 12° grid collapses them to ~16 distinct
/// cells, and one `area` query per cell returns *every* station inside it, not
/// just the ones near a city.  Fewer requests and better coverage at once.
const REGION_CELL_DEG: f64 = 12.0;

/// `true` when the regional backdrop (capitals + major cities, clustered
/// into `REGION_CELL_DEG` cells) should be fetched at this zoom.
///
/// Below `OBS_TIER_ZOOM_CUTOFF` the viewport query is skipped (too far out
/// to be useful) and the backdrop is the only source. At/above it the
/// viewport query covers what's visible and the backdrop would just be
/// paying for continental cells the user has zoomed past — that was the
/// budget leak this predicate closes.
fn should_fetch_backdrop(zoom: f64) -> bool {
    zoom < OBS_TIER_ZOOM_CUTOFF
}

/// Distinct regional cell centres covering `points`.
///
/// Points are snapped to a `cell_deg` grid and de-duplicated, so nearby cities
/// share one query instead of each paying for its own.
fn region_cells(points: &[(f64, f64)], cell_deg: f64) -> Vec<(f64, f64)> {
    let mut seen: HashSet<(i64, i64)> = HashSet::new();
    let mut out = Vec::new();
    for &(lat, lon) in points {
        let la = (lat / cell_deg).round();
        let lo = (lon / cell_deg).round();
        if seen.insert((la as i64, lo as i64)) {
            out.push((la * cell_deg, lo * cell_deg));
        }
    }
    out
}

/// How long a per-capital bbox result is reused before re-fetching.
///
/// Deliberately long.  There are ~114 capitals and major cities, one request
/// each, against a gateway budget of 50 requests/hour for anonymous callers —
/// the old 5 min TTL implied ~1400 requests/hour, roughly 27× over, which is
/// why the layer used to wedge itself into a permanent 429.  These boxes are
/// low-zoom context, not the data you are reading; the viewport stations you
/// are actually looking at refresh on [`VIEWPORT_DATA_TTL`] instead.
const CAPITAL_DATA_TTL: Duration = Duration::from_secs(6 * 3600);

/// Max concurrent per-location HTTP requests.
///
/// The gateway allows a burst of 20 for anonymous callers; 32 in flight at
/// once overran it on its own, independently of the hourly quota.  Eight keeps
/// us clear of the burst ceiling while still filling the map quickly, and
/// combined with centre-outward sorting the nearest stations start first.
const MAX_CONCURRENT_LOCATIONS: usize = 8;

/// Fraction of the published hourly quota front will actually spend.
///
/// The remainder is headroom: the quota is shared per client IP, so another
/// tool (or a second front instance) on the same address must not be able to
/// push us over simply because we sized ourselves to exactly 100%.
const BUDGET_UTILISATION: f64 = 0.8;

/// A rolling-hour request budget.
///
/// The gateway's quota is `count` requests per `time_window` seconds, so the
/// matching client-side model is a sliding window over request timestamps
/// rather than a fixed counter that resets on the hour: the latter lets you
/// spend the whole allowance in the last minute of one window and the first
/// minute of the next, which the gateway would reject.
///
/// Spending is *advisory* — `try_spend` returning `false` means "skip this
/// request", not "fail". Callers fall back to cached data, so running out of
/// budget degrades freshness rather than breaking the layer.
#[derive(Debug, Default)]
struct RequestBudget {
    /// Instants of requests issued within the current window, oldest first.
    spent: Vec<Instant>,
}

impl RequestBudget {
    /// Drop timestamps that have aged out of the window.
    fn expire(&mut self, window: Duration) {
        let now = Instant::now();
        self.spent.retain(|t| now.duration_since(*t) < window);
    }

    /// Take one request from the budget if any remains.
    fn try_spend(&mut self, limit: u32, window: Duration) -> bool {
        self.expire(window);
        if self.spent.len() as u32 >= limit {
            return false;
        }
        self.spent.push(Instant::now());
        true
    }

    /// How many requests remain in the current window.
    fn remaining(&mut self, limit: u32, window: Duration) -> u32 {
        self.expire(window);
        limit.saturating_sub(self.spent.len() as u32)
    }
}

/// Width of the gateway's quota window.
const BUDGET_WINDOW: Duration = Duration::from_secs(3600);

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
    /// Per-location observation cache.  Key: (lat.to_bits(), lon.to_bits()).
    /// Value: points from that location's bbox query + the fetch instant.
    location_cache: ObsCache,
    /// Rolling-hour spend against the gateway quota, shared by every request
    /// this provider makes so the phases cannot each blow the budget alone.
    budget: Arc<tokio::sync::Mutex<RequestBudget>>,
}

#[derive(Debug, Clone)]
struct MemCacheEntry {
    fetched_at: Instant,
    bounds: Option<Bounds>,
    layer: ObservationLayer,
}

/// How long the viewport's own observations are reused, in memory and on disk.
///
/// Short relative to [`CAPITAL_DATA_TTL`], deliberately: this is the data the
/// user is actually reading, so it is where the request budget gets spent.
const VIEWPORT_DATA_TTL: Duration = Duration::from_secs(600);

impl EumetnetProvider {
    pub fn new(client: Client, dirs: FrontDirs, config: EumetnetConfig) -> Self {
        Self {
            client,
            dirs,
            config,
            mem_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            rate_limited_until: Arc::new(tokio::sync::Mutex::new(None)),
            location_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            budget: Arc::new(tokio::sync::Mutex::new(RequestBudget::default())),
        }
    }

    /// How many requests front will spend per hour, given whether an API key
    /// is configured.  Held below the published quota by [`BUDGET_UTILISATION`].
    fn budget_limit(&self) -> u32 {
        let quota = f64::from(self.config.hourly_quota());
        (quota * BUDGET_UTILISATION).floor().max(1.0) as u32
    }

    /// Reserve one request against the hourly budget.
    ///
    /// `false` means the caller should skip the request and leave whatever is
    /// cached on screen — freshness degrades, the layer keeps working.
    async fn try_spend(&self) -> bool {
        let limit = self.budget_limit();
        let mut budget = self.budget.lock().await;
        let ok = budget.try_spend(limit, BUDGET_WINDOW);
        if !ok {
            write_log(
                &self.dirs.log_path,
                format!(
                    "eumetnet: hourly request budget exhausted ({limit}/h) — \
                     serving cached observations; set eumetnet.api_key for a larger quota"
                ),
            );
        }
        ok
    }

    /// Requests still available this hour.
    async fn budget_remaining(&self) -> u32 {
        let limit = self.budget_limit();
        self.budget.lock().await.remaining(limit, BUDGET_WINDOW)
    }

    /// Attach the API key to a request when one is configured.
    ///
    /// The gateway accepts the key as an `apikey` header; without it the
    /// caller is treated as anonymous and gets the much smaller quota.
    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let key = self.config.api_key.trim();
        if key.is_empty() {
            req
        } else {
            req.header("apikey", key)
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

        // Try station names from the 24 h disk cache first so we don't block
        // the entire pipeline on a network call.  On a cold cache the names
        // HashMap is empty — stations will show their WIGOS ID until the
        // background fetch completes and caches to disk for the next refresh.
        let cached_stations = Self::load_disk_cache::<StationListEntry>(
            stations_cache_path,
            STATION_LIST_TTL.as_secs(),
        )
        .filter(|e| e.bounds.is_none() && !e.stations.is_empty());

        let names: HashMap<String, String> = cached_stations
            .map(|e| {
                e.stations
                    .into_iter()
                    .map(|s| (s.wigos_id, s.name))
                    .collect()
            })
            .unwrap_or_default();

        // If the disk cache missed, fetch the station list in the background
        // so capitals/cities can start loading immediately.
        let station_list_fut = if names.is_empty() {
            let provider = self.clone();
            let sp = stations_cache_path.to_path_buf();
            let ep = endpoint.to_string();
            let cid = collection_id.to_string();
            let log_p = log.to_path_buf();
            Some(tokio::spawn(async move {
                provider.fetch_station_list(&sp, &ep, &cid, &log_p).await
            }))
        } else {
            None
        };

        // `seen_wigos` deduplicates stations across all phases.
        let mut seen_wigos: HashSet<String> = HashSet::new();

        // Spawn the viewport area query immediately so it runs concurrently
        // with the location batch.  It's the slowest single request (the
        // gateway can take ~40s to assemble a continent-wide area response)
        // and covers the most important stations — those actually visible.
        let viewport_task = if zoom >= OBS_TIER_ZOOM_CUTOFF {
            let provider = self.clone();
            let ep = endpoint.to_string();
            let cid = collection_id.to_string();
            let cp = cache_path.to_path_buf();
            let nms = names.clone();
            let log_p = log.to_path_buf();
            let fetch_bounds = bounds.map(|b| b.expanded(0.5));
            Some(tokio::spawn(async move {
                provider
                    .fetch_viewport_points(&cp, &ep, &cid, fetch_bounds, &nms, &log_p)
                    .await
            }))
        } else {
            None
        };

        // The regional backdrop (capitals + major cities) is only useful
        // when zoomed out enough that no viewport query ran above — at
        // higher zoom the viewport already covers what's visible, and
        // fetching ~16 continental cells on top of it would be pure waste
        // against the anonymous hourly quota.
        if should_fetch_backdrop(zoom) {
            // Merge capitals + major cities into one centre-sorted list so
            // stations pop in from the viewport centre outward instead of in
            // two neat capital-then-city waves.
            let center = bounds
                .map(|b| WorldPoint {
                    x: (b.min_x + b.max_x) * 0.5,
                    y: (b.min_y + b.max_y) * 0.5,
                })
                .unwrap_or_else(|| lat_lon_to_world(EUROPE_LAT, EUROPE_LON));

            // Cluster the city list into regional cells before fetching: one
            // query per cell covers every station inside it, so a full
            // refresh costs ~16 requests instead of 113 and actually fits
            // the gateway budget.
            let cities: Vec<(f64, f64)> = EUROPEAN_CAPITALS
                .iter()
                .chain(EUROPEAN_MAJOR_CITIES.iter())
                .copied()
                .collect();
            let mut all_locations = region_cells(&cities, REGION_CELL_DEG);
            all_locations.sort_by(|&(la, lo_a), &(lb, lo_b)| {
                let da = {
                    let w = lat_lon_to_world(la, lo_a);
                    (w.x - center.x).powi(2) + (w.y - center.y).powi(2)
                };
                let db = {
                    let w = lat_lon_to_world(lb, lo_b);
                    (w.x - center.x).powi(2) + (w.y - center.y).powi(2)
                };
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            });

            self.fetch_location_batch(
                endpoint,
                collection_id,
                &all_locations,
                "locations",
                &names,
                log,
                &point_tx,
                &flush_tx,
                &self.location_cache,
                &mut seen_wigos,
            )
            .await;
        }

        // Let the background station list fetch complete so it writes to
        // disk cache for the next refresh.  The current refresh already
        // used the disk-cached names (or WIGOS IDs on a cold cache).
        if let Some(fut) = station_list_fut {
            let _ = fut.await;
        }

        // Collect the pre-warmed viewport result and stream it.
        if let Some(task) = viewport_task {
            if let Ok(pts) = task.await {
                let mut sent_any = false;
                for pt in pts {
                    if seen_wigos.insert(pt.station_id.clone()) {
                        let _ = point_tx.send(pt);
                        sent_any = true;
                    }
                }
                if sent_any {
                    let _ = flush_tx.send(());
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

    /// Fetch viewport-area observation points using mem + disk caches.
    /// `fetch_bounds` is the expanded area sent to the API; the cached result
    /// is stored with these bounds so nearby viewports also hit the cache.
    #[allow(clippy::too_many_arguments)]
    async fn fetch_viewport_points(
        &self,
        cache_path: &Path,
        endpoint: &str,
        collection_id: &str,
        fetch_bounds: Option<Bounds>,
        names: &HashMap<String, String>,
        log: &Path,
    ) -> Vec<ObservationPoint> {
        let cache_hit = {
            let cache = self.mem_cache.lock().await;
            cache.get(collection_id).and_then(|e| {
                if e.fetched_at.elapsed() < VIEWPORT_DATA_TTL
                    && bounds_covered(e.bounds, fetch_bounds)
                {
                    Some(e.layer.points.clone())
                } else {
                    None
                }
            })
        };

        if let Some(pts) = cache_hit {
            write_log(
                log,
                format!("eumetnet: viewport mem cache hit ({} pts)", pts.len()),
            );
            return pts;
        }

        let disk_hit =
            Self::load_disk_cache::<DiskCacheEntry>(cache_path, VIEWPORT_DATA_TTL.as_secs())
                .and_then(|e| {
                    if bounds_covered(e.bounds, fetch_bounds) {
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
            let mut cache = self.mem_cache.lock().await;
            cache.insert(
                collection_id.to_string(),
                MemCacheEntry {
                    fetched_at: Instant::now(),
                    bounds: fetch_bounds,
                    layer: ObservationLayer {
                        points: pts.clone(),
                        updated_at: None,
                    },
                },
            );
            return pts;
        }

        let polygon = bounds_polygon(fetch_bounds);
        let pts = self
            .fetch_area_points(
                endpoint,
                collection_id,
                &polygon,
                names,
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
                bounds: fetch_bounds,
                layer: layer.clone(),
            }) {
                let _ = write_atomic(cache_path, &json);
            }
            let mut cache = self.mem_cache.lock().await;
            cache.insert(
                collection_id.to_string(),
                MemCacheEntry {
                    fetched_at: Instant::now(),
                    bounds: fetch_bounds,
                    layer,
                },
            );
        }

        pts
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
        flush_tx: &tokio::sync::mpsc::UnboundedSender<()>,
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

        // Flush any fresh cached entries so they appear immediately.
        let _ = flush_tx.send(());

        if stale.is_empty() {
            return;
        }

        // Spend at most half the remaining hourly allowance on this batch.
        //
        // These are the context boxes, not the viewport; leaving headroom means
        // a pan or zoom immediately afterwards can still refresh the stations
        // the user is looking at rather than finding the budget already gone.
        // `stale` is centre-sorted, so truncating drops the farthest first.
        let allowance = (self.budget_remaining().await / 2).max(1) as usize;
        if stale.len() > allowance {
            write_log(
                log,
                format!(
                    "eumetnet: {label} — {} stale, budget allows {allowance} this pass",
                    stale.len()
                ),
            );
            stale.truncate(allowance);
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
            let half = REGION_CELL_DEG / 2.0;
            let (lat0, lat1) = (lat - half, lat + half);
            let (lon0, lon1) = (lon - half, lon + half);
            format!("POLYGON(({lon0} {lat0},{lon1} {lat0},{lon1} {lat1},{lon0} {lat1},{lon0} {lat0}))")
        }).collect();

        let dt = &datetime;
        let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_LOCATIONS));
        let mut futs: FuturesUnordered<_> = FuturesUnordered::new();
        for (i, &(clat, clon)) in stale.iter().enumerate() {
            let poly = &polygons[i];
            let sem = Arc::clone(&sem);
            futs.push(async move {
                let _permit = sem.acquire().await;
                let result = self
                    .fetch_area_values(endpoint, collection_id, dt, poly, log)
                    .await;
                (clat, clon, result)
            });
        }

        let mut rate_limit_cooldown: Option<Duration> = None;

        while let Some((clat, clon, fetch)) = futs.next().await {
            if rate_limit_cooldown.is_some() {
                break;
            }

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
                    {
                        let mut cache = cache.lock().await;
                        cache.insert(key, (pts.clone(), fetch_instant));
                    }
                    let mut sent_any = false;
                    for pt in pts {
                        if seen_wigos.insert(pt.station_id.clone()) {
                            let _ = point_tx.send(pt);
                            sent_any = true;
                        }
                    }
                    if sent_any {
                        let _ = flush_tx.send(());
                    }
                }
                AreaFetch::Empty => {
                    let mut cache = cache.lock().await;
                    cache.insert(key, (Vec::new(), fetch_instant));
                }
                AreaFetch::RateLimited(reset) => {
                    let cooldown = reset
                        .map(Duration::from_secs)
                        .unwrap_or(RATE_LIMIT_DEFAULT_COOLDOWN)
                        .min(RATE_LIMIT_MAX_COOLDOWN);
                    rate_limit_cooldown = Some(cooldown);
                }
                // Our own budget ran out.  Nothing was asked of the gateway, so
                // don't cache an empty result — that would mark the location
                // fresh and suppress the refetch once budget returns.  Stop the
                // pass: everything still queued would hit the same wall, and
                // the list is centre-sorted so what we skipped is the far stuff.
                AreaFetch::OverBudget => break,
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
        &self,
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
        let stations = match self
            .fetch_area_values(endpoint, collection_id, &datetime, polygon, log)
            .await
        {
            AreaFetch::Ok(stations) => stations,
            AreaFetch::Empty => Vec::new(),
            // Budget spent — keep whatever is already on screen.
            AreaFetch::OverBudget => return Vec::new(),
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
        &self,
        endpoint: &str,
        collection_id: &str,
        datetime: &str,
        polygon: &str,
        log: &Path,
    ) -> AreaFetch {
        // Check the budget before the request, not after a 429: the point is
        // to stay under the quota rather than to discover it the hard way.
        if !self.try_spend().await {
            return AreaFetch::OverBudget;
        }
        let url = format!("{endpoint}/collections/{collection_id}/area");
        // A continent-wide surface `area` response can be tens of MB and take the
        // gateway ~40s to assemble, so allow generous headroom while still staying
        // under the 120s global client timeout.
        let fetch = async {
            let r = self
                .authed(self.client.get(&url))
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

    // ── zoom tiering ─────────────────────────────────────────────────

    /// Below the shared cutoff, only the regional backdrop should run —
    /// this is the state the viewport gate already covered.
    #[test]
    fn backdrop_fetches_below_the_cutoff() {
        assert!(should_fetch_backdrop(OBS_TIER_ZOOM_CUTOFF - 0.1));
    }

    /// At/above the shared cutoff the viewport query alone covers what's
    /// visible; the regional backdrop must not also fire, or a refresh pays
    /// for ~16 continental cells whose stations are entirely off-screen —
    /// the budget leak this checkpoint closes.
    #[test]
    fn backdrop_does_not_fetch_at_or_above_the_cutoff() {
        assert!(!should_fetch_backdrop(OBS_TIER_ZOOM_CUTOFF));
        assert!(!should_fetch_backdrop(OBS_TIER_ZOOM_CUTOFF + 1.0));
    }

    // ── region clustering ──────────────────────────────────────────────

    /// The regression that made observations appear at random: 113 city boxes
    /// against a 50/hour quota could never all be fetched.  Clustering must
    /// bring a full refresh inside the anonymous budget.
    #[test]
    fn clustering_brings_a_full_refresh_within_the_anonymous_budget() {
        let cities: Vec<(f64, f64)> = EUROPEAN_CAPITALS
            .iter()
            .chain(EUROPEAN_MAJOR_CITIES.iter())
            .copied()
            .collect();
        let cells = region_cells(&cities, REGION_CELL_DEG);
        assert!(
            cells.len() < cities.len() / 4,
            "expected a big reduction, got {} cells from {} cities",
            cells.len(),
            cities.len()
        );
        let budget = (f64::from(crate::config::ANON_HOURLY_QUOTA) * BUDGET_UTILISATION) as usize;
        assert!(
            cells.len() < budget,
            "{} cells must fit the {budget}/h anonymous budget",
            cells.len()
        );
    }

    #[test]
    fn region_cells_deduplicates_nearby_points() {
        // Three points inside one 12° cell collapse to a single query.
        let pts = [(46.05, 14.51), (46.5, 15.0), (45.8, 14.0)];
        assert_eq!(region_cells(&pts, 12.0).len(), 1);
    }

    #[test]
    fn region_cells_keeps_distant_points_apart() {
        // Lisbon and Helsinki cannot share a cell.
        let pts = [(38.72, -9.14), (60.17, 24.94)];
        assert_eq!(region_cells(&pts, 12.0).len(), 2);
    }

    /// Every city must still fall inside the cell that represents it, or the
    /// bbox query would miss the stations it exists to cover.
    #[test]
    fn every_city_is_covered_by_its_cell_bbox() {
        let cities: Vec<(f64, f64)> = EUROPEAN_CAPITALS
            .iter()
            .chain(EUROPEAN_MAJOR_CITIES.iter())
            .copied()
            .collect();
        let cells = region_cells(&cities, REGION_CELL_DEG);
        let half = REGION_CELL_DEG / 2.0;
        for &(lat, lon) in &cities {
            assert!(
                cells.iter().any(|&(cla, clo)| {
                    (lat - cla).abs() <= half + 1e-9 && (lon - clo).abs() <= half + 1e-9
                }),
                "city {lat},{lon} is not covered by any cell"
            );
        }
    }

    // ── RequestBudget ──────────────────────────────────────────────────

    #[test]
    fn budget_allows_exactly_the_limit_then_refuses() {
        let mut b = RequestBudget::default();
        for _ in 0..5 {
            assert!(b.try_spend(5, BUDGET_WINDOW));
        }
        assert!(!b.try_spend(5, BUDGET_WINDOW));
        assert_eq!(b.remaining(5, BUDGET_WINDOW), 0);
    }

    /// A refused request must not consume budget — otherwise a caller that
    /// polls while exhausted would keep the window permanently full.
    #[test]
    fn refused_requests_do_not_consume_budget() {
        let mut b = RequestBudget::default();
        assert!(b.try_spend(1, BUDGET_WINDOW));
        for _ in 0..10 {
            assert!(!b.try_spend(1, BUDGET_WINDOW));
        }
        assert_eq!(b.spent.len(), 1);
    }

    /// Spends older than the window age out, so the budget recovers rather
    /// than staying exhausted for the life of the process.
    #[test]
    fn budget_recovers_once_spends_age_out_of_the_window() {
        let mut b = RequestBudget::default();
        let window = Duration::from_millis(40);
        assert!(b.try_spend(1, window));
        assert!(!b.try_spend(1, window));
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(b.remaining(1, window), 1);
        assert!(b.try_spend(1, window));
    }

    /// The whole point of the budget: an anonymous caller must be held below
    /// the gateway's published 50/hour, and a key must lift it.
    #[test]
    fn budget_limit_tracks_whether_an_api_key_is_configured() {
        let anon = EumetnetConfig::default();
        assert_eq!(anon.hourly_quota(), crate::config::ANON_HOURLY_QUOTA);

        let keyed = EumetnetConfig {
            api_key: "abc123".to_string(),
            ..EumetnetConfig::default()
        };
        assert_eq!(keyed.hourly_quota(), crate::config::AUTH_HOURLY_QUOTA);
        assert!(keyed.hourly_quota() > anon.hourly_quota());
    }

    /// Whitespace is not a key.  A config with a stray space must still be
    /// budgeted as anonymous rather than silently assuming the larger quota.
    #[test]
    fn blank_api_key_is_treated_as_anonymous() {
        let cfg = EumetnetConfig {
            api_key: "   ".to_string(),
            ..EumetnetConfig::default()
        };
        assert_eq!(cfg.hourly_quota(), crate::config::ANON_HOURLY_QUOTA);
    }

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
