use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// One regional backdrop cell persisted to disk: the `(lat, lon)` bit pattern
/// the in-memory cache keys on, the wall-clock second it was fetched, and every
/// station that cell's `area` query returned.
///
/// The in-memory cache times each cell with an `Instant` (monotonic, and so
/// unserialisable). Persistence folds that into a wall-clock `fetched_unix` on
/// save and back into an age on load, so a relaunch recovers each cell's real
/// age instead of resetting the clock and re-paying the whole backdrop.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct BackdropCacheEntry {
    lat_bits: u64,
    lon_bits: u64,
    fetched_unix: u64,
    #[serde(default)]
    points: Vec<ObservationPoint>,
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

/// Dedup admission: is this point new to `seen`?
///
/// Keyed on the WIGOS id, which — unlike the display name — does not change as
/// the name cache warms.
///
/// An **empty** id is admitted unconditionally rather than inserted. Empty ids
/// only arise from `#[serde(default)]` when an on-disk cache entry written by a
/// build predating `wigos_id` is loaded. Inserting them would make every such
/// point collide on the one empty key and collapse the whole cached layer to a
/// single station — verified: a 3-station pre-upgrade entry survives as 1.
/// A point with no identity cannot be shown to duplicate anything, so the safe
/// reading is "keep it"; the ids repopulate on the next fetch.
fn admit_point(seen: &mut HashSet<String>, wigos_id: &str) -> bool {
    if wigos_id.is_empty() {
        return true;
    }
    seen.insert(wigos_id.to_string())
}

/// Convert one parsed `AreaStation` into an `ObservationPoint`.
///
/// `station_id` (display name) is resolved from `names`, which may still be
/// cold; `wigos_id` is carried straight through regardless, so identity never
/// depends on whether the name cache has warmed up.
fn station_to_point(s: AreaStation, names: &HashMap<String, String>) -> ObservationPoint {
    ObservationPoint {
        station_id: names
            .get(&s.wigos_id)
            .cloned()
            .unwrap_or_else(|| s.wigos_id.clone()),
        wigos_id: s.wigos_id.clone(),
        point: GeoPoint::new(s.lon, s.lat),
        world: lat_lon_to_world(s.lat, s.lon),
        temperature: s.values.get("air_temperature").copied(),
        wind_speed: s.values.get("wind_speed").copied(),
        wind_direction: s.values.get("wind_from_direction").copied(),
        humidity: s.values.get("relative_humidity").copied(),
        pressure: s.values.get("air_pressure_at_mean_sea_level").copied(),
    }
}

/// A cell's importance, used as the ordering's second sort key. Lower ranks
/// come first — a capital-bearing cell is fetched before a major-city-only
/// one, at equal viewport overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CityTier {
    Capital,
    Major,
    None,
}

/// Best (lowest-rank) tier of any city in `capitals`/`majors` that snaps onto
/// the `cell_deg` grid cell `(cell_lat, cell_lon)` — the same rounding
/// `region_cells` uses to build cells, so tiers line up with how the cells
/// were derived.
fn cell_tier(
    cell_lat: f64,
    cell_lon: f64,
    capitals: &[(f64, f64)],
    majors: &[(f64, f64)],
    cell_deg: f64,
) -> CityTier {
    let snaps_here = |&(lat, lon): &(f64, f64)| {
        (lat / cell_deg).round() * cell_deg == cell_lat
            && (lon / cell_deg).round() * cell_deg == cell_lon
    };
    if capitals.iter().any(snaps_here) {
        CityTier::Capital
    } else if majors.iter().any(snaps_here) {
        CityTier::Major
    } else {
        CityTier::None
    }
}

/// World-space bounding box of the `cell_deg`-wide cell centred on
/// `(cell_lat, cell_lon)`, used for the viewport-overlap sort key.
fn cell_bounds(cell_lat: f64, cell_lon: f64, cell_deg: f64) -> Bounds {
    let half = cell_deg / 2.0;
    let corner_a = lat_lon_to_world(cell_lat + half, cell_lon - half);
    let corner_b = lat_lon_to_world(cell_lat - half, cell_lon + half);
    Bounds {
        min_x: corner_a.x.min(corner_b.x),
        max_x: corner_a.x.max(corner_b.x),
        min_y: corner_a.y.min(corner_b.y),
        max_y: corner_a.y.max(corner_b.y),
    }
}

/// Order cells by what the user gets first when the budget runs out partway:
/// visible before invisible, important before minor, near before far.
///
/// `capitals`/`majors` supply the tier lookup and `cell_deg` must match the
/// grid `cells` was built on (see `region_cells`); `viewport` and `center`
/// are both in the same normalised world space as `cells`' cell centres once
/// converted through `lat_lon_to_world`.
fn order_cells(
    cells: &[(f64, f64)],
    capitals: &[(f64, f64)],
    majors: &[(f64, f64)],
    cell_deg: f64,
    viewport: Bounds,
    center: WorldPoint,
) -> Vec<(f64, f64)> {
    let mut out = cells.to_vec();
    out.sort_by(|&(la, lo_a), &(lb, lo_b)| {
        let overlap_a = viewport.intersects(cell_bounds(la, lo_a, cell_deg));
        let overlap_b = viewport.intersects(cell_bounds(lb, lo_b, cell_deg));
        // Descending: overlapping (true) sorts before non-overlapping.
        overlap_b
            .cmp(&overlap_a)
            .then_with(|| {
                let tier_a = cell_tier(la, lo_a, capitals, majors, cell_deg);
                let tier_b = cell_tier(lb, lo_b, capitals, majors, cell_deg);
                tier_a.cmp(&tier_b)
            })
            .then_with(|| {
                let da = {
                    let w = lat_lon_to_world(la, lo_a);
                    (w.x - center.x).powi(2) + (w.y - center.y).powi(2)
                };
                let db = {
                    let w = lat_lon_to_world(lb, lo_b);
                    (w.x - center.x).powi(2) + (w.y - center.y).powi(2)
                };
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    out
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

/// How long a regional backdrop cell is retained on disk.
///
/// Deliberately longer than [`CAPITAL_DATA_TTL`] — that constant is the
/// *freshness* window deciding when a cell is refetched for display; this is the
/// *retention* window deciding how long the cell survives on disk. Persisting
/// the backdrop stops every launch re-paying it against the 50/hour quota, and
/// keeping a full day of cells is the ground the observation-history playback
/// layer will read from. Cells older than this are dropped on both save and
/// load.
const BACKDROP_HISTORY_TTL: Duration = Duration::from_secs(24 * 3600);

/// Current wall-clock time in whole seconds since the Unix epoch, or 0 if the
/// clock is set before the epoch.
fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Convert live in-memory backdrop cells to their on-disk form, dropping any
/// older than `max_age`.
///
/// `cells` yields `(lat_bits, lon_bits, age, points)`, where `age` is how long
/// ago the cell was fetched (`Instant::elapsed`). Each age is folded into a
/// wall-clock `fetched_unix` against `now_unix` so it survives a restart.
fn backdrop_to_disk(
    cells: impl Iterator<Item = (u64, u64, Duration, Vec<ObservationPoint>)>,
    now_unix: u64,
    max_age: Duration,
) -> Vec<BackdropCacheEntry> {
    cells
        .filter(|(_, _, age, _)| *age < max_age)
        .map(|(lat_bits, lon_bits, age, points)| BackdropCacheEntry {
            lat_bits,
            lon_bits,
            fetched_unix: now_unix.saturating_sub(age.as_secs()),
            points,
        })
        .collect()
}

/// Rebuild in-memory backdrop cells from their on-disk form, dropping any older
/// than `max_age`.
///
/// Returns `(key, age, points)`; the caller turns `age` back into the `Instant`
/// the live cache times against. A `fetched_unix` in the future relative to
/// `now_unix` (clock moved backwards) yields a zero age via `saturating_sub` —
/// the cell is treated as just-fetched rather than dropped.
fn backdrop_from_disk(
    entries: Vec<BackdropCacheEntry>,
    now_unix: u64,
    max_age: Duration,
) -> Vec<((u64, u64), Duration, Vec<ObservationPoint>)> {
    entries
        .into_iter()
        .filter_map(|e| {
            let age = Duration::from_secs(now_unix.saturating_sub(e.fetched_unix));
            if age >= max_age {
                None
            } else {
                Some(((e.lat_bits, e.lon_bits), age, e.points))
            }
        })
        .collect()
}

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
    /// Set once the persisted backdrop has been loaded into `location_cache`,
    /// so the disk read happens at most once per process.
    backdrop_loaded: Arc<AtomicBool>,
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
            backdrop_loaded: Arc::new(AtomicBool::new(false)),
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
        let backdrop_path = self.dirs.cache_dir.join("eumetnet/surface-backdrop.json");
        self.fetch_observations(
            &cache_path,
            &stations_path,
            &backdrop_path,
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
        backdrop_cache_path: &Path,
        endpoint: &str,
        collection_id: &str,
        zoom: f64,
        bounds: Option<Bounds>,
        point_tx: tokio::sync::mpsc::UnboundedSender<ObservationPoint>,
        flush_tx: tokio::sync::mpsc::UnboundedSender<()>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let log = &self.dirs.log_path;
        let t_total = Instant::now();

        // Warm the backdrop from disk before any batch runs so a relaunch shows
        // yesterday's regional cells immediately and only refetches the stale
        // ones, rather than re-paying the whole backdrop against the quota.
        self.load_backdrop(backdrop_cache_path).await;

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
            let cell_list = region_cells(&cities, REGION_CELL_DEG);
            // A cell that overlaps nothing (bounds unknown) falls back to the
            // full world so overlap can never disqualify a cell it has no
            // evidence against — tier and distance still decide.
            let viewport = bounds.unwrap_or(Bounds {
                min_x: 0.0,
                max_x: 1.0,
                min_y: 0.0,
                max_y: 1.0,
            });
            let all_locations = order_cells(
                &cell_list,
                EUROPEAN_CAPITALS,
                EUROPEAN_MAJOR_CITIES,
                REGION_CELL_DEG,
                viewport,
                center,
            );

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

            // Persist the refreshed backdrop (pruned to the 24 h retention
            // window) so the next launch reuses it instead of re-paying it.
            self.save_backdrop(backdrop_cache_path).await;
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
                    if admit_point(&mut seen_wigos, &pt.wigos_id) {
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
                                if admit_point(seen_wigos, &pt.wigos_id) {
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
                            if admit_point(seen_wigos, &pt.wigos_id) {
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
                        .map(|s| station_to_point(s, names))
                        .collect();
                    {
                        let mut cache = cache.lock().await;
                        cache.insert(key, (pts.clone(), fetch_instant));
                    }
                    let mut sent_any = false;
                    for pt in pts {
                        if admit_point(seen_wigos, &pt.wigos_id) {
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
        if !self.try_spend().await {
            return stale_cache.unwrap_or_default();
        }

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

    /// Load the persisted regional backdrop into `location_cache` once per
    /// process, dropping cells older than [`BACKDROP_HISTORY_TTL`]. A no-op on
    /// later calls, when the file is absent, or when it fails to parse.
    ///
    /// Cells already present in memory win (`or_insert`): a fetch that beat the
    /// load must not be clobbered by a staler disk copy.
    async fn load_backdrop(&self, path: &Path) {
        if self.backdrop_loaded.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(Some(bytes)) = read_if_exists(path) else {
            return;
        };
        let Ok(entries) = serde_json::from_slice::<Vec<BackdropCacheEntry>>(&bytes) else {
            return;
        };
        let restored = backdrop_from_disk(entries, now_unix_secs(), BACKDROP_HISTORY_TTL);
        if restored.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut cache = self.location_cache.lock().await;
        for (key, age, points) in restored {
            let fetched = now.checked_sub(age).unwrap_or(now);
            cache.entry(key).or_insert((points, fetched));
        }
    }

    /// Persist the regional backdrop cache to disk, pruned to
    /// [`BACKDROP_HISTORY_TTL`]. Called after each backdrop refresh so the newest
    /// cells — and everything still inside the retention window — survive a
    /// restart instead of being re-paid against the hourly quota.
    async fn save_backdrop(&self, path: &Path) {
        let now_unix = now_unix_secs();
        let entries = {
            let cache = self.location_cache.lock().await;
            backdrop_to_disk(
                cache.iter().map(|(&(lat_bits, lon_bits), (points, fetched))| {
                    (lat_bits, lon_bits, fetched.elapsed(), points.clone())
                }),
                now_unix,
                BACKDROP_HISTORY_TTL,
            )
        };
        if let Ok(json) = serde_json::to_vec(&entries) {
            let _ = write_atomic(path, &json);
        }
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
            .map(|s| station_to_point(s, names))
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

    // ── backdrop disk persistence ───────────────────────────────────────

    fn obs_point(wigos_id: &str) -> ObservationPoint {
        ObservationPoint {
            point: GeoPoint::new(14.5, 46.0),
            world: lat_lon_to_world(46.0, 14.5),
            station_id: wigos_id.to_string(),
            wigos_id: wigos_id.to_string(),
            temperature: Some(21.0),
            wind_speed: None,
            wind_direction: None,
            humidity: None,
            pressure: None,
        }
    }

    /// A cell serialised then deserialised must recover its key, its points,
    /// and — crucially — its *age*, not reset to fresh. The wall-clock fold is
    /// the only thing that lets a restart know a cell is 1 h old rather than
    /// brand new, which is what keeps the freshness TTL meaningful across runs.
    #[test]
    fn backdrop_round_trip_preserves_key_points_and_age() {
        let now = 100_000u64;
        let cells = vec![(1u64, 2u64, Duration::from_secs(3600), vec![obs_point("0-1")])];
        let disk = backdrop_to_disk(cells.into_iter(), now, BACKDROP_HISTORY_TTL);
        assert_eq!(disk.len(), 1);
        assert_eq!(disk[0].fetched_unix, now - 3600);

        let back = backdrop_from_disk(disk, now, BACKDROP_HISTORY_TTL);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].0, (1, 2));
        assert_eq!(back[0].1, Duration::from_secs(3600));
        assert_eq!(back[0].2.len(), 1);
        assert_eq!(back[0].2[0].wigos_id, "0-1");
    }

    /// Save-side pruning: a cell already older than the retention window is
    /// never written, so the on-disk file cannot grow without bound.
    #[test]
    fn backdrop_to_disk_drops_cells_older_than_the_retention_window() {
        let cells = vec![
            (1u64, 1u64, BACKDROP_HISTORY_TTL + Duration::from_secs(1), vec![]),
            (2u64, 2u64, Duration::from_secs(3600), vec![]),
        ];
        let disk = backdrop_to_disk(cells.into_iter(), 100_000, BACKDROP_HISTORY_TTL);
        assert_eq!(disk.len(), 1);
        assert_eq!((disk[0].lat_bits, disk[0].lon_bits), (2, 2));
    }

    /// Load-side pruning: a file that outlived the retention window (app was
    /// closed for over a day) drops the expired cells on load rather than
    /// resurrecting day-old readings as if current.
    #[test]
    fn backdrop_from_disk_drops_entries_older_than_the_retention_window() {
        let now = 200_000u64;
        let entries = vec![
            BackdropCacheEntry {
                lat_bits: 1,
                lon_bits: 1,
                fetched_unix: now - (BACKDROP_HISTORY_TTL.as_secs() + 1),
                points: vec![],
            },
            BackdropCacheEntry {
                lat_bits: 2,
                lon_bits: 2,
                fetched_unix: now - 3600,
                points: vec![],
            },
        ];
        let back = backdrop_from_disk(entries, now, BACKDROP_HISTORY_TTL);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].0, (2, 2));
    }

    /// A `fetched_unix` ahead of `now` (system clock moved backwards between
    /// runs) must not underflow into a huge age that silently drops the cell —
    /// it is clamped to zero age and kept.
    #[test]
    fn backdrop_from_disk_clamps_future_timestamps_to_zero_age() {
        let entries = vec![BackdropCacheEntry {
            lat_bits: 1,
            lon_bits: 1,
            fetched_unix: 5_000,
            points: vec![],
        }];
        let back = backdrop_from_disk(entries, 1_000, BACKDROP_HISTORY_TTL);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].1, Duration::from_secs(0));
    }

    // ── WIGOS dedup identity ────────────────────────────────────────────

    /// An on-disk cache entry written before `wigos_id` existed deserializes
    /// with every id defaulted to `""`. Those points must all survive dedup.
    ///
    /// Keying them through a plain `HashSet::insert` collapses them onto the
    /// single empty key: a 3-station pre-upgrade layer renders as 1 station
    /// until the 10-minute viewport TTL expires. Verified before the guard
    /// existed — survivors were 1 of 3.
    #[test]
    fn empty_wigos_ids_from_a_pre_upgrade_cache_all_survive_dedup() {
        let old_layer = r#"{"points":[
          {"point":{"lat":46.0,"lon":14.5},"world":{"x":0.5,"y":0.3},
           "station_id":"Ljubljana","temperature":21.0,"wind_speed":null,
           "wind_direction":null,"humidity":null,"pressure":null},
          {"point":{"lat":48.2,"lon":16.4},"world":{"x":0.5,"y":0.3},
           "station_id":"Vienna","temperature":19.0,"wind_speed":null,
           "wind_direction":null,"humidity":null,"pressure":null},
          {"point":{"lat":45.8,"lon":15.9},"world":{"x":0.5,"y":0.3},
           "station_id":"Zagreb","temperature":23.0,"wind_speed":null,
           "wind_direction":null,"humidity":null,"pressure":null}
        ]}"#;
        let layer: crate::layers::ObservationLayer =
            serde_json::from_str(old_layer).expect("pre-upgrade entry must still deserialize");
        assert_eq!(layer.points.len(), 3);
        assert!(
            layer.points.iter().all(|p| p.wigos_id.is_empty()),
            "serde default must yield empty ids for the old format"
        );

        let mut seen: HashSet<String> = HashSet::new();
        let kept = layer
            .points
            .iter()
            .filter(|p| admit_point(&mut seen, &p.wigos_id))
            .count();
        assert_eq!(
            kept, 3,
            "identity-less points must not collapse onto each other"
        );
    }

    /// Real ids still dedup normally — the empty-id escape hatch must not
    /// disable dedup for everything else.
    #[test]
    fn real_wigos_ids_still_dedup() {
        let mut seen: HashSet<String> = HashSet::new();
        assert!(admit_point(&mut seen, "0-20000-0-11035"));
        assert!(!admit_point(&mut seen, "0-20000-0-11035"));
        assert!(admit_point(&mut seen, "0-20000-0-06260"));
    }

    /// The same physical station reported cold (no name cache yet) and warm
    /// (name cache populated later in the session) must dedup to one entry.
    /// `station_id` legitimately differs between the two calls — that's the
    /// conflation this checkpoint exists to stop dedup from keying on.
    #[test]
    fn wigos_dedup_key_is_stable_across_cold_and_warm_name_cache() {
        let station = || AreaStation {
            wigos_id: "0-20000-0-06260".to_string(),
            lon: 5.18,
            lat: 52.1,
            values: HashMap::new(),
        };

        let cold_names: HashMap<String, String> = HashMap::new();
        let mut warm_names: HashMap<String, String> = HashMap::new();
        warm_names.insert("0-20000-0-06260".to_string(), "De Bilt".to_string());

        let phase1 = station_to_point(station(), &cold_names);
        let phase2 = station_to_point(station(), &warm_names);

        // Sanity check that this scenario actually reproduces the
        // conflation: the display name differs between phases.
        assert_ne!(phase1.station_id, phase2.station_id);

        let mut seen: HashSet<String> = HashSet::new();
        let mut surviving = 0;
        for pt in [phase1, phase2] {
            if seen.insert(pt.wigos_id.clone()) {
                surviving += 1;
            }
        }
        assert_eq!(
            surviving, 1,
            "same station across a cold/warm name cache must dedup to one entry"
        );
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

    // ── order_cells ────────────────────────────────────────────────────

    /// Equator/prime-meridian centre, shared by the three tests below so
    /// distance comparisons line up with the symmetric fixture cells.
    fn equator_center() -> WorldPoint {
        lat_lon_to_world(0.0, 0.0)
    }

    /// Overlap must win regardless of tier or distance: two cells at equal
    /// distance from centre, neither holding a tiered city, one overlapping
    /// the viewport and one not.
    #[test]
    fn order_cells_prefers_viewport_overlap_at_equal_tier_and_distance() {
        let overlapping = (0.0, 12.0); // world x ~0.533, box x ~[0.517, 0.55]
        let non_overlapping = (0.0, -12.0); // world x ~0.467, box x ~[0.45, 0.483]
        let viewport = Bounds {
            min_x: 0.52,
            max_x: 0.6,
            min_y: 0.0,
            max_y: 1.0,
        };

        let ordered = order_cells(
            &[non_overlapping, overlapping],
            &[],
            &[],
            REGION_CELL_DEG,
            viewport,
            equator_center(),
        );

        assert_eq!(ordered, vec![overlapping, non_overlapping]);
    }

    /// Tier must win when overlap is tied: two cells at equal distance from
    /// centre, both inside the viewport, one holding a capital and one
    /// holding only a major city.
    #[test]
    fn order_cells_prefers_capital_tier_at_equal_overlap_and_distance() {
        let capital_cell = (0.0, 12.0);
        let major_cell = (0.0, -12.0);
        let whole_world = Bounds {
            min_x: 0.0,
            max_x: 1.0,
            min_y: 0.0,
            max_y: 1.0,
        };

        let ordered = order_cells(
            &[major_cell, capital_cell],
            &[(0.0, 12.0)],
            &[(0.0, -12.0)],
            REGION_CELL_DEG,
            whole_world,
            equator_center(),
        );

        assert_eq!(ordered, vec![capital_cell, major_cell]);
    }

    /// Distance must decide when overlap and tier are tied: two untiered
    /// cells both inside the viewport, one nearer to centre than the other.
    #[test]
    fn order_cells_prefers_nearer_cell_at_equal_overlap_and_tier() {
        let near = (0.0, 12.0);
        let far = (0.0, 24.0);
        let whole_world = Bounds {
            min_x: 0.0,
            max_x: 1.0,
            min_y: 0.0,
            max_y: 1.0,
        };

        let ordered = order_cells(
            &[far, near],
            &[],
            &[],
            REGION_CELL_DEG,
            whole_world,
            equator_center(),
        );

        assert_eq!(ordered, vec![near, far]);
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

    // ── fetch_station_list budget reservation ───────────────────────────

    /// `fetch_station_list` must reserve budget through `try_spend` before
    /// firing its `/locations` request, or the client-side budget under-counts
    /// what the gateway actually sees. A cache miss against an unroutable
    /// endpoint still spends one unit even though the request itself fails,
    /// proving the reservation happens ahead of the network call rather than
    /// never happening at all.
    #[tokio::test]
    async fn fetch_station_list_reserves_budget_before_the_network_request() {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("front-eumetnet-test-{nanos}"));
        let log_path = base.join("front.log");
        let dirs = FrontDirs {
            config_dir: base.clone(),
            cache_dir: base.clone(),
            maps_dir: base.clone(),
            radar_dir: base.clone(),
            log_path: log_path.clone(),
        };
        let stations_cache_path = base.join("stations.json");

        // Populate a stale (past the 24 h fresh TTL, but usable as a
        // fallback) station-list cache so a network failure has something to
        // degrade to.
        let stale = StationListEntry {
            bounds: None,
            stations: vec![StationInfo {
                wigos_id: "0-20000-0-99999".to_string(),
                name: "Test Station".to_string(),
                lon: 14.5,
                lat: 46.0,
            }],
        };
        write_atomic(&stations_cache_path, &serde_json::to_vec(&stale).unwrap()).unwrap();
        let file = std::fs::File::options()
            .write(true)
            .open(&stations_cache_path)
            .unwrap();
        file.set_modified(SystemTime::now() - STATION_LIST_TTL - Duration::from_secs(3600))
            .unwrap();
        drop(file);

        let provider = EumetnetProvider::new(Client::new(), dirs, EumetnetConfig::default());

        let before = provider.budget_remaining().await;
        assert!(before > 0, "test needs budget available to spend");

        // Nothing listens here, so the request fails fast; the point is
        // whether budget was reserved *before* that failure.
        let result = provider
            .fetch_station_list(
                &stations_cache_path,
                "http://127.0.0.1:1",
                "observations",
                &log_path,
            )
            .await;

        // Network failed, so the stale cache is what comes back.
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].wigos_id, "0-20000-0-99999");

        let after = provider.budget_remaining().await;
        assert_eq!(
            after,
            before - 1,
            "fetch_station_list must call try_spend before issuing its /locations request"
        );

        let _ = std::fs::remove_dir_all(&base);
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
