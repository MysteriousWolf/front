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

use crate::cache::{read_if_exists, write_atomic, write_log, FrontDirs};
use crate::config::MeteoGateConfig;
use crate::layers::Rgb8;
use crate::retry::RetryPolicy;

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
/// one network blip convince a caller that a slot was absent, which silently
/// pulled the displayed frame backwards in time.
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

/// In-RAM cache of compact `.frd` bytes, keyed by timestamp.
///
/// Unlike [`GridCache`] (bounded to [`MAX_CACHED_GRIDS`] decoded grids at
/// ~16.7 MB each), `.frd` bytes are ~0.8 MB — cheap enough to keep resident
/// for the whole playback window. This map does not evict on its own;
/// `App::trigger_field_preload` calls [`MeteoGateProvider::prune_field_window`]
/// with the current window so RAM doesn't grow across a long session. A plain
/// `std::sync::Mutex` (not `tokio::sync::Mutex`) is deliberate: every access is
/// a quick map operation with no `.await` held across the lock, so callers on
/// the sync UI path (`App::request_field_refresh`'s warm fast path) can read
/// it without needing an async context.
type FrdCache = Arc<std::sync::Mutex<HashMap<i64, Arc<Vec<u8>>>>>;

#[derive(Debug, Clone)]
pub struct MeteoGateProvider {
    client: Client,
    dirs: FrontDirs,
    config: MeteoGateConfig,
    /// In-memory cache of the decoded LAEA radar grid.  Keyed by
    /// timestamp so zoom changes within the same frame don't re-decode
    /// the grid from disk.
    grid_cache: GridCache,
    /// In-memory cache of compact `.frd` bytes, keyed by timestamp. See
    /// [`FrdCache`].
    frd_cache: FrdCache,
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
            frd_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
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

    /// Returns a samplable [`RadarField`] for `timestamp`, fetching and
    /// decoding the grid if it isn't already cached (see [`Self::load_grid`]).
    pub async fn field(&self, timestamp: i64) -> Result<RadarField> {
        Ok(RadarField {
            grid: self.load_grid(timestamp).await?,
        })
    }

    /// Ensure `timestamp`'s `.frd` bytes are resident in RAM, without
    /// decoding a grid. This is what the field preload calls to warm a
    /// playback-window timestamp cheaply — no `MAX_CACHED_GRIDS`-sized
    /// decode, just the compact bytes.
    pub async fn warm_field(&self, timestamp: i64) -> Result<()> {
        self.frd_bytes(timestamp).await.map(|_| ())
    }

    /// True when `timestamp`'s `.frd` bytes are already resident in RAM —
    /// no disk or network access needed to serve it.
    pub fn field_is_warm(&self, timestamp: i64) -> bool {
        self.frd_cache.lock().unwrap().contains_key(&timestamp)
    }

    /// Drop RAM-resident `.frd` bytes for timestamps outside `keep`.
    ///
    /// `.frd` bytes are cheap enough (~0.8 MB) to hold for the whole
    /// playback window, but frames that have scrolled out of the window
    /// entirely shouldn't accumulate in RAM for the rest of the session.
    pub fn prune_field_window(&self, keep: &HashSet<i64>) {
        self.frd_cache
            .lock()
            .unwrap()
            .retain(|ts, _| keep.contains(ts));
    }

    /// Serve `timestamp`'s field synchronously when it is already warm: found
    /// in the decoded-grid LRU, or decodable from RAM-cached `.frd` bytes
    /// without touching disk or the network. `None` when the field would
    /// need an async fetch — the caller falls back to [`Self::field`].
    ///
    /// This is the no-refetch-on-warm fast path: timeline stepping onto an
    /// already-loaded frame swaps `current_field` this tick instead of a
    /// tick later via the async round trip.
    pub fn field_warm(&self, timestamp: i64) -> Option<RadarField> {
        // `try_lock` rather than `lock`: this is a sync fast path called from
        // the UI thread, and the tokio grid-cache mutex is only ever held
        // briefly (no `.await` inside the critical section anywhere it's
        // used), so contention should be near-instant — a permanently absent
        // grid cache hit here just falls through to the byte-cache path.
        if let Ok(mut cache) = self.grid_cache.try_lock() {
            if let Some(pos) = cache.iter().position(|(ts, _)| *ts == timestamp) {
                let entry = cache.remove(pos);
                let grid = Arc::clone(&entry.1);
                cache.push(entry);
                return Some(RadarField { grid });
            }
        }
        let bytes = self.frd_cache.lock().unwrap().get(&timestamp).cloned()?;
        let grid = Arc::new(decode_frd(&bytes).ok()?);
        if let Ok(mut cache) = self.grid_cache.try_lock() {
            if let Some(pos) = cache.iter().position(|(ts, _)| *ts == timestamp) {
                cache.remove(pos);
            }
            cache.push((timestamp, Arc::clone(&grid)));
            while cache.len() > MAX_CACHED_GRIDS {
                cache.remove(0);
            }
        }
        Some(RadarField { grid })
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

    /// Produce the grid for `timestamp`, from RAM/disk `.frd` bytes when
    /// present and otherwise by fetching the GeoTIFF and converting it.
    ///
    /// A RAM hit decodes in-process with no I/O at all; a disk hit
    /// decompresses one zstd payload (~3 ms). The cold path still pays the
    /// GeoTIFF's ~150 ms inflate, but only ever once per frame: the
    /// conversion is written out and cached in RAM so no later load repeats
    /// it.
    async fn load_grid_uncached(&self, timestamp: i64) -> Result<RadarGrid> {
        let (bytes, fresh_grid) = self.frd_bytes(timestamp).await?;
        // `parse_geotiff` already produced a grid when the bytes were freshly
        // downloaded — decoding the just-encoded bytes back would be pure
        // waste, so only decode when the bytes came from RAM or disk.
        if let Some(grid) = fresh_grid {
            return Ok(grid);
        }
        let log = &self.dirs.log_path;
        let decoded = {
            let bytes = Arc::clone(&bytes);
            tokio::task::spawn_blocking(move || decode_frd(&bytes))
                .await
                .wrap_err("decode_frd task panicked")?
        };
        match decoded {
            Ok(g) => Ok(g),
            // A truncated or older-format file: drop the RAM and disk copies
            // and fetch fresh rather than failing the frame.
            Err(e) => {
                write_log(
                    log,
                    format!("meteogate: discarding unreadable frd ts={timestamp}: {e}"),
                );
                self.frd_cache.lock().unwrap().remove(&timestamp);
                let _ = std::fs::remove_file(self.cache_path(timestamp));
                let (grid, encoded) = self.download_and_convert(timestamp).await?;
                let encoded = Arc::new(encoded);
                if let Err(e) = write_atomic(&self.cache_path(timestamp), &encoded) {
                    write_log(log, format!("meteogate: caching frd ts={timestamp}: {e}"));
                }
                self.frd_cache.lock().unwrap().insert(timestamp, encoded);
                Ok(grid)
            }
        }
    }

    /// Ensure `.frd` bytes for `timestamp` are cached in RAM, returning them.
    ///
    /// Checked in priority order: RAM cache, on-disk `.frd`, then a fresh
    /// GeoTIFF download. Also returns a decoded grid for free when the bytes
    /// had to be freshly parsed from a GeoTIFF download, since
    /// `parse_geotiff` already produced one.
    async fn frd_bytes(&self, timestamp: i64) -> Result<(Arc<Vec<u8>>, Option<RadarGrid>)> {
        if let Some(bytes) = self.frd_cache.lock().unwrap().get(&timestamp).cloned() {
            write_log(
                &self.dirs.log_path,
                format!("meteogate: frd ram cache hit ts={timestamp}"),
            );
            return Ok((bytes, None));
        }
        let path = self.cache_path(timestamp);
        if let Some(raw) = read_if_exists(&path)? {
            let bytes = Arc::new(raw);
            self.frd_cache
                .lock()
                .unwrap()
                .insert(timestamp, Arc::clone(&bytes));
            write_log(
                &self.dirs.log_path,
                format!("meteogate: frd disk cache hit ts={timestamp}"),
            );
            return Ok((bytes, None));
        }
        let (grid, encoded) = self.download_and_convert(timestamp).await?;
        let bytes = Arc::new(encoded);
        if let Err(e) = write_atomic(&path, &bytes) {
            write_log(
                &self.dirs.log_path,
                format!("meteogate: caching frd ts={timestamp}: {e}"),
            );
        }
        self.frd_cache
            .lock()
            .unwrap()
            .insert(timestamp, Arc::clone(&bytes));
        Ok((bytes, Some(grid)))
    }

    /// Fetch and decode the GeoTIFF for `timestamp`, converting it to a grid
    /// plus its `.frd` encoding. Used both for a fresh download and to
    /// recover from a corrupted on-disk `.frd`.
    async fn download_and_convert(&self, timestamp: i64) -> Result<(RadarGrid, Vec<u8>)> {
        let log = &self.dirs.log_path;
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
        Ok((grid, encoded))
    }

    /// Fetch the GeoTIFF body, retrying transient failures.
    ///
    /// S3 is not rate limited for this bucket, so a failure here is almost
    /// always a dropped connection or a brief 5xx — worth a few quick retries
    /// rather than surfacing as a hole in the timeline.  A `404` is final and
    /// returns immediately: no amount of retrying publishes a frame.
    async fn download_geotiff(&self, timestamp: i64) -> Result<Vec<u8>> {
        const POLICY: RetryPolicy = RetryPolicy::new(
            DOWNLOAD_RETRY_BASE,
            MAX_RETRY_AFTER,
            Some(DOWNLOAD_ATTEMPTS),
        );
        let log = &self.dirs.log_path;
        let url = self.s3_url(timestamp);
        // `delay` is the next sleep duration, seeded at the policy base and
        // advanced by `POLICY.double` after each sleep. A server `Retry-After`
        // redirects it: the override is used for its own sleep *and* becomes
        // the new baseline the doubling continues from — matching the original
        // mutating-`delay` loop rather than snapping back to the pure
        // attempt-indexed sequence. See docs/spec/task-system-unification.md.
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
                delay = POLICY.double(delay);
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
    /// Fractional grid position of a (lat, lon), before rounding to a cell.
    fn lat_lon_to_colrow(&self, lat_deg: f64, lon_deg: f64) -> (f64, f64) {
        let (e, n) = laea_forward(lat_deg, lon_deg);
        let crs_e = e + LAEA_FALSE_E;
        let crs_n = n + LAEA_FALSE_N;
        (
            (crs_e - self.tie_x) / self.scale_x,
            (self.tie_y - crs_n) / self.scale_y,
        )
    }

    /// Bilinearly interpolate dBZ at a fractional (lat, lon).
    ///
    /// The source is a 1 km grid but a z=7 tile pixel is ~0.8 km, so a
    /// magnifying nearest-neighbour sample paints the same 1 km cell across
    /// several output pixels — the blocky "radar pixels" seen when zoomed in.
    /// Interpolating the four surrounding cells instead makes band boundaries
    /// follow the field smoothly rather than the grid.
    ///
    /// The mean is taken in **linear reflectivity** (Z),
    /// not dBZ, and no-data cells count as zero Z rather than being skipped —
    /// so echo edges fade into gaps instead of stepping to a hard border.
    fn sample_bilinear(&self, lat_deg: f64, lon_deg: f64) -> Option<f32> {
        let (cf, rf) = self.lat_lon_to_colrow(lat_deg, lon_deg);
        self.sample_bilinear_at(cf, rf)
    }

    /// Interpolation core of [`sample_bilinear`], split out so it can be
    /// tested on fractional grid coordinates without round-tripping LAEA.
    fn sample_bilinear_at(&self, cf: f64, rf: f64) -> Option<f32> {
        // Interpolate between cell *centres*: a sample sitting exactly on a
        // centre must reproduce that cell, so the box spans floor..floor+1.
        let c0 = cf.floor();
        let r0 = rf.floor();
        if c0 < 0.0 || r0 < 0.0 {
            return None;
        }
        let (w, h) = (self.width as usize, self.height as usize);
        let c0 = c0 as usize;
        let r0 = r0 as usize;
        if c0 >= w || r0 >= h {
            return None;
        }
        // Clamp the far corner so the last row/column degrades to nearest
        // rather than reading out of bounds.
        let c1 = (c0 + 1).min(w - 1);
        let r1 = (r0 + 1).min(h - 1);
        let fx = cf - c0 as f64;
        let fy = rf - r0 as f64;

        let z_at = |col: usize, row: usize| -> f64 {
            match code_to_dbz(self.codes[row * w + col]) {
                Some(dbz) => 10f64.powf(f64::from(dbz) / 10.0),
                None => 0.0,
            }
        };
        let top = z_at(c0, r0) * (1.0 - fx) + z_at(c1, r0) * fx;
        let bot = z_at(c0, r1) * (1.0 - fx) + z_at(c1, r1) * fx;
        let z_mean = top * (1.0 - fy) + bot * fy;
        if z_mean <= 0.0 {
            return None;
        }
        let dbz = (10.0 * z_mean.log10()) as f32;
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

/// One band of the dBZ colour scale: `max` is the band's exclusive upper
/// bound (the last band is open-ended and never matched by the `<` scan —
/// the lookup falls through to it). Shared with the legend (CP-2/CP-3), which
/// enumerates this table instead of restating the thresholds.
pub(crate) struct DbzBand {
    pub max: f32,
    pub color: Rgb8,
}

/// The unit label for the dBZ scale, exported alongside the bands so the
/// legend never needs its own hardcoded copy.
pub(crate) const DBZ_UNIT: &str = "dBZ";

/// OPERA dBZ → colour mapping (standard weather radar palette). Ordered by
/// ascending `max`; the last entry's `max` is never reached by the scan in
/// `dbz_to_color` and represents the open-ended 60+ band.
pub(crate) const DBZ_BANDS: &[DbzBand] = &[
    DbzBand {
        max: 5.0,
        color: Rgb8::new(140, 180, 255),
    }, // very light
    DbzBand {
        max: 15.0,
        color: Rgb8::new(80, 140, 255),
    }, // light
    DbzBand {
        max: 25.0,
        color: Rgb8::new(100, 220, 100),
    }, // medium (lighter green)
    DbzBand {
        max: 35.0,
        color: Rgb8::new(40, 180, 40),
    }, // medium (green)
    DbzBand {
        max: 40.0,
        color: Rgb8::new(180, 220, 40),
    }, // medium (yellow-green)
    DbzBand {
        max: 45.0,
        color: Rgb8::new(220, 220, 40),
    }, // medium (yellow)
    DbzBand {
        max: 50.0,
        color: Rgb8::new(240, 160, 40),
    }, // medium-high (orange)
    DbzBand {
        max: 55.0,
        color: Rgb8::new(220, 60, 40),
    }, // high (red)
    DbzBand {
        max: 60.0,
        color: Rgb8::new(200, 40, 180),
    }, // very high (magenta)
    DbzBand {
        max: f32::INFINITY,
        color: Rgb8::new(200, 200, 200),
    }, // extreme, 60+ (white)
];

/// Map a dBZ value to a display colour and Braille intensity (1–14).
fn dbz_to_color(dbz: f32) -> (Rgb8, u8) {
    let color = DBZ_BANDS
        .iter()
        .find(|band| dbz < band.max)
        .map(|band| band.color)
        .unwrap_or_else(|| DBZ_BANDS[DBZ_BANDS.len() - 1].color);
    // Intensity proportional to dBZ (higher = brighter dot)
    let intensity = (dbz.clamp(0.0, 70.0) / 5.0) as u8 + 1;
    (color, intensity.min(14))
}

/// A decoded radar grid the renderer can sample directly at any lat/lon.
/// Cheap to clone: shares the underlying grid storage via `Arc`.
#[derive(Debug, Clone)]
pub struct RadarField {
    grid: Arc<RadarGrid>,
}

impl RadarField {
    /// Bilinearly sample the dBZ field at `(lat, lon)` and map it to a
    /// display colour and Braille intensity (1–14).  `None` where the grid
    /// has no data (outside coverage, or below [`MIN_DBZ`]).
    pub fn sample(&self, lat: f64, lon: f64) -> Option<(Rgb8, u8)> {
        let dbz = self.grid.sample_bilinear(lat, lon)?;
        Some(dbz_to_color(dbz))
    }
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

        // Spot-check a pixel via the bilinear sampler used by the direct-
        // resample field path: should not panic and should return a
        // reasonable dBZ or None (no data at this location).
        if let Some(v) = grid.sample_bilinear(50.0, 10.0) {
            assert!(
                (1.0..=100.0).contains(&v),
                "sample {v} out of reasonable dBZ range"
            );
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

    // ── warm-cache layer (CP-3) ─────────────────────────────────────────

    fn tiny_grid(fill: u8) -> RadarGrid {
        RadarGrid {
            codes: vec![fill; 16],
            width: 4,
            height: 4,
            tie_x: 0.0,
            tie_y: 0.0,
            scale_x: 1000.0,
            scale_y: 1000.0,
        }
    }

    /// A provider backed by a throwaway scratch dir, so tests never touch the
    /// real `~/.cache/front` and never need network access — everything below
    /// warms the RAM `.frd` cache directly rather than fetching.
    fn test_provider(label: &str) -> MeteoGateProvider {
        let dir = std::env::temp_dir().join(format!(
            "front-test-meteogate-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dirs = FrontDirs {
            config_dir: dir.clone(),
            cache_dir: dir.clone(),
            maps_dir: dir.join("maps"),
            radar_dir: dir.join("radar"),
            log_path: dir.join("front.log"),
        };
        MeteoGateProvider::new(
            Client::new(),
            dirs,
            MeteoGateConfig::default(),
            Arc::new(AtomicBool::new(false)),
        )
    }

    #[test]
    fn field_warm_serves_ram_frd_bytes_without_disk_or_network_and_never_redecodes() {
        let provider = test_provider("warm-hit");
        let grid = tiny_grid(42);
        let encoded = encode_frd(&grid).expect("encode");
        // Simulate a byte-cache warm the way `warm_field`/`load_grid_uncached`
        // would after a real fetch: RAM only, no disk file, no download —
        // any file/network access from `field_warm` below would defeat the
        // point of this test.
        provider
            .frd_cache
            .lock()
            .unwrap()
            .insert(1_000, Arc::new(encoded));

        let field = provider.field_warm(1_000).expect("bytes are warm in RAM");
        assert_eq!(field.grid.codes, grid.codes);

        // Second call must hit the decoded-grid LRU, not redecode from
        // bytes: same `Arc`, not merely equal content.
        let field2 = provider.field_warm(1_000).expect("still warm");
        assert!(
            Arc::ptr_eq(&field.grid, &field2.grid),
            "a warm ts must not be redecoded"
        );

        assert!(
            provider.field_warm(999_999).is_none(),
            "a cold ts (no RAM bytes) has no synchronous fast path"
        );
    }

    #[test]
    fn decoded_grid_lru_stays_bounded_while_frd_bytes_cover_the_whole_window() {
        let provider = test_provider("lru-bound");
        for ts in 0..5i64 {
            let grid = tiny_grid(ts as u8 + 1);
            let encoded = encode_frd(&grid).expect("encode");
            provider
                .frd_cache
                .lock()
                .unwrap()
                .insert(ts, Arc::new(encoded));
        }
        for ts in 0..5i64 {
            assert!(provider.field_warm(ts).is_some(), "ts {ts} should decode");
        }
        assert_eq!(
            provider.grid_cache.try_lock().unwrap().len(),
            MAX_CACHED_GRIDS,
            "decoded grids must stay bounded even though every frame's .frd \
             bytes are RAM-resident"
        );
        assert_eq!(
            provider.frd_cache.lock().unwrap().len(),
            5,
            ".frd bytes stay resident window-wide, not bounded like the grid LRU"
        );
    }

    #[test]
    fn prune_field_window_drops_frames_outside_the_keep_set() {
        let provider = test_provider("prune-window");
        for ts in [100i64, 200, 300] {
            provider
                .frd_cache
                .lock()
                .unwrap()
                .insert(ts, Arc::new(Vec::new()));
        }
        let keep: HashSet<i64> = [100i64, 300].into_iter().collect();
        provider.prune_field_window(&keep);
        let remaining: HashSet<i64> = provider.frd_cache.lock().unwrap().keys().copied().collect();
        assert_eq!(remaining, keep);
    }

    #[test]
    fn field_is_warm_reflects_the_ram_byte_cache_only() {
        let provider = test_provider("is-warm");
        assert!(!provider.field_is_warm(5_000));
        provider
            .frd_cache
            .lock()
            .unwrap()
            .insert(5_000, Arc::new(Vec::new()));
        assert!(provider.field_is_warm(5_000));
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

    /// The dBZ band table must be enumerable (for the future legend) and
    /// carry its unit label as data rather than a hardcoded string elsewhere.
    #[test]
    fn dbz_bands_are_enumerable_with_unit_label() {
        assert_eq!(DBZ_UNIT, "dBZ");
        assert_eq!(DBZ_BANDS.len(), 10, "one band per documented threshold");
        assert!(
            DBZ_BANDS.last().unwrap().max.is_infinite(),
            "top band must be open-ended"
        );
    }

    fn grid_row(codes: Vec<u8>) -> RadarGrid {
        let width = codes.len() as u32;
        RadarGrid {
            codes,
            width,
            height: 1,
            tie_x: 0.0,
            tie_y: 0.0,
            scale_x: 1000.0,
            scale_y: 1000.0,
        }
    }

    #[test]
    fn bilinear_interpolates_between_cells_in_linear_z() {
        // Two adjacent cells: 20 dBZ (code 105) and 40 dBZ (code 145).
        let g = grid_row(vec![105, 145]);
        // Exactly on a cell centre reproduces that cell — no smearing.
        assert!((g.sample_bilinear_at(0.0, 0.0).unwrap() - 20.0).abs() < 0.01);
        assert!((g.sample_bilinear_at(1.0, 0.0).unwrap() - 40.0).abs() < 0.01);
        // Halfway is the linear-reflectivity mean (~37 dBZ), which is what
        // keeps strong returns from being under-weighted — not 30, and not
        // either endpoint the way nearest-neighbour would snap.
        let mid = g.sample_bilinear_at(0.5, 0.0).unwrap() as f64;
        let expect = 10.0 * ((10f64.powi(2) + 10f64.powi(4)) / 2.0).log10();
        assert!((mid - expect).abs() < 0.05, "mid {mid} vs {expect}");
    }

    #[test]
    fn bilinear_fades_into_no_data_instead_of_stepping() {
        // Cell 0 is no-data (code 0), cell 1 is 40 dBZ.
        let g = grid_row(vec![0, 145]);
        // On the no-data centre there is still nothing to draw.
        assert_eq!(g.sample_bilinear_at(0.0, 0.0), None);
        // Midway toward the echo the edge fades in rather than stepping to a
        // hard 1 km border — the blocky-edge fix.
        let mid = g.sample_bilinear_at(0.5, 0.0).unwrap();
        assert!(mid > MIN_DBZ && mid < 40.0, "faded edge, got {mid}");
    }

    /// `RadarField::sample` is the direct-resample entry point: a cell centre
    /// must reproduce that cell's band colour exactly, and a point halfway
    /// between two cells must land on the colour for the interpolated dBZ
    /// (linear-Z mean), not snap to either endpoint's band.
    #[test]
    fn radar_field_samples_bilinear_band_colour() {
        // Vary latitude only, at the projection's central meridian (10°E):
        // `d_lon` is then zero, so easting stays exactly zero for both points
        // and only the row axis moves — an exact, decoupled row mapping
        // without relying on the projection's longitude curvature.
        let lon = 10.0;
        let lat0 = 55.0;
        let lat1 = 54.9;
        let (e0, n0) = laea_forward(lat0, lon);
        let (_, n1) = laea_forward(lat1, lon);
        let grid = RadarGrid {
            codes: vec![dbz_to_code(20.0).unwrap(), dbz_to_code(40.0).unwrap()],
            width: 1,
            height: 2,
            tie_x: e0 + LAEA_FALSE_E,
            tie_y: n0 + LAEA_FALSE_N,
            scale_x: 1000.0,
            scale_y: n0 - n1,
        };
        let field = RadarField {
            grid: Arc::new(grid),
        };

        let (color0, _) = field.sample(lat0, lon).expect("cell 0 has data");
        assert_eq!(color0, dbz_to_color(20.0).0);

        let lat_mid = (lat0 + lat1) / 2.0;
        let (color_mid, _) = field.sample(lat_mid, lon).expect("mid has data");
        let expect_dbz = 10.0 * ((10f64.powi(2) + 10f64.powi(4)) / 2.0).log10();
        assert_eq!(
            color_mid,
            dbz_to_color(expect_dbz as f32).0,
            "mid-cell colour must match the interpolated dBZ, not a hard jump"
        );
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

    /// `download_geotiff`'s sleeps have no seam to drive without real network
    /// I/O, so these mirror the loop's exact `delay` recurrence — seed at
    /// `DOWNLOAD_RETRY_BASE`, advance via `POLICY.double` — and assert it
    /// against today's sequence. The loop is the source of truth; if it
    /// changes, these must be updated to match.
    #[test]
    fn download_retry_sequence_matches_today() {
        const POLICY: RetryPolicy = RetryPolicy::new(
            DOWNLOAD_RETRY_BASE,
            MAX_RETRY_AFTER,
            Some(DOWNLOAD_ATTEMPTS),
        );
        // Default path, no header: 400ms then 800ms — the two sleeps a
        // 3-attempt download performs.
        let mut delay = DOWNLOAD_RETRY_BASE;
        assert_eq!(delay, Duration::from_millis(400));
        delay = POLICY.double(delay);
        assert_eq!(delay, Duration::from_millis(800));
    }

    /// A server `Retry-After` redirects the backoff baseline: it is used for
    /// its own sleep AND the doubling continues from it, rather than snapping
    /// back to the pure attempt-indexed sequence. This is the exact case
    /// CP-2's first cut got wrong (it returned 800ms here); the loop mirrors
    /// the original mutating-`delay` semantics.
    #[test]
    fn retry_after_redirects_the_doubling_baseline() {
        const POLICY: RetryPolicy = RetryPolicy::new(
            DOWNLOAD_RETRY_BASE,
            MAX_RETRY_AFTER,
            Some(DOWNLOAD_ATTEMPTS),
        );
        // Attempt 1 gets Retry-After: 30s (replacing the DOWNLOAD_RETRY_BASE
        // seed); attempt 2 fails with no header.
        let mut delay = Duration::from_secs(30).min(MAX_RETRY_AFTER); // header wins, sleep 1
        assert_eq!(delay, Duration::from_secs(30));
        delay = POLICY.double(delay); // baseline carried forward, not reset
        assert_eq!(
            delay,
            Duration::from_secs(60),
            "the sleep after a 30s Retry-After must be 60s, not the base sequence's 800ms"
        );
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
