use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Datelike;
use color_eyre::eyre::{Context, Result};
use reqwest::Client;
use tiff::decoder::{ifd, Decoder, DecodingResult};
use tiff::tags::Tag;
use tokio::sync::Semaphore;

use crate::cache::{read_if_exists, write_atomic, write_log, FrontDirs};
use crate::config::MeteoGateConfig;
use crate::geo::{world_to_lat_lon, Bounds, TileCoord, WorldPoint};
use crate::layers::{RadarFrame, RadarRun, RadarTile, Rgb8};

/// Cadence of the OPERA composite: one published slot every 5 minutes.
pub const SLOT_SECS: i64 = 300;

/// Selectable history depths, in hours.  The `openradar-24h` bucket retains a
/// rolling 24 h (verified: the day before yesterday lists zero keys), so 24 is
/// the deepest window it can serve.
pub const HISTORY_OPTIONS: [u8; 4] = [3, 6, 12, 24];

/// Default history depth in hours.
pub const DEFAULT_HISTORY_HOURS: u8 = 3;

/// Number of 5-minute slots spanning `hours` of history.
pub fn frames_for_hours(hours: u8) -> usize {
    (hours as usize) * 3600 / SLOT_SECS as usize
}

/// Next depth in the cycle, wrapping back to the shortest.
pub fn next_history_hours(hours: u8) -> u8 {
    let i = HISTORY_OPTIONS.iter().position(|&h| h == hours);
    match i {
        Some(i) => HISTORY_OPTIONS[(i + 1) % HISTORY_OPTIONS.len()],
        // Unrecognised value (e.g. hand-edited state.toml): snap to default.
        None => DEFAULT_HISTORY_HOURS,
    }
}

const S3_PREFIX: &str = "OPERA/COMP";
const PRODUCT: &str = "DBZH";
const TILE_PX: u32 = 256;

/// LAEA projection parameters (OPERA GeoTIFF embedded CRS).
const LAEA_LAT0: f64 = 55.0_f64.to_radians(); // latitude of natural origin
const LAEA_LON0: f64 = 10.0_f64.to_radians(); // central meridian
const LAEA_R: f64 = 6_378_137.0; // WGS84 semi-major axis used as authalic sphere

/// False easting/northing from the GeoTIFF CRS (EPSG:3035-ish).
/// The CRS coordinate = true LAEA + (false_easting, false_northing).
const LAEA_FALSE_E: f64 = 1_950_000.0;
const LAEA_FALSE_N: f64 = -2_100_000.0;

/// Minimum dBZ value (≈ 1 dBZ). Values below this are noise/undetect.
const MIN_DBZ: f32 = 1.0;

/// Cap on samples taken per axis when averaging a source footprint.
///
/// Zoomed fully out a single pixel covers ~2900 source cells; reading all of
/// them would cost more than the whole frame decode for no visible gain, since
/// the mean has long since converged.  8 per axis (64 samples) keeps small
/// features represented while bounding the work per pixel.
const MAX_FOOTPRINT_SAMPLES: u32 = 8;

// ---------------------------------------------------------------------------
// On-disk grid format (`.frd`)
// ---------------------------------------------------------------------------
//
// The OPERA GeoTIFF is a transport format, not a good cache format.  It carries
// two f32 bands — inflating one frame yields ~151 MB, of which we discard half
// (only sample 0 is reflectivity) — and its deflate stream costs ~147 ms to
// decode, which dominated frame loading whether the bytes came from S3 or disk.
//
// Band 0 is not really continuous: it is quantised to exact 0.5 dBZ steps, so
// every value it holds fits a `u8` code *losslessly*.  Storing those codes under
// zstd instead gives, per frame: 0.80 MB on disk (vs 2.82 MB), ~3 ms to decode
// (vs ~198 ms), and a 16.7 MB grid in RAM (vs 66.9 MB).  Smaller, faster and
// lighter at once, which is why the cache holds `.frd` and not the source TIFF.

/// Magic + version.  Bumping the trailing byte invalidates old caches: readers
/// reject unknown versions and the frame is refetched.
const FRD_MAGIC: &[u8; 4] = b"FRD1";

/// dBZ of code 1.  Code 0 is reserved for no-data/undetect.
const DBZ_BASE: f32 = -32.0;

/// dBZ per code step.  Matches the quantisation OPERA already applies; the
/// writer verifies this rather than assuming it.
const DBZ_STEP: f32 = 0.5;

/// Values at or below this are the file's no-data sentinel (-9 999 000), not
/// weak echo.
const DBZ_SENTINEL_MAX: f32 = -1000.0;

/// zstd level for grid payloads.  Level 1 is the sweet spot here: it matches
/// level 9's size to within 7 % (0.80 vs 0.75 MB) while decompressing at
/// ~6 GB/s, and compression happens once per frame on a background task.
const FRD_ZSTD_LEVEL: i32 = 1;

/// Encode one dBZ sample as a `u8` code, or `None` when it is no-data.
///
/// Returns `Err` when the value doesn't sit on the expected 0.5 dBZ grid or
/// exceeds the ceiling.  That is deliberate: silently rounding to the nearest
/// code would corrupt pixels invisibly if OPERA ever changed its quantisation,
/// so the conversion fails loudly instead.
///
/// Values *below* [`DBZ_BASE`] are the one exception, and are folded into the
/// no-data code rather than failing.  OPERA does emit them — measured across
/// three consecutive composites, each carried exactly one pixel in the
/// -35..-32.5 range out of ~628 000 — and that single pixel used to abort the
/// whole frame, so roughly three quarters of all frames never decoded and the
/// timeline showed permanent gaps.  Nothing is lost by dropping them: they sit
/// far below [`MIN_DBZ`], the threshold under which a sample is treated as
/// undetect and never drawn.
fn dbz_to_code(v: f32) -> Result<u8> {
    if !v.is_finite() || v <= DBZ_SENTINEL_MAX {
        return Ok(0);
    }
    let step = ((v - DBZ_BASE) / DBZ_STEP).round();
    let code = step + 1.0;
    if code < 1.0 {
        // Weak echo below the encodable floor — undetect for our purposes.
        return Ok(0);
    }
    if code > 255.0 {
        color_eyre::eyre::bail!("dBZ value {v} above representable range");
    }
    let code = code as u8;
    // Verify rather than trust: the round-trip must be exact.
    if (code_to_dbz(code).unwrap_or(f32::NAN) - v).abs() > 1e-3 {
        color_eyre::eyre::bail!("dBZ value {v} is not on the {DBZ_STEP} dBZ grid");
    }
    Ok(code)
}

/// Decode a `u8` code back to dBZ.  `None` for the no-data code.
#[inline]
fn code_to_dbz(code: u8) -> Option<f32> {
    if code == 0 {
        None
    } else {
        Some(DBZ_BASE + (f32::from(code) - 1.0) * DBZ_STEP)
    }
}

/// Shared in-memory cache for decoded radar grids, keyed by timestamp.
///
/// A small LRU keeps the frames touched during interaction (the current one
/// plus a couple being preloaded) resident instead of re-decoding on pan/zoom.
type GridCache = Arc<tokio::sync::Mutex<Vec<(i64, Arc<RadarGrid>)>>>;

/// Maximum number of decoded grids held in memory at once.
///
/// Each grid is ~16.7 MB of codes, so this is still among the larger resident
/// costs.  It only pays off when the *same* timestamp is re-decoded — a zoom or
/// pan of the frame on screen.  Preload streams distinct timestamps through it
/// and never re-reads them, so extra slots buy almost nothing: measured over a
/// session that decoded 144 frames, a 3-slot cache returned 1 hit.  Two slots
/// keep the interactive frame resident against one concurrent preload; the rest
/// is left to the on-disk `.frd` grids, which reload in ~3 ms.
const MAX_CACHED_GRIDS: usize = 2;

/// How long a negative HEAD result ("object not on S3 yet") is cached
/// before the slot is probed again.
const MISSING_RETRY: Duration = Duration::from_secs(60);

/// How long a *transient* probe failure (timeout, 5xx, 429, DNS blip) is
/// cached before re-probing.
///
/// Deliberately far shorter than [`MISSING_RETRY`]: a slot we merely failed to
/// reach is very likely there, and pinning it as unreachable for a full minute
/// is what made a single dropped packet look like a missing frame.
const TRANSIENT_RETRY: Duration = Duration::from_secs(5);

/// Ceiling on a server-supplied `Retry-After`, so a hostile or mistaken header
/// cannot freeze radar for the rest of the session.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// How many times a GeoTIFF body fetch is attempted before the frame is
/// reported as failed to the caller (which then schedules its own retry).
const DOWNLOAD_ATTEMPTS: u32 = 3;

/// First backoff between download attempts; doubles each try.
const DOWNLOAD_RETRY_BASE: Duration = Duration::from_millis(400);

/// HEAD timeout.  The old 3 s was tight enough that an ordinary slow round
/// trip registered as "frame does not exist"; the probe is cheap, so give it
/// room to actually answer.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// What a HEAD probe actually established about a slot.
///
/// The distinction matters: S3 answering `404` is authoritative — the frame is
/// genuinely not published yet — whereas a timeout or a `503` says nothing
/// about the object at all.  Collapsing the two (the previous behaviour) let
/// one network blip convince `resolve_nearest_available` that a slot was
/// absent, which silently pulled the displayed frame backwards in time.
#[derive(Debug, Clone, Copy)]
enum Probe {
    /// The object is there.
    Exists,
    /// S3 said `404`: not published (yet).
    Absent,
    /// We could not find out.  Carries how long to wait before asking again.
    Transient(Duration),
}

/// Outcome of probing S3 for a given timestamp, cached so repeated
/// pans/zooms don't re-issue HEAD requests for slots we already know
/// about.  Objects are immutable once published, so positive results
/// never expire; negative results are retried after [`MISSING_RETRY`],
/// and unreachable ones after the much shorter [`TRANSIENT_RETRY`].
#[derive(Debug, Clone, Copy)]
enum ProbeResult {
    Exists,
    Missing(Instant),
    /// Probe failed for reasons unrelated to the object; retry at this instant.
    Unreachable(Instant),
}

type ProbeCache = Arc<tokio::sync::Mutex<HashMap<i64, ProbeResult>>>;

/// Internal metadata returned by [`MeteoGateProvider::frame_impl`].
struct FrameMetadata {
    time: i64,
    path: String,
    tiles: Vec<RadarTile>,
    missing_tiles: usize,
    target_zoom: u8,
}

#[derive(Debug, Clone)]
pub struct MeteoGateProvider {
    client: Client,
    dirs: FrontDirs,
    config: MeteoGateConfig,
    /// In-memory cache of the decoded LAEA radar grid.  Keyed by
    /// timestamp so zoom changes within the same frame don't re-decode
    /// the grid from disk.
    grid_cache: GridCache,
    /// Cache of HEAD-probe results so timestamp resolution doesn't
    /// re-hit S3 on every viewport change.
    probe_cache: ProbeCache,
    cancel: Arc<AtomicBool>,
}

impl MeteoGateProvider {
    pub fn new(
        client: Client,
        dirs: FrontDirs,
        config: MeteoGateConfig,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        Self {
            client,
            dirs,
            config,
            grid_cache: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            probe_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            cancel,
        }
    }

    fn s3_key(&self, timestamp: i64) -> String {
        let dt = time_to_datetime(timestamp);
        format!(
            "{:04}/{:02}/{:02}/{S3_PREFIX}/OPERA@{datetime}@0@{PRODUCT}.tiff",
            dt.year,
            dt.month,
            dt.day,
            datetime = dt.str,
        )
    }

    fn s3_url(&self, timestamp: i64) -> String {
        format!(
            "{}/{}/{}",
            self.config.s3_endpoint,
            self.config.s3_bucket,
            self.s3_key(timestamp)
        )
    }

    /// Path of the cached grid.  The source TIFF is never kept: it is converted
    /// to `.frd` on arrival and only that is stored.
    fn cache_path(&self, timestamp: i64) -> PathBuf {
        self.dirs
            .radar_dir
            .join(format!("meteogate/radar/{}.frd", timestamp))
    }

    /// Delete GeoTIFFs left by builds that cached the source format.
    ///
    /// They are never read again — the cache holds `.frd` now — and at ~2.8 MB
    /// each they would otherwise sit on disk until the 24 h prune aged them out.
    /// Returns the number of bytes reclaimed.
    pub fn purge_legacy_tiffs(&self) -> u64 {
        let dir = self.dirs.radar_dir.join("meteogate/radar");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        let mut freed = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "tiff") {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                if std::fs::remove_file(&path).is_ok() {
                    freed += size;
                }
            }
        }
        freed
    }

    /// Timestamps whose grid is already on disk.
    ///
    /// These load without touching the network, so the timeline can show them
    /// as available rather than as slots that would need a fetch.  One readdir
    /// of a few hundred entries; call it on timeline changes, not per render.
    pub fn cached_timestamps(&self) -> HashSet<i64> {
        let dir = self.dirs.radar_dir.join("meteogate/radar");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return HashSet::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                if path.extension()? != "frd" {
                    return None;
                }
                path.file_stem()?.to_str()?.parse::<i64>().ok()
            })
            .collect()
    }

    /// Return the list of available radar frame timestamps, newest first,
    /// spanning `hours` of history at the 5-minute slot cadence.
    ///
    /// Probes S3 for the current boundary slot so the caller gets the
    /// absolute latest frame when it is already published.  Falls back to
    /// one slot back (`latest - 300`) — the same conservative value used
    /// by the synchronous [`compute_frame_list`] — if the boundary is not
    /// on S3 yet.  One HEAD request at most; result is probe-cached.
    pub async fn frame_list(&self, hours: u8) -> Result<Vec<i64>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let latest = now - (now % SLOT_SECS);
        let start = if self.probe_geotiff(latest).await.unwrap_or(false) {
            latest
        } else {
            latest - SLOT_SECS
        };
        Ok((0..frames_for_hours(hours) as i64)
            .map(|i| start - i * SLOT_SECS)
            .collect())
    }

    pub async fn frame(&self, timestamp: i64, bounds: Bounds, zoom: f64) -> Result<RadarFrame> {
        let metadata = self.frame_impl(timestamp, bounds, zoom, None).await?;
        Ok(RadarFrame {
            time: metadata.time,
            path: metadata.path,
            tiles: metadata.tiles,
            missing_tiles: metadata.missing_tiles,
            target_zoom: metadata.target_zoom,
        })
    }

    /// Returns the cached radar grid for `timestamp`, fetching and
    /// decoding the GeoTIFF if no entry is cached yet.  The returned
    /// `Arc` shares storage with the cache; cloning is cheap.
    async fn load_grid(&self, timestamp: i64) -> Result<Arc<RadarGrid>> {
        let log = &self.dirs.log_path;
        // Fast path: check the LRU without holding the lock during I/O.
        {
            let mut cache = self.grid_cache.lock().await;
            if let Some(pos) = cache.iter().position(|(ts, _)| *ts == timestamp) {
                // Bump to most-recently-used so the frame the user is
                // viewing isn't evicted by concurrent preloads.
                let entry = cache.remove(pos);
                let g = Arc::clone(&entry.1);
                cache.push(entry);
                write_log(log, format!("meteogate: grid cache hit ts={timestamp}"));
                return Ok(g);
            }
        }
        let g = Arc::new(self.load_grid_uncached(timestamp).await?);
        let mut cache = self.grid_cache.lock().await;
        // A concurrent caller may have decoded the same grid while we were
        // parsing; keep a single entry per timestamp.
        if let Some(pos) = cache.iter().position(|(ts, _)| *ts == timestamp) {
            cache.remove(pos);
        }
        cache.push((timestamp, Arc::clone(&g)));
        while cache.len() > MAX_CACHED_GRIDS {
            cache.remove(0);
        }
        Ok(g)
    }

    /// Like [`Self::frame`] but builds tiles in **centre-first clockwise spiral
    /// order** and sends each completed tile through `tile_tx` as soon as
    /// it is ready.  Up to `MAX_CONCURRENT_TILES` tiles are built
    /// concurrently via `tokio::task::spawn_blocking`.
    ///
    /// The returned frame's `tiles` vec may be empty — the caller is
    /// expected to collect tiles from the channel and reconstruct the
    /// frame incrementally.
    pub async fn frame_streamed(
        &self,
        timestamp: i64,
        bounds: Bounds,
        zoom: f64,
        tile_tx: tokio::sync::mpsc::UnboundedSender<Result<RadarTile>>,
    ) -> Result<RadarFrame> {
        let mut metadata = self
            .frame_impl(timestamp, bounds, zoom, Some(tile_tx))
            .await?;
        let missing = std::mem::take(&mut metadata.missing_tiles);
        Ok(RadarFrame {
            time: metadata.time,
            path: metadata.path,
            tiles: metadata.tiles,
            missing_tiles: missing,
            target_zoom: metadata.target_zoom,
        })
    }

    /// Maximum concurrent tile build tasks.
    const MAX_CONCURRENT_TILES: usize = 16;

    /// Shared implementation for [`frame`] and [`frame_streamed`].
    ///
    /// When `tile_tx` is `Some`, tiles are sent through the channel
    /// in centre-first spiral order as they complete; the returned
    /// tiles vec will be empty.  When `None`, tiles are built with
    /// Rayon and returned all at once.
    async fn frame_impl(
        &self,
        timestamp: i64,
        bounds: Bounds,
        zoom: f64,
        tile_tx: Option<tokio::sync::mpsc::UnboundedSender<Result<RadarTile>>>,
    ) -> Result<FrameMetadata> {
        let log = &self.dirs.log_path;
        let effective_ts = self.resolve_nearest_available(timestamp).await?;
        let grid = self.load_grid(effective_ts).await?;

        let z = radar_zoom(zoom);
        let center = WorldPoint {
            x: bounds.min_x + bounds.width() / 2.0,
            y: bounds.min_y + bounds.height() / 2.0,
        };
        let tiles = crate::geo::tiles_spiral_from(bounds, z, center);
        write_log(
            log,
            format!(
                "meteogate: frame_impl building {} tiles at zoom {z}",
                tiles.len()
            ),
        );

        if let Some(tx) = tile_tx {
            // Streaming path: build tiles concurrently (up to 8) via
            // spawn_blocking, send each through the channel as done.
            let total = tiles.len();
            let semaphore = Arc::new(Semaphore::new(Self::MAX_CONCURRENT_TILES));
            let mut handles = Vec::with_capacity(total);
            let grid = Arc::clone(&grid);
            for tc in tiles {
                if self.cancel.load(Ordering::Relaxed) {
                    break;
                }
                let permit = Arc::clone(&semaphore).acquire_owned().await?;
                let g = Arc::clone(&grid);
                let tx = tx.clone();
                handles.push(tokio::spawn(async move {
                    let _permit = permit;
                    let result = tokio::task::spawn_blocking(move || build_tile(&g, tc, z))
                        .await
                        .expect("blocking task panicked");
                    let _ = tx.send(result);
                }));
            }
            drop(tx); // drop our clone so the receiver sees EOF after all handles
            let mut missing_tiles = 0usize;
            for h in handles {
                if h.await.is_err() {
                    // Task panic — count as missing
                    missing_tiles += 1;
                }
            }
            drop(grid); // release Arc
            Ok(FrameMetadata {
                time: effective_ts,
                path: format!("{PRODUCT}/{}", timestamp),
                tiles: Vec::new(),
                missing_tiles,
                target_zoom: z,
            })
        } else {
            // Bulk path: Rayon parallel, collect all at once.
            write_log(log, "meteogate: frame_impl starting tile build (rayon)");
            use rayon::prelude::*;
            let results: Vec<Result<RadarTile>> = tiles
                .par_iter()
                .map(|&tc| build_tile(&grid, tc, z))
                .collect();
            write_log(log, "meteogate: frame_impl tile build done");
            let missing_tiles = results.iter().filter(|r| r.is_err()).count();
            let radar_tiles: Vec<RadarTile> = results.into_iter().filter_map(Result::ok).collect();
            Ok(FrameMetadata {
                time: effective_ts,
                path: format!("{PRODUCT}/{}", timestamp),
                tiles: radar_tiles,
                missing_tiles,
                target_zoom: z,
            })
        }
    }

    /// Produce the grid for `timestamp`, from the `.frd` cache when present and
    /// otherwise by fetching the GeoTIFF and converting it.
    ///
    /// The cached path decompresses one zstd payload (~3 ms).  The cold path
    /// still pays the GeoTIFF's ~150 ms inflate, but only ever once per frame:
    /// the conversion is written out so no later load repeats it.
    async fn load_grid_uncached(&self, timestamp: i64) -> Result<RadarGrid> {
        let log = &self.dirs.log_path;
        let path = self.cache_path(timestamp);

        if let Some(bytes) = read_if_exists(&path)? {
            let grid = tokio::task::spawn_blocking(move || decode_frd(&bytes))
                .await
                .wrap_err("decode_frd task panicked")?;
            match grid {
                Ok(g) => {
                    write_log(log, format!("meteogate: frd cache hit ts={timestamp}"));
                    return Ok(g);
                }
                // A truncated or older-format file: drop it and refetch rather
                // than failing the frame.
                Err(e) => {
                    write_log(
                        log,
                        format!("meteogate: discarding unreadable frd ts={timestamp}: {e}"),
                    );
                    let _ = std::fs::remove_file(&path);
                }
            }
        }

        let bytes = self.download_geotiff(timestamp).await?;
        write_log(log, format!("meteogate: downloaded {} bytes", bytes.len()));

        let converted = tokio::task::spawn_blocking(move || -> Result<_> {
            let grid = parse_geotiff(&bytes)?;
            let encoded = encode_frd(&grid)?;
            Ok((grid, encoded))
        })
        .await
        .wrap_err("parse_geotiff task panicked")?;

        // Log before propagating.  This error used to travel out silently, so a
        // frame that downloaded fine but failed to convert looked identical to
        // one that was never published — the log showed the download and then
        // simply nothing, which is how a 77 % conversion failure rate went
        // unnoticed.
        let (grid, encoded) = match converted {
            Ok(v) => v,
            Err(e) => {
                write_log(
                    log,
                    format!("meteogate: converting ts={timestamp} failed: {e}"),
                );
                return Err(e);
            }
        };

        write_log(
            log,
            format!(
                "meteogate: converted ts={timestamp} -> frd {} bytes",
                encoded.len()
            ),
        );
        // A failed write only costs a re-convert next time; don't fail the frame.
        if let Err(e) = write_atomic(&path, &encoded) {
            write_log(log, format!("meteogate: caching frd ts={timestamp}: {e}"));
        }
        Ok(grid)
    }

    /// Fetch the GeoTIFF body, retrying transient failures.
    ///
    /// S3 is not rate limited for this bucket, so a failure here is almost
    /// always a dropped connection or a brief 5xx — worth a few quick retries
    /// rather than surfacing as a hole in the timeline.  A `404` is final and
    /// returns immediately: no amount of retrying publishes a frame.
    async fn download_geotiff(&self, timestamp: i64) -> Result<Vec<u8>> {
        let log = &self.dirs.log_path;
        let url = self.s3_url(timestamp);
        let mut delay = DOWNLOAD_RETRY_BASE;
        let mut last: Option<String> = None;

        for attempt in 1..=DOWNLOAD_ATTEMPTS {
            if self.cancel.load(Ordering::Relaxed) {
                color_eyre::eyre::bail!("cancelled");
            }
            write_log(log, format!("meteogate: downloading {url} (try {attempt})"));
            match self.client.get(&url).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        match resp.bytes().await {
                            Ok(b) => return Ok(b.to_vec()),
                            // A truncated body is worth another attempt.
                            Err(e) => last = Some(format!("read body: {e}")),
                        }
                    } else if matches!(
                        status,
                        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
                    ) {
                        color_eyre::eyre::bail!("MeteoGate GeoTIFF {url}: HTTP {status}");
                    } else {
                        if let Some(after) = retry_after(resp.headers()) {
                            delay = after.min(MAX_RETRY_AFTER);
                        }
                        last = Some(format!("HTTP {status}"));
                    }
                }
                Err(e) => last = Some(e.to_string()),
            }

            if attempt < DOWNLOAD_ATTEMPTS {
                write_log(
                    log,
                    format!(
                        "meteogate: download ts={timestamp} failed ({}) — retrying in {:?}",
                        last.as_deref().unwrap_or("unknown"),
                        delay
                    ),
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(MAX_RETRY_AFTER);
            }
        }
        color_eyre::eyre::bail!(
            "download MeteoGate GeoTIFF {url} after {DOWNLOAD_ATTEMPTS} attempts: {}",
            last.as_deref().unwrap_or("unknown")
        )
    }

    /// Send a HEAD request and classify the answer.
    ///
    /// Only a `404`/`410` counts as [`Probe::Absent`].  Everything else that
    /// isn't a success — timeout, connection error, `429`, any `5xx` — is
    /// [`Probe::Transient`], because none of it tells us whether the object is
    /// on S3.  `Retry-After` is honoured when the server sends one.
    async fn head_geotiff(&self, timestamp: i64) -> Probe {
        let url = self.s3_url(timestamp);
        write_log(&self.dirs.log_path, format!("meteogate: HEAD {url}"));
        match tokio::time::timeout(HEAD_TIMEOUT, self.client.head(&url).send()).await {
            Ok(Ok(resp)) => {
                let status = resp.status();
                write_log(
                    &self.dirs.log_path,
                    format!("meteogate: HEAD ts={timestamp} -> {status}"),
                );
                if status.is_success() {
                    Probe::Exists
                } else if matches!(
                    status,
                    reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
                ) {
                    Probe::Absent
                } else {
                    Probe::Transient(retry_after(resp.headers()).unwrap_or(TRANSIENT_RETRY))
                }
            }
            Ok(Err(e)) => {
                write_log(
                    &self.dirs.log_path,
                    format!("meteogate: HEAD ts={timestamp} error: {e}"),
                );
                Probe::Transient(TRANSIENT_RETRY)
            }
            Err(_) => {
                write_log(
                    &self.dirs.log_path,
                    format!("meteogate: HEAD ts={timestamp} timeout"),
                );
                Probe::Transient(TRANSIENT_RETRY)
            }
        }
    }

    /// Starting from `timestamp` (a 5‑min boundary), try HEAD requests
    /// backwards in 5‑min steps and return the newest that exists (up to
    /// 30 min / 7 attempts).
    /// Check whether the object for `ts` exists, consulting the probe
    /// cache first.  A locally cached grid counts as existing without
    /// any network traffic.
    async fn probe_geotiff(&self, ts: i64) -> Result<bool> {
        {
            let cache = self.probe_cache.lock().await;
            match cache.get(&ts) {
                Some(ProbeResult::Exists) => return Ok(true),
                Some(ProbeResult::Missing(at)) if at.elapsed() < MISSING_RETRY => return Ok(false),
                Some(ProbeResult::Unreachable(until)) if Instant::now() < *until => {
                    return Ok(false)
                }
                _ => {}
            }
        }
        // A grid already on disk is proof enough; never spend a request on it.
        let probe = if self.cache_path(ts).exists() {
            Probe::Exists
        } else {
            self.head_geotiff(ts).await
        };
        let mut cache = self.probe_cache.lock().await;
        cache.insert(
            ts,
            match probe {
                Probe::Exists => ProbeResult::Exists,
                Probe::Absent => ProbeResult::Missing(Instant::now()),
                Probe::Transient(wait) => {
                    ProbeResult::Unreachable(Instant::now() + wait.min(MAX_RETRY_AFTER))
                }
            },
        );
        // Keep the cache from growing without bound across long sessions.
        if cache.len() > 512 {
            cache.clear();
        }
        Ok(matches!(probe, Probe::Exists))
    }

    async fn resolve_nearest_available(&self, timestamp: i64) -> Result<i64> {
        let log = &self.dirs.log_path;

        // Fast path: the requested slot itself is published in the common
        // case, so check it alone before fanning out.  This also avoids
        // spending six needless HEADs on every frame of the timeline.
        if self.probe_geotiff(timestamp).await.unwrap_or(false) {
            write_log(
                log,
                format!("meteogate: resolved ts={timestamp} -> nearest={timestamp}"),
            );
            return Ok(timestamp);
        }

        let candidates: Vec<i64> = (5..=30).step_by(5).map(|o| timestamp - o * 60).collect();

        // Probe the fallback slots concurrently, but pick the winner by
        // candidate order rather than completion order: a probe that resolves
        // from cache finishes instantly and would otherwise beat a nearer slot
        // still awaiting its HEAD, silently pulling the frame further back in
        // time.  Waiting for the full round costs one 3 s timeout at worst.
        let probes = candidates.iter().map(|&ts| async move {
            match self.probe_geotiff(ts).await {
                Ok(true) => Some(ts),
                _ => None,
            }
        });

        if let Some(ts) = futures::future::join_all(probes)
            .await
            .into_iter()
            .flatten()
            .next()
        {
            write_log(
                log,
                format!("meteogate: resolved ts={timestamp} -> nearest={ts}"),
            );
            return Ok(ts);
        }
        write_log(
            log,
            format!(
                "meteogate: no GeoTIFF found for ts={timestamp} or any 5-min slot within 30 min"
            ),
        );
        color_eyre::eyre::bail!(
            "no GeoTIFF available for {timestamp} or any 5‑min slot within the past 30 min"
        );
    }
}

// ---------------------------------------------------------------------------
// Grid representation
// ---------------------------------------------------------------------------

/// Parse a `Retry-After` header into a delay.
///
/// Only the delta-seconds form is honoured; the HTTP-date form is rare on
/// object stores and not worth a date parser here — callers fall back to their
/// own default when this returns `None`.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Compute the list of expected radar frame timestamps spanning `hours` of
/// history, newest first.  Purely local — no network traffic — so the UI can
/// poll it cheaply to detect when a new slot opens.
///
/// Starts one slot back from the current 5-min boundary: the boundary
/// itself is the still-scanning slot and is never published yet, causing
/// the two most recent entries to resolve to the same file on S3.
pub fn compute_frame_list(hours: u8) -> Vec<i64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let latest = now - (now % SLOT_SECS);
    (0..frames_for_hours(hours) as i64)
        .map(|i| latest - SLOT_SECS - i * SLOT_SECS)
        .collect()
}

#[derive(Debug)]
struct RadarGrid {
    /// Row-major dBZ codes, `width * height` long.  Held as codes rather than
    /// f32: a tile build samples ~1 M of the 16.7 M pixels, so decoding at
    /// sample time is cheaper than expanding the whole grid, and keeps the
    /// resident grid at 16.7 MB instead of 66.9 MB.
    codes: Vec<u8>,
    width: u32,
    height: u32,
    /// LAEA easting of the top-left pixel centre.
    tie_x: f64,
    /// LAEA northing of the top-left pixel centre.
    tie_y: f64,
    /// East-west pixel size in metres (positive).
    scale_x: f64,
    /// North-south pixel size in metres (always positive;
    /// row index increases southward so northing = tie_y - row * scale_y).
    scale_y: f64,
}

impl RadarGrid {
    /// Convert (lat, lon) in degrees to the nearest grid (col, row).
    ///
    /// The GeoTIFF CRS uses a false easting of 1 950 000 m and false northing
    /// of –2 100 000 m, so `tie_x`/`tie_y` from the file are in *CRS* space.
    fn lat_lon_to_uv(&self, lat_deg: f64, lon_deg: f64) -> Option<(usize, usize)> {
        let (e, n) = laea_forward(lat_deg, lon_deg);
        // Shift from "true" LAEA to CRS space before comparing with the
        // tie point (which is also in CRS space).
        let crs_e = e + LAEA_FALSE_E;
        let crs_n = n + LAEA_FALSE_N;
        let col = ((crs_e - self.tie_x) / self.scale_x).round() as isize;
        let row = ((self.tie_y - crs_n) / self.scale_y).round() as isize;
        if col >= 0 && col < self.width as isize && row >= 0 && row < self.height as isize {
            Some((col as usize, row as usize))
        } else {
            None
        }
    }

    /// Sample dBZ value at (col, row). Returns `None` if the pixel is fill/no-data.
    fn sample(&self, col: usize, row: usize) -> Option<f32> {
        let code = self.codes[row * self.width as usize + col];
        code_to_dbz(code).filter(|v| *v >= MIN_DBZ)
    }

    /// Average dBZ over the `span_u` × `span_v` source footprint that one
    /// output pixel covers, centred on (`col`, `row`).
    ///
    /// Zoomed out, one screen pixel stands for a lot of radar: at tile zoom 4 a
    /// pixel spans roughly 46 source cells, and at zoom 1 nearer 2900.  Taking a
    /// single sample from that footprint — the old behaviour — meant whichever
    /// cell happened to land under the sample point decided the whole pixel, so
    /// small showers and the small gaps between them blinked in and out as the
    /// map moved rather than shrinking smoothly.
    ///
    /// The mean is taken in **linear reflectivity** (Z), not dBZ.  dBZ is
    /// logarithmic, so averaging it directly under-weights the strong returns
    /// that dominate a cell; converting to Z, averaging, and converting back is
    /// what the quantity actually means.
    ///
    /// No-data cells count as zero Z rather than being skipped.  That is what
    /// keeps gaps visible: a footprint half echo and half hole averages to about
    /// 3 dB less than a full one, instead of reading as solid echo.
    fn sample_area(&self, col: usize, row: usize, span_u: u32, span_v: u32) -> Option<f32> {
        if span_u <= 1 && span_v <= 1 {
            return self.sample(col, row);
        }
        // Cap the work per pixel: past a handful of samples per axis the result
        // stops changing visibly, but the cost keeps growing with the square.
        let step_u = span_u.div_ceil(MAX_FOOTPRINT_SAMPLES).max(1) as usize;
        let step_v = span_v.div_ceil(MAX_FOOTPRINT_SAMPLES).max(1) as usize;
        let half_u = (span_u / 2) as usize;
        let half_v = (span_v / 2) as usize;

        let u0 = col.saturating_sub(half_u);
        let v0 = row.saturating_sub(half_v);
        let u1 = (col + half_u).min(self.width as usize - 1);
        let v1 = (row + half_v).min(self.height as usize - 1);

        let mut z_sum = 0.0f64;
        let mut n = 0u32;
        let mut v = v0;
        while v <= v1 {
            let base = v * self.width as usize;
            let mut u = u0;
            while u <= u1 {
                // Read the code directly: `sample` applies the MIN_DBZ display
                // threshold, which must not be applied per-cell before the
                // average — that would erase the weak edges of a cell.
                if let Some(dbz) = code_to_dbz(self.codes[base + u]) {
                    z_sum += 10f64.powf(f64::from(dbz) / 10.0);
                }
                n += 1;
                u += step_u;
            }
            v += step_v;
        }
        if n == 0 {
            return None;
        }
        let z_mean = z_sum / f64::from(n);
        if z_mean <= 0.0 {
            return None;
        }
        let dbz = 10.0 * z_mean.log10();
        let dbz = dbz as f32;
        (dbz >= MIN_DBZ).then_some(dbz)
    }
}

// ---------------------------------------------------------------------------
// `.frd` container
// ---------------------------------------------------------------------------

/// Serialise a grid: fixed header, then the zstd-compressed code plane.
fn encode_frd(grid: &RadarGrid) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(grid.codes.len() / 16);
    out.extend_from_slice(FRD_MAGIC);
    out.extend_from_slice(&grid.width.to_le_bytes());
    out.extend_from_slice(&grid.height.to_le_bytes());
    out.extend_from_slice(&grid.tie_x.to_le_bytes());
    out.extend_from_slice(&grid.tie_y.to_le_bytes());
    out.extend_from_slice(&grid.scale_x.to_le_bytes());
    out.extend_from_slice(&grid.scale_y.to_le_bytes());
    let payload =
        zstd::encode_all(grid.codes.as_slice(), FRD_ZSTD_LEVEL).wrap_err("compress radar grid")?;
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Header layout: magic(4) + width(4) + height(4) + 4×f64 geotransform.
const FRD_HEADER_LEN: usize = 4 + 4 + 4 + 8 * 4;

/// Parse a `.frd` produced by [`encode_frd`].
///
/// Rejects an unknown magic/version rather than guessing, so a cache written by
/// an older build is refetched instead of misread.
fn decode_frd(bytes: &[u8]) -> Result<RadarGrid> {
    if bytes.len() < FRD_HEADER_LEN || &bytes[0..4] != FRD_MAGIC {
        color_eyre::eyre::bail!("not an frd grid (bad magic)");
    }
    let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
    let f64_at = |o: usize| f64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
    let width = u32_at(4);
    let height = u32_at(8);
    let tie_x = f64_at(12);
    let tie_y = f64_at(20);
    let scale_x = f64_at(28);
    let scale_y = f64_at(36);

    let expected = (width as usize) * (height as usize);
    let codes = zstd::decode_all(&bytes[FRD_HEADER_LEN..]).wrap_err("decompress radar grid")?;
    if codes.len() != expected {
        color_eyre::eyre::bail!(
            "frd payload is {} codes, header declares {width}x{height}={expected}",
            codes.len()
        );
    }
    Ok(RadarGrid {
        codes,
        width,
        height,
        tie_x,
        tie_y,
        scale_x,
        scale_y,
    })
}

// ---------------------------------------------------------------------------
// GeoTIFF parsing
// ---------------------------------------------------------------------------

/// Extract an f64 from a `tiff::decoder::ifd::Value`, dispatching numeric variants.
fn extract_f64(v: &ifd::Value) -> f64 {
    match *v {
        ifd::Value::Double(d) => d,
        ifd::Value::Float(f) => f as f64,
        ifd::Value::Short(u) => u as f64,
        ifd::Value::Unsigned(u) => u as f64,
        ifd::Value::Signed(i) => i as f64,
        ifd::Value::Byte(b) => b as f64,
        ref other => {
            // Fallback: try to string-parse (shouldn't happen with well-formed files)
            format!("{other:?}").parse().unwrap_or(0.0)
        }
    }
}

fn parse_geotiff(bytes: &[u8]) -> Result<RadarGrid> {
    let cursor = std::io::Cursor::new(bytes);
    let mut decoder = Decoder::new(cursor).wrap_err("open TIFF decoder")?;

    let (width, height) = decoder.dimensions().wrap_err("read TIFF dimensions")?;

    // Read geotransform: ModelTiepointTag and ModelPixelScaleTag
    let tie_x;
    let tie_y;
    match decoder
        .get_tag(Tag::ModelTiepointTag)
        .wrap_err("read ModelTiepointTag")?
    {
        ifd::Value::List(ref vals) if vals.len() >= 6 => {
            tie_x = extract_f64(&vals[3]);
            tie_y = extract_f64(&vals[4]);
        }
        other => {
            color_eyre::eyre::bail!("unexpected ModelTiepointTag value: {other:?}");
        }
    }

    let scale_x;
    let scale_y;
    match decoder
        .get_tag(Tag::ModelPixelScaleTag)
        .wrap_err("read ModelPixelScaleTag")?
    {
        ifd::Value::List(ref vals) if vals.len() >= 2 => {
            scale_x = extract_f64(&vals[0]).abs();
            scale_y = extract_f64(&vals[1]).abs();
        }
        other => {
            color_eyre::eyre::bail!("unexpected ModelPixelScaleTag value: {other:?}");
        }
    }

    // Read image data
    let image = decoder.read_image().wrap_err("read TIFF image data")?;
    let pixels: Vec<f32> = match image {
        DecodingResult::F32(buf) => buf,
        DecodingResult::F64(buf) => buf.iter().map(|&v| v as f32).collect(),
        DecodingResult::U16(buf) => buf.iter().map(|&v| v as f32).collect(),
        DecodingResult::I16(buf) => buf.iter().map(|&v| v as f32).collect(),
        _ => color_eyre::eyre::bail!("unsupported TIFF data type for radar grid"),
    };

    let npixels = (width as usize) * (height as usize);

    // Determine the inter-sample stride.
    //   SamplesPerPixel=1 → single band → stride = 1
    //   SamplesPerPixel>1 + Chunky → interleaved → stride = samples
    //   SamplesPerPixel>1 + Planar/Separate → tiff crate returns only
    //     band 0 → stride = 1
    let stride = {
        let samples = decoder
            .find_tag_unsigned::<u16>(Tag::SamplesPerPixel)
            .ok()
            .flatten()
            .unwrap_or(1) as usize;
        if samples <= 1 {
            1_usize
        } else {
            let planar = decoder
                .find_tag_unsigned::<u16>(Tag::PlanarConfiguration)
                .ok()
                .flatten()
                .unwrap_or(1); // default = Chunky (1)
            if planar == 2 {
                // Separate (planar) – crate returns only band 0
                1_usize
            } else {
                // Chunky (interleaved) – data has all bands interleaved
                samples
            }
        }
    };

    let expected = npixels * stride;
    if pixels.len() < expected {
        color_eyre::eyre::bail!(
            "TIFF: expected at least {expected} values for stride {stride}, got {got}",
            got = pixels.len()
        );
    }

    // Quantise straight to codes: only sample 0 of each pixel is reflectivity,
    // so with stride 2 this also drops the band we never read.
    let codes: Vec<u8> = (0..npixels)
        .map(|i| dbz_to_code(pixels[i * stride]))
        .collect::<Result<_>>()?;

    Ok(RadarGrid {
        codes,
        width,
        height,
        tie_x,
        tie_y,
        scale_x,
        scale_y,
    })
}

// ---------------------------------------------------------------------------
// Spherical LAEA forward projection  (lat/lon → easting/northing)
// Matches EPSG:3035 conventions.
// ---------------------------------------------------------------------------

fn laea_forward(lat_deg: f64, lon_deg: f64) -> (f64, f64) {
    let lat = lat_deg.to_radians();
    let lon = lon_deg.to_radians();

    let sin_phi = lat.sin();
    let cos_phi = lat.cos();
    let sin_phi1 = LAEA_LAT0.sin();
    let cos_phi1 = LAEA_LAT0.cos();

    let d_lon = lon - LAEA_LON0;
    let cos_d_lon = d_lon.cos();

    let denominator = 1.0 + sin_phi1 * sin_phi + cos_phi1 * cos_phi * cos_d_lon;
    let k = (2.0 / denominator).sqrt();

    let e = LAEA_R * k * cos_phi * d_lon.sin();
    let n = LAEA_R * k * (cos_phi1 * sin_phi - sin_phi1 * cos_phi * cos_d_lon);

    (e, n)
}

// ---------------------------------------------------------------------------
// Tile construction
// ---------------------------------------------------------------------------

/// Convert a tile pixel at (tx, ty) within tile `tc` at zoom `z` to (lat, lon).
fn tile_pixel_to_latlon(tc: TileCoord, z: u8, tx: u32, ty: u32) -> (f64, f64) {
    let n = 1u64 << z;
    let wx = (tc.x as f64 + tx as f64 / TILE_PX as f64) / n as f64;
    let wy = (tc.y as f64 + ty as f64 / TILE_PX as f64) / n as f64;
    let geo = world_to_lat_lon(WorldPoint { x: wx, y: wy });
    (geo.lat, geo.lon)
}

fn build_tile(grid: &RadarGrid, tc: TileCoord, z: u8) -> Result<RadarTile> {
    let (span_u, span_v) = footprint_cells(grid, tc, z);
    let mut rows = Vec::with_capacity(TILE_PX as usize);
    for py in 0..TILE_PX {
        rows.push(build_row(grid, tc, z, py, span_u, span_v));
    }
    Ok(RadarTile {
        coord: tc,
        size: TILE_PX,
        rows,
    })
}

/// How many source grid cells one output pixel of this tile covers, per axis.
///
/// Measured rather than derived from a zoom formula: the tile grid is Web
/// Mercator and the radar grid is LAEA, so the ratio depends on latitude and on
/// the angle between the two projections.  Stepping one pixel diagonally at the
/// tile centre and converting both ends to LAEA metres captures all of that.
fn footprint_cells(grid: &RadarGrid, tc: TileCoord, z: u8) -> (u32, u32) {
    let mid = TILE_PX / 2;
    let (lat_a, lon_a) = tile_pixel_to_latlon(tc, z, mid, mid);
    let (lat_b, lon_b) = tile_pixel_to_latlon(tc, z, mid + 1, mid + 1);
    let (xa, ya) = laea_forward(lat_a, lon_a);
    let (xb, yb) = laea_forward(lat_b, lon_b);
    let span = |d: f64, scale: f64| -> u32 {
        if !d.is_finite() || scale <= 0.0 {
            return 1;
        }
        ((d.abs() / scale).round() as u32).max(1)
    };
    (span(xb - xa, grid.scale_x), span(yb - ya, grid.scale_y))
}

fn build_row(
    grid: &RadarGrid,
    tc: TileCoord,
    z: u8,
    py: u32,
    span_u: u32,
    span_v: u32,
) -> Vec<RadarRun> {
    let mut runs: Vec<RadarRun> = Vec::new();
    let mut current: Option<(Rgb8, u8)> = None;
    let mut start_x: u16 = 0;

    for px in 0..TILE_PX {
        let (lat, lon) = tile_pixel_to_latlon(tc, z, px, py);
        let value: Option<(Rgb8, u8)> = match grid.lat_lon_to_uv(lat, lon) {
            Some((col, row)) => grid.sample_area(col, row, span_u, span_v).map(dbz_to_color),
            None => None,
        };

        if value != current {
            if let Some((color, intensity)) = current {
                runs.push(RadarRun {
                    start_x,
                    end_x: px as u16,
                    color,
                    intensity,
                });
            }
            current = value;
            start_x = px as u16;
        }
    }

    if let Some((color, intensity)) = current {
        runs.push(RadarRun {
            start_x,
            end_x: TILE_PX as u16,
            color,
            intensity,
        });
    }

    runs
}

/// Map a dBZ value to a display colour and Braille intensity (1–14).
fn dbz_to_color(dbz: f32) -> (Rgb8, u8) {
    // OPERA dBZ → colour mapping (standard weather radar palette):
    //   < 0  → transparent (handled by caller)
    //   0–5  → light blue (very light)
    //   5–15 → medium blue (light)
    //  15–25 → lighter green (medium)
    //  25–35 → green (medium)
    //  35–40 → yellow-green (medium)
    //  40–45 → yellow (medium)
    //  45–50 → orange (medium-high)
    //  50–55 → red (high)
    //  55–60 → magenta (very high)
    //  60+   → white (extreme)
    let (r, g, b) = if dbz < 5.0 {
        (140, 180, 255)
    } else if dbz < 15.0 {
        (80, 140, 255)
    } else if dbz < 25.0 {
        (100, 220, 100)
    } else if dbz < 35.0 {
        (40, 180, 40)
    } else if dbz < 40.0 {
        (180, 220, 40)
    } else if dbz < 45.0 {
        (220, 220, 40)
    } else if dbz < 50.0 {
        (240, 160, 40)
    } else if dbz < 55.0 {
        (220, 60, 40)
    } else if dbz < 60.0 {
        (200, 40, 180)
    } else {
        (200, 200, 200)
    };
    // Intensity proportional to dBZ (higher = brighter dot)
    let intensity = (dbz.clamp(0.0, 70.0) / 5.0) as u8 + 1;
    (Rgb8::new(r, g, b), intensity.min(14))
}

fn radar_zoom(zoom: f64) -> u8 {
    zoom.round().clamp(1.0, 7.0) as u8
}

// ---------------------------------------------------------------------------
// Date-time helper
// ---------------------------------------------------------------------------

struct DateTimeStr {
    year: i32,
    month: u32,
    day: u32,
    str: String,
}

fn time_to_datetime(timestamp: i64) -> DateTimeStr {
    use chrono::TimeZone;
    let dt = chrono::Utc
        .timestamp_opt(timestamp, 0)
        .single()
        .unwrap_or_default();
    DateTimeStr {
        year: dt.year(),
        month: dt.month(),
        day: dt.day(),
        str: dt.format("%Y%m%dT%H%M").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a real OPERA GeoTIFF downloaded from S3 and verify grid
    /// dimensions, tiepoint, and pixel sampling.
    #[test]
    fn parse_actual_geotiff() -> Result<()> {
        // Integration fixture, not checked in — skip when absent so the
        // suite passes on machines without the sample file.
        let Ok(bytes) = std::fs::read("/tmp/test_dbzh.tiff") else {
            eprintln!("skipping parse_actual_geotiff: /tmp/test_dbzh.tiff not present");
            return Ok(());
        };
        let grid = parse_geotiff(&bytes)?;

        assert_eq!(grid.width, 3800, "width");
        assert_eq!(grid.height, 4400, "height");
        assert!((grid.tie_x + 500.000_271_433_265_9).abs() < 0.01, "tie_x");
        assert!((grid.tie_y - 499.999_912_387_225_8).abs() < 0.01, "tie_y");
        assert!((grid.scale_x - 1000.0).abs() < 0.01, "scale_x");
        assert!((grid.scale_y - 1000.0).abs() < 0.01, "scale_y");

        // Spot-check a few pixels: sample somewhere in the middle of the
        // grid where we expect valid data (dBZ values).
        let mid_col = grid.width as usize / 2;
        let mid_row = grid.height as usize / 2;

        // Most pixels should be fill/no-data (radar only covers land),
        // but the function should not panic and should return Some or None
        // for valid positions.
        let sample = grid.sample(mid_col, mid_row);
        // Either None (no data at this location) or a reasonable dBZ
        if let Some(v) = sample {
            assert!(
                (1.0..=100.0).contains(&v),
                "sample {v} out of reasonable dBZ range"
            );
        }

        // Verify lat_lon_to_uv for a point that should be in range
        // (roughly central Europe, say 50°N, 10°E)
        if let Some((col, row)) = grid.lat_lon_to_uv(50.0, 10.0) {
            assert!(col < grid.width as usize);
            assert!(row < grid.height as usize);
        }

        Ok(())
    }

    #[test]
    fn every_opera_dbz_step_round_trips_exactly() {
        // OPERA quantises to 0.5 dBZ; the whole u8 range must survive a
        // round-trip, since that exactness is what makes the format lossless
        // rather than a cheap approximation.
        for code in 1..=255u8 {
            let dbz = code_to_dbz(code).expect("non-zero code has a value");
            assert_eq!(dbz_to_code(dbz).unwrap(), code, "dBZ {dbz} lost its code");
        }
        assert_eq!(code_to_dbz(0), None, "code 0 is no-data");
    }

    #[test]
    fn nodata_and_sentinel_encode_to_the_nodata_code() {
        assert_eq!(dbz_to_code(f32::NAN).unwrap(), 0);
        assert_eq!(dbz_to_code(-9_999_000.0).unwrap(), 0);
        assert_eq!(dbz_to_code(f32::NEG_INFINITY).unwrap(), 0);
    }

    #[test]
    fn off_grid_values_fail_loudly_rather_than_rounding() {
        // A value between two steps would otherwise be silently snapped, which
        // is how a quantisation change upstream would corrupt pixels unnoticed.
        assert!(
            dbz_to_code(10.25).is_err(),
            "off-grid value must be rejected"
        );
        // Above the ceiling is still a hard error: it would mean the product
        // changed shape, not that the weather was quiet.
        assert!(dbz_to_code(200.0).is_err());
    }

    /// Weak echo below the encodable floor is folded into no-data instead of
    /// failing.  OPERA emits a handful of such pixels per composite, and
    /// rejecting them aborted the whole frame — roughly three quarters of all
    /// frames never decoded, leaving permanent gaps in the timeline.
    #[test]
    fn sub_floor_values_become_nodata_rather_than_failing_the_frame() {
        // The exact values observed in three consecutive failing composites.
        for v in [-32.5_f32, -35.0, -100.0] {
            assert_eq!(
                dbz_to_code(v).expect("must not fail the frame"),
                0,
                "{v} dBZ should read as undetect"
            );
        }
        // The floor itself still encodes normally.
        assert_eq!(dbz_to_code(DBZ_BASE).unwrap(), 1);
    }

    /// A single sub-floor pixel must not stop a grid from encoding — that one
    /// pixel in ~628 000 is exactly what used to kill an entire frame.
    #[test]
    fn one_sub_floor_pixel_does_not_abort_the_grid() {
        let mut values = vec![10.0_f32; 64];
        values[37] = -35.0;
        let codes: Result<Vec<u8>> = values.iter().map(|&v| dbz_to_code(v)).collect();
        let codes = codes.expect("grid with one sub-floor pixel must still encode");
        assert_eq!(codes[37], 0, "the outlier reads as undetect");
        assert!(codes.iter().filter(|&&c| c != 0).count() == 63);
    }

    #[test]
    fn frd_round_trips_grid_and_geotransform() {
        let mut codes = vec![0u8; 64 * 32];
        for (i, c) in codes.iter_mut().enumerate() {
            *c = (i % 256) as u8;
        }
        let grid = RadarGrid {
            codes: codes.clone(),
            width: 64,
            height: 32,
            tie_x: -500.5,
            tie_y: 500.25,
            scale_x: 1000.0,
            scale_y: 2000.0,
        };
        let encoded = encode_frd(&grid).expect("encode");
        let back = decode_frd(&encoded).expect("decode");
        assert_eq!(back.codes, codes);
        assert_eq!((back.width, back.height), (64, 32));
        assert_eq!(back.tie_x, -500.5);
        assert_eq!(back.tie_y, 500.25);
        assert_eq!(back.scale_x, 1000.0);
        assert_eq!(back.scale_y, 2000.0);
    }

    #[test]
    fn frd_rejects_foreign_and_truncated_files() {
        // A GeoTIFF (or anything else) must not be mistaken for an frd.
        assert!(decode_frd(b"II*\0garbage").is_err());
        assert!(decode_frd(b"").is_err());
        // Right magic, payload shorter than the header claims.
        let grid = RadarGrid {
            codes: vec![7u8; 16],
            width: 4,
            height: 4,
            tie_x: 0.0,
            tie_y: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
        };
        let mut enc = encode_frd(&grid).unwrap();
        enc.truncate(enc.len() - 1);
        assert!(
            decode_frd(&enc).is_err(),
            "truncated payload must not decode"
        );
    }

    #[test]
    fn frd_payload_beats_the_source_geotiff_on_size() {
        // Realistic shape: mostly no-data with banded echo, like a real sweep.
        let (w, h) = (3800u32, 4400u32);
        let mut codes = vec![0u8; (w * h) as usize];
        for row in 0..h as usize {
            for col in 0..w as usize {
                if (row / 40 + col / 40) % 7 == 0 {
                    codes[row * w as usize + col] = 60 + (row % 32) as u8;
                }
            }
        }
        let grid = RadarGrid {
            codes,
            width: w,
            height: h,
            tie_x: 0.0,
            tie_y: 0.0,
            scale_x: 1000.0,
            scale_y: 1000.0,
        };
        let encoded = encode_frd(&grid).expect("encode");
        // Source frames measure ~2.8 MB; the point of the format is to land
        // well under that while decoding ~60x faster.
        assert!(
            encoded.len() < 2_800_000,
            "frd payload {} bytes should undercut the source geotiff",
            encoded.len()
        );
        assert_eq!(decode_frd(&encoded).unwrap().codes, grid.codes);
    }

    // ── footprint averaging (software decimation) ──────────────────────

    /// A small grid whose codes are produced by `f(col, row)` in dBZ.
    fn grid_from(w: u32, h: u32, f: impl Fn(usize, usize) -> Option<f32>) -> RadarGrid {
        let mut codes = vec![0u8; (w * h) as usize];
        for row in 0..h as usize {
            for col in 0..w as usize {
                if let Some(dbz) = f(col, row) {
                    codes[row * w as usize + col] = dbz_to_code(dbz).unwrap();
                }
            }
        }
        RadarGrid {
            codes,
            width: w,
            height: h,
            tie_x: 0.0,
            tie_y: 0.0,
            scale_x: 1000.0,
            scale_y: 1000.0,
        }
    }

    /// A span of 1 must behave exactly like the old point sample, so zoomed
    /// fully in nothing changes.
    #[test]
    fn unit_footprint_is_the_plain_sample() {
        let g = grid_from(8, 8, |c, _| Some(10.0 + c as f32));
        for col in 0..8 {
            assert_eq!(g.sample_area(col, 3, 1, 1), g.sample(col, 3));
        }
    }

    /// The regression this exists for: a hole surrounded by echo must still
    /// read as weaker than solid echo once decimated, rather than being filled
    /// in or dropped depending on where the sample point landed.
    #[test]
    fn a_gap_inside_echo_survives_decimation() {
        // Uniform 40 dBZ.
        let solid = grid_from(16, 16, |_, _| Some(40.0));
        // Same, but a quarter of the cells are holes.
        let holey = grid_from(16, 16, |c, r| ((c + r) % 4 != 0).then_some(40.0));

        let a = solid.sample_area(8, 8, 8, 8).unwrap();
        let b = holey.sample_area(8, 8, 8, 8).unwrap();
        assert!(
            b < a - 0.5,
            "a gappy footprint ({b} dBZ) must read weaker than a solid one ({a} dBZ)"
        );
    }

    /// A lone strong cell in an empty footprint must not vanish — it should
    /// register as weaker echo, not as nothing at all.
    #[test]
    fn an_isolated_cell_is_attenuated_not_erased() {
        let g = grid_from(16, 16, |c, r| (c == 8 && r == 8).then_some(55.0));
        let v = g
            .sample_area(8, 8, 4, 4)
            .expect("isolated cell must survive");
        assert!(
            v < 55.0,
            "should be attenuated by the surrounding emptiness"
        );
        assert!(v >= MIN_DBZ, "but must stay above the display threshold");
    }

    /// Averaging happens in linear Z, not dBZ.  Halving the *area* covered by a
    /// given echo drops it by ~3 dB; a naive dBZ mean would instead halve the
    /// dBZ number, which is a completely different (and wrong) answer.
    #[test]
    fn averaging_is_linear_in_z_not_in_dbz() {
        // Half the cells at 40 dBZ, half empty.
        let g = grid_from(16, 16, |c, _| (c % 2 == 0).then_some(40.0));
        let v = g.sample_area(8, 8, 8, 8).unwrap();
        assert!(
            (v - 37.0).abs() < 0.6,
            "half coverage of 40 dBZ should read ~37 dBZ, got {v}"
        );
    }

    /// An entirely empty footprint stays empty — decimation must not invent
    /// echo where the radar saw none.
    #[test]
    fn an_empty_footprint_stays_empty() {
        let g = grid_from(16, 16, |_, _| None);
        assert_eq!(g.sample_area(8, 8, 8, 8), None);
    }

    /// Uniform echo is preserved exactly, whatever the footprint size.
    #[test]
    fn uniform_echo_survives_any_footprint() {
        let g = grid_from(64, 64, |_, _| Some(30.0));
        for span in [1, 2, 5, 16, 40] {
            let v = g.sample_area(32, 32, span, span).unwrap();
            assert!((v - 30.0).abs() < 0.2, "span {span} gave {v}");
        }
    }

    /// The sample cap bounds cost without changing the answer much: a huge
    /// footprint over uniform echo must still read as that echo.
    #[test]
    fn oversized_footprints_are_capped_but_still_correct() {
        let g = grid_from(200, 200, |_, _| Some(25.0));
        let v = g.sample_area(100, 100, 180, 180).unwrap();
        assert!((v - 25.0).abs() < 0.2, "got {v}");
    }

    /// Footprints near the grid edge must clamp rather than panic.
    #[test]
    fn footprint_clamps_at_grid_edges() {
        let g = grid_from(16, 16, |_, _| Some(20.0));
        for (c, r) in [(0, 0), (15, 15), (0, 15), (15, 0)] {
            assert!(g.sample_area(c, r, 12, 12).is_some());
        }
    }

    /// Verify that `lat_lon_to_uv` maps the LAEA origin (55°N, 10°E) to the
    /// centre of the grid (~pixel 1950, 2100) when accounting for false
    /// easting/northing.
    #[test]
    fn lat_lon_to_uv_maps_origin_to_center() {
        // Build a minimal grid with parameters matching the real GeoTIFF.
        let grid = RadarGrid {
            codes: vec![0u8; 3800 * 4400],
            width: 3800,
            height: 4400,
            tie_x: -500.0, // CRS easting of pixel (0,0)
            tie_y: 500.0,  // CRS northing of pixel (0,0)
            scale_x: 1000.0,
            scale_y: 1000.0,
        };

        // LAEA origin (55°N, 10°E) should map to roughly (1950, 2100).
        let (col, row) = grid.lat_lon_to_uv(55.0, 10.0).expect("origin in grid");
        assert!(
            (col as isize - 1950).abs() <= 2,
            "origin col {col} not near 1950"
        );
        assert!(
            (row as isize - 2100).abs() <= 2,
            "origin row {row} not near 2100"
        );
    }

    /// Verify that a point far outside the grid returns `None`.
    #[test]
    fn lat_lon_to_uv_outside_returns_none() {
        let grid = RadarGrid {
            codes: vec![0u8; 3800 * 4400],
            width: 3800,
            height: 4400,
            tie_x: -500.0,
            tie_y: 500.0,
            scale_x: 1000.0,
            scale_y: 1000.0,
        };
        // South Pacific — far outside the OPERA grid
        assert!(grid.lat_lon_to_uv(-30.0, -150.0).is_none());
    }

    #[test]
    fn laea_origin_is_zero() {
        // At the projection centre (55°N, 10°E) the LAEA coordinates
        // should be (0, 0).
        let (e, n) = laea_forward(55.0, 10.0);
        assert!(
            e.abs() < 0.001,
            "easting {e} not near zero at projection centre"
        );
        assert!(
            n.abs() < 0.001,
            "northing {n} not near zero at projection centre"
        );
    }

    #[test]
    fn laea_symmetric_east_west() {
        // 1° east and 1° west of centre should give equal easting magnitude.
        let (e1, _) = laea_forward(55.0, 11.0);
        let (e2, _) = laea_forward(55.0, 9.0);
        let diff = (e1 + e2).abs();
        assert!(
            diff < 0.01,
            "easting not symmetric: {e1} vs {e2}, diff={diff}"
        );
    }

    #[test]
    fn dbz_to_color_thresholds_map_to_expected_hues() {
        // Very light rain: blue family
        let (c, i) = dbz_to_color(2.0);
        assert!(c.b > c.r && c.b > c.g, "< 5 dBZ should be blue-dominant");
        assert!(i >= 1);

        // Light rain: still blue but darker
        let (c2, _) = dbz_to_color(10.0);
        assert!(c2.b > c2.r);

        // Moderate rain: green family
        let (c3, _) = dbz_to_color(20.0);
        assert!(c3.g > c3.r && c3.g > c3.b);

        // Heavy rain: red family
        let (c4, _) = dbz_to_color(52.0);
        assert!(c4.r > c4.b);

        // Extreme: white-ish (all channels ≥ 128)
        let (c5, i5) = dbz_to_color(65.0);
        assert!(c5.r >= 128 && c5.g >= 128 && c5.b >= 128);
        assert_eq!(i5, 14, "intensity caps at 14");
    }

    #[test]
    fn dbz_to_color_intensity_increases_with_dbz() {
        let (_, i_low) = dbz_to_color(5.0);
        let (_, i_high) = dbz_to_color(45.0);
        assert!(i_high > i_low, "higher dBZ must give higher intensity");
    }

    #[test]
    fn radar_zoom_clamps_and_rounds() {
        assert_eq!(radar_zoom(0.0), 1, "below min clamps to 1");
        assert_eq!(radar_zoom(8.0), 7, "above max clamps to 7");
        assert_eq!(radar_zoom(4.4), 4, "rounds down");
        assert_eq!(radar_zoom(4.6), 5, "rounds up");
    }

    // ── retry / probe classification ───────────────────────────────────

    #[test]
    fn retry_after_parses_delta_seconds() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::RETRY_AFTER, "30".parse().unwrap());
        assert_eq!(retry_after(&h), Some(Duration::from_secs(30)));
    }

    /// The HTTP-date form is not parsed; callers must fall back to their own
    /// default rather than treating a `None` as "retry immediately".
    #[test]
    fn retry_after_ignores_the_http_date_form() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2026 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(retry_after(&h), None);
    }

    #[test]
    fn retry_after_absent_is_none() {
        assert_eq!(retry_after(&reqwest::header::HeaderMap::new()), None);
    }

    /// A transient failure must be retried far sooner than a confirmed
    /// absence — that difference is the whole point of splitting the two.
    #[test]
    fn transient_failures_are_retried_sooner_than_confirmed_absences() {
        assert!(
            TRANSIENT_RETRY < MISSING_RETRY,
            "an unreachable slot is likely present; a 404 slot is known not to be"
        );
    }

    /// A server-supplied Retry-After must not be able to stall radar for the
    /// rest of the session.
    #[test]
    fn retry_after_is_capped() {
        let huge = Duration::from_secs(86_400);
        assert_eq!(huge.min(MAX_RETRY_AFTER), MAX_RETRY_AFTER);
    }

    #[test]
    fn compute_frame_list_returns_descending_5min_steps() {
        let frames = compute_frame_list(DEFAULT_HISTORY_HOURS);
        assert_eq!(frames.len(), 36, "3 h at 5-min cadence");
        // Each frame is 300 s before the previous.
        for w in frames.windows(2) {
            assert_eq!(w[0] - w[1], 300, "frames must be 5 min apart");
        }
        // Most recent frame is a multiple of 300.
        assert_eq!(frames[0] % 300, 0, "latest frame aligned to 5 min boundary");
    }

    #[test]
    fn frame_list_length_tracks_history_depth() {
        for hours in HISTORY_OPTIONS {
            assert_eq!(
                compute_frame_list(hours).len(),
                frames_for_hours(hours),
                "{hours} h list must span the requested depth"
            );
        }
        assert_eq!(frames_for_hours(24), 288, "24 h is the bucket's retention");
    }

    #[test]
    fn history_cycle_wraps_and_recovers_from_bogus_values() {
        assert_eq!(next_history_hours(3), 6);
        assert_eq!(next_history_hours(6), 12);
        assert_eq!(next_history_hours(12), 24);
        assert_eq!(next_history_hours(24), 3, "wraps back to the shortest");
        // A value never offered (e.g. hand-edited state.toml) must not wedge
        // the cycle — it snaps back to the default.
        assert_eq!(next_history_hours(7), DEFAULT_HISTORY_HOURS);
    }
}
