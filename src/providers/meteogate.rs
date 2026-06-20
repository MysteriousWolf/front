use std::collections::HashMap;
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

/// Shared in-memory cache for the decoded radar grid.
type GridCache = Arc<tokio::sync::Mutex<Option<(i64, Arc<RadarGrid>)>>>;

/// How long a negative HEAD result ("object not on S3 yet") is cached
/// before the slot is probed again.
const MISSING_RETRY: Duration = Duration::from_secs(60);

/// Outcome of probing S3 for a given timestamp, cached so repeated
/// pans/zooms don't re-issue HEAD requests for slots we already know
/// about.  Objects are immutable once published, so positive results
/// never expire; negative results are retried after [`MISSING_RETRY`].
#[derive(Debug, Clone, Copy)]
enum ProbeResult {
    Exists,
    Missing(Instant),
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
    /// the ~63 MB float grid from the raw GeoTIFF on disk.
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
            grid_cache: Arc::new(tokio::sync::Mutex::new(None)),
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

    fn cache_path(&self, timestamp: i64) -> PathBuf {
        self.dirs
            .radar_dir
            .join(format!("meteogate/radar/{}.tiff", timestamp))
    }

    /// Discover available frame timestamps (12 frames × 5 min = 1 hour).
    pub async fn frame_list(&self) -> Result<Vec<i64>> {
        Ok(compute_frame_list())
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
        let mut cache = self.grid_cache.lock().await;
        if let Some((ts, cached)) = cache.as_ref() {
            if *ts == timestamp {
                write_log(log, format!("meteogate: grid cache hit ts={ts}"));
                return Ok(Arc::clone(cached));
            }
        }
        write_log(log, format!("meteogate: fetching geotiff ts={timestamp}"));
        let bytes = self.fetch_geotiff(timestamp).await?;
        write_log(
            log,
            format!("meteogate: got {} bytes, parsing", bytes.len()),
        );
        let g = Arc::new(parse_geotiff(&bytes)?);
        write_log(log, "meteogate: grid parsed OK");
        *cache = Some((timestamp, Arc::clone(&g)));
        Ok(g)
    }

    /// Like [`frame`] but builds tiles in **centre-first clockwise spiral
    /// order** and sends each completed tile through `tile_tx` as soon as
    /// it is ready.  Up to [`MAX_CONCURRENT_TILES`] tiles are built
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
    const MAX_CONCURRENT_TILES: usize = 8;

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

    async fn fetch_geotiff(&self, timestamp: i64) -> Result<Vec<u8>> {
        let log = &self.dirs.log_path;
        let path = self.cache_path(timestamp);
        if let Some(bytes) = read_if_exists(&path)? {
            write_log(log, format!("meteogate: geotiff cache hit ts={timestamp}"));
            return Ok(bytes);
        }
        let url = self.s3_url(timestamp);
        write_log(log, format!("meteogate: downloading {url}"));
        let bytes = self
            .client
            .get(&url)
            .send()
            .await
            .wrap_err_with(|| format!("download MeteoGate GeoTIFF: {url}"))?
            .error_for_status()
            .wrap_err_with(|| format!("MeteoGate GeoTIFF response: {url}"))?
            .bytes()
            .await
            .wrap_err("read MeteoGate GeoTIFF")?
            .to_vec();
        write_log(log, format!("meteogate: downloaded {} bytes", bytes.len()));
        write_atomic(&path, &bytes)?;
        Ok(bytes)
    }

    /// Send a HEAD request with a short timeout; return `true` if the
    /// object exists.  A failure (timeout / network error) is treated
    /// as "not found" so the outer retry loop moves on quickly.
    async fn head_geotiff(&self, timestamp: i64) -> Result<bool> {
        let url = self.s3_url(timestamp);
        write_log(&self.dirs.log_path, format!("meteogate: HEAD {url}"));
        match tokio::time::timeout(Duration::from_secs(3), self.client.head(&url).send()).await {
            Ok(Ok(resp)) => {
                let ok = resp.status().is_success();
                write_log(
                    &self.dirs.log_path,
                    format!("meteogate: HEAD ts={timestamp} -> {}", resp.status()),
                );
                Ok(ok)
            }
            Ok(Err(e)) => {
                write_log(
                    &self.dirs.log_path,
                    format!("meteogate: HEAD ts={timestamp} error: {e}"),
                );
                Ok(false) // treat as not found
            }
            Err(_) => {
                write_log(
                    &self.dirs.log_path,
                    format!("meteogate: HEAD ts={timestamp} timeout"),
                );
                Ok(false) // treat as not found
            }
        }
    }

    /// Starting from `timestamp` (a 5‑min boundary), try HEAD requests
    /// backwards in 5‑min steps and return the newest that exists (up to
    /// 30 min / 7 attempts).
    /// Check whether the object for `ts` exists, consulting the probe
    /// cache first.  A locally cached GeoTIFF counts as existing without
    /// any network traffic.
    async fn probe_geotiff(&self, ts: i64) -> Result<bool> {
        {
            let cache = self.probe_cache.lock().await;
            match cache.get(&ts) {
                Some(ProbeResult::Exists) => return Ok(true),
                Some(ProbeResult::Missing(at)) if at.elapsed() < MISSING_RETRY => return Ok(false),
                _ => {}
            }
        }
        let exists = self.cache_path(ts).exists() || self.head_geotiff(ts).await?;
        let mut cache = self.probe_cache.lock().await;
        cache.insert(
            ts,
            if exists {
                ProbeResult::Exists
            } else {
                ProbeResult::Missing(Instant::now())
            },
        );
        // Keep the cache from growing without bound across long sessions.
        if cache.len() > 512 {
            cache.clear();
        }
        Ok(exists)
    }

    async fn resolve_nearest_available(&self, timestamp: i64) -> Result<i64> {
        let log = &self.dirs.log_path;
        for offset in (0..=30).step_by(5) {
            let ts = timestamp - offset * 60;
            if self.probe_geotiff(ts).await? {
                write_log(
                    log,
                    format!("meteogate: resolved ts={timestamp} -> nearest={ts}"),
                );
                return Ok(ts);
            }
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

/// Compute the list of expected radar frame timestamps (12 frames ×
/// 5 min = 1 hour), newest first.  Purely local — no network traffic —
/// so the UI can poll it cheaply to detect when a new slot opens.
pub fn compute_frame_list() -> Vec<i64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let latest = now - (now % 300);
    (0..12).map(|i| latest - i * 300).collect()
}

#[derive(Debug)]
struct RadarGrid {
    /// Row-major dBZ samples, `width * height` long.
    data: Vec<f32>,
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
        let v = self.data[row * self.width as usize + col];
        if v >= MIN_DBZ {
            Some(v)
        } else {
            None
        }
    }
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

    let data: Vec<f32> = if stride == 1 {
        let mut pixels = pixels;
        pixels.truncate(npixels);
        pixels
    } else {
        (0..npixels).map(|i| pixels[i * stride]).collect()
    };

    Ok(RadarGrid {
        data,
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
    let mut rows = Vec::with_capacity(TILE_PX as usize);
    for py in 0..TILE_PX {
        rows.push(build_row(grid, tc, z, py));
    }
    Ok(RadarTile {
        coord: tc,
        size: TILE_PX,
        rows,
    })
}

fn build_row(grid: &RadarGrid, tc: TileCoord, z: u8, py: u32) -> Vec<RadarRun> {
    let mut runs: Vec<RadarRun> = Vec::new();
    let mut current: Option<(Rgb8, u8)> = None;
    let mut start_x: u16 = 0;

    for px in 0..TILE_PX {
        let (lat, lon) = tile_pixel_to_latlon(tc, z, px, py);
        let value: Option<(Rgb8, u8)> = match grid.lat_lon_to_uv(lat, lon) {
            Some((col, row)) => grid.sample(col, row).map(dbz_to_color),
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

    /// Verify that `lat_lon_to_uv` maps the LAEA origin (55°N, 10°E) to the
    /// centre of the grid (~pixel 1950, 2100) when accounting for false
    /// easting/northing.
    #[test]
    fn lat_lon_to_uv_maps_origin_to_center() {
        // Build a minimal grid with parameters matching the real GeoTIFF.
        let grid = RadarGrid {
            data: vec![0.0f32; 3800 * 4400],
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
            data: vec![0.0f32; 3800 * 4400],
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
}
