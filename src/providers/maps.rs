use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use color_eyre::eyre::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::cache::{read_if_exists, write_atomic, write_log, FrontDirs};
use crate::geo::{Bounds, GeoPoint, WorldPoint};
use crate::layers::{BorderLayer, BorderLine, BorderLineKind, BorderResolution, SpatialGrid};

const GEOJSON_BASE_URL: &str = "https://d2ad6b4ur7yvpq.cloudfront.net/naturalearth-3.3.0";
const GEOJSON_RAW_BASE_URL: &str =
    "https://raw.githubusercontent.com/nvkelso/natural-earth-vector/master/geojson";
const ADMIN_1_LINES_FILE: &str = "ne_10m_admin_1_states_provinces_lines.geojson";
const ROADS_FILE: &str = "ne_10m_roads.geojson";

#[derive(Debug, Clone)]
pub struct NaturalEarthProvider {
    client: Client,
    dirs: FrontDirs,
    /// Limits concurrent GeoJSON downloads to prevent OOM.
    download_semaphore: Arc<Semaphore>,
    /// When `true`, long‑running operations (tile generation) should
    /// exit as soon as possible.  Set on quit so `spawn_blocking`
    /// threads don't keep the process alive.
    pub cancel: Arc<AtomicBool>,
}

impl NaturalEarthProvider {
    pub fn new(client: Client, dirs: FrontDirs, cancel: Arc<AtomicBool>) -> Self {
        Self {
            client,
            dirs,
            download_semaphore: Arc::new(Semaphore::new(2)),
            cancel,
        }
    }

    /// Download / load from cache all source GeoJSON for a resolution,
    /// parse and simplify it, then return the combined lines.
    async fn all_source_lines(
        &self,
        resolution: BorderResolution,
        spawn_cancel: Arc<AtomicBool>,
    ) -> Result<Vec<BorderLine>> {
        // Limit concurrent GeoJSON downloads — each resolution
        // downloads 50–120 MB of raw data.
        let _permit = self
            .download_semaphore
            .acquire()
            .await
            .map_err(|e| color_eyre::eyre::eyre!("download semaphore: {e}"))?;
        let start = Instant::now();
        let path = self.dirs.maps_dir.join(format!(
            "natural-earth-{}-countries.geojson",
            resolution.country_scale()
        ));
        let country_bytes = match read_if_exists(&path)? {
            Some(bytes) => bytes,
            None => {
                let bytes = self
                    .download_first(&country_urls(resolution), "Natural Earth countries")
                    .await?;
                write_atomic(&path, &bytes)?;
                bytes
            }
        };
        let region_bytes = if resolution.includes_regions() {
            self.region_detail().await?
        } else {
            None
        };
        let road_bytes = if resolution.includes_regions() {
            self.road_detail().await?
        } else {
            None
        };

        let log_path = self.dirs.log_path.clone();
        let res_label = resolution.label().to_string();
        let cancel = self.cancel.clone();
        // Process each file in its own spawn_blocking so the raw bytes
        // and their serde_json::Value tree are freed before the next
        // file starts.  This keeps peak memory proportional to the
        // largest single file (~250 MB for roads) rather than the sum
        // of all three (~630 MB for High10m).
        // --- Countries ---
        let mut all_lines: Vec<BorderLine> = {
            let cancel = cancel.clone();
            let sc = spawn_cancel.clone();
            tokio::task::spawn_blocking(move || -> Result<Vec<BorderLine>> {
                if cancel.load(Ordering::Relaxed) || sc.load(Ordering::Relaxed) {
                    return Ok(Vec::new());
                }
                parse_country_lines(resolution, &country_bytes, &cancel)
            })
            .await
            .map_err(|e| color_eyre::eyre::eyre!("spawn_blocking: {e}"))??
        };
        // --- Regions (optional) ---
        if let Some(bytes) = region_bytes {
            if cancel.load(Ordering::Relaxed) || spawn_cancel.load(Ordering::Relaxed) {
                return Ok(all_lines);
            }
            let cancel = cancel.clone();
            let sc = spawn_cancel.clone();
            let eps = simplification_epsilon(resolution);
            let region_lines = tokio::task::spawn_blocking(move || -> Result<Vec<BorderLine>> {
                if cancel.load(Ordering::Relaxed) || sc.load(Ordering::Relaxed) {
                    return Ok(Vec::new());
                }
                let mut lines = parse_lines(BorderLineKind::Region, &bytes, &cancel)?;
                if cancel.load(Ordering::Relaxed) || sc.load(Ordering::Relaxed) {
                    return Ok(lines);
                }
                if let Some(eps) = eps {
                    for line in &mut lines {
                        line.points =
                            simplify_points(std::mem::take(&mut line.points), eps, &cancel);
                        line.compute_bbox();
                    }
                }
                Ok(lines)
            })
            .await
            .map_err(|e| color_eyre::eyre::eyre!("spawn_blocking: {e}"))??;
            all_lines.extend(region_lines);
        }
        // --- Roads (optional) ---
        if let Some(bytes) = road_bytes {
            if cancel.load(Ordering::Relaxed) || spawn_cancel.load(Ordering::Relaxed) {
                return Ok(all_lines);
            }
            let cancel = cancel.clone();
            let sc = spawn_cancel.clone();
            let eps = simplification_epsilon(resolution);
            let road_lines = tokio::task::spawn_blocking(move || -> Result<Vec<BorderLine>> {
                if cancel.load(Ordering::Relaxed) || sc.load(Ordering::Relaxed) {
                    return Ok(Vec::new());
                }
                let mut lines = parse_lines(BorderLineKind::Road, &bytes, &cancel)?;
                if cancel.load(Ordering::Relaxed) || sc.load(Ordering::Relaxed) {
                    return Ok(lines);
                }
                if let Some(eps) = eps {
                    for line in &mut lines {
                        line.points =
                            simplify_points(std::mem::take(&mut line.points), eps, &cancel);
                        line.compute_bbox();
                    }
                }
                Ok(lines)
            })
            .await
            .map_err(|e| color_eyre::eyre::eyre!("spawn_blocking: {e}"))??;
            all_lines.extend(road_lines);
        }
        write_log(
            &log_path,
            format!(
                "source: {} parsed ({} lines) in {:?}",
                res_label,
                all_lines.len(),
                start.elapsed()
            ),
        );
        Ok(all_lines)
    }

    /// Path to the `.generated` marker for a resolution.  When present,
    /// all tiles at the resolution's zoom level have been generated and
    /// cached; there is no need to parse source GeoJSON.
    fn generated_marker(&self, resolution: BorderResolution) -> PathBuf {
        let tile_zoom = resolution.tile_zoom();
        self.dirs
            .maps_dir
            .join("tiles")
            .join("v1")
            .join(resolution.label())
            .join(tile_zoom.to_string())
            .join(".generated")
    }

    /// Path to the compact deduplicated cache.  Written after a full
    /// tile load succeeds — subsequent boots hit this instead of
    /// reading thousands of individual tile files.
    fn dedup_cache_path(&self, resolution: BorderResolution) -> PathBuf {
        let tile_zoom = resolution.tile_zoom();
        self.dirs
            .maps_dir
            .join("tiles")
            .join("v1")
            .join(resolution.label())
            .join(tile_zoom.to_string())
            .join(".dedup")
    }

    /// Load border lines for `resolution` from the compact dedup cache.
    /// Returns `None` if the cache doesn't exist yet (caller must
    /// regenerate).
    fn load_all_tiles(&self, resolution: BorderResolution) -> Result<Option<Vec<BorderLine>>> {
        let dedup_path = self.dedup_cache_path(resolution);
        if !dedup_path.exists() {
            return Ok(None);
        }
        let Some(bytes) = read_if_exists(&dedup_path)? else {
            return Ok(None);
        };
        match serde_json::from_slice::<Vec<BorderLine>>(&bytes) {
            Ok(lines) => {
                write_log(
                    &self.dirs.log_path,
                    format!(
                        "{}: loaded {} lines from cache",
                        resolution.label(),
                        lines.len(),
                    ),
                );
                Ok(Some(lines))
            }
            Err(_) => {
                write_log(
                    &self.dirs.log_path,
                    format!(
                        "{}: corrupt dedup cache, need regeneration",
                        resolution.label(),
                    ),
                );
                let _ = std::fs::remove_file(&dedup_path);
                Ok(None)
            }
        }
    }

    /// Generate all border tiles for `resolution` from parsed source
    /// lines, then writes the `.generated` marker.  Returns early
    /// without doing anything if the marker already exists (so
    /// concurrent background tasks don't duplicate work).
    pub(crate) fn generate_all_tiles(
        &self,
        resolution: BorderResolution,
        all_lines: &[BorderLine],
    ) -> Result<()> {
        let marker = self.generated_marker(resolution);
        if marker.exists() {
            return Ok(());
        }

        let dedup_path = self.dedup_cache_path(resolution);
        if let Some(parent) = dedup_path.parent() {
            std::fs::create_dir_all(parent)
                .wrap_err_with(|| format!("create dedup cache dir {}", parent.display()))?;
        }

        // Deduplicate the source lines (should already be unique, but
        // dedup is cheap insurance against future changes).
        let before = all_lines.len();
        let mut seen = std::collections::HashSet::new();
        let unique: Vec<&BorderLine> = all_lines
            .iter()
            .filter(|line| seen.insert(serde_json::to_vec(line).ok()))
            .collect();
        if unique.len() != before {
            write_log(
                &self.dirs.log_path,
                format!(
                    "{}: source dedup {before} → {} lines",
                    resolution.label(),
                    unique.len(),
                ),
            );
        }

        // Write the compact dedup cache — all future loads hit this.
        let json = serde_json::to_vec(&unique)?;
        write_atomic(&dedup_path, &json)?;

        if let Some(parent) = marker.parent() {
            std::fs::create_dir_all(parent)
                .wrap_err_with(|| format!("create marker dir {}", parent.display()))?;
        }
        write_atomic(&marker, b"ok")?;
        Ok(())
    }

    /// Load borders for the given resolution.  On the first call the
    /// source GeoJSON is downloaded and parsed, then the compact dedup
    /// cache is written.  Subsequent calls load from the dedup cache —
    /// a single file read, fast and no re-parsing.
    ///
    /// The returned `BorderLayer` always contains the **full world**
    /// set of border lines (not just those visible in `bounds`), so
    /// panning is seamless.  The `bounds` parameter is retained for
    /// forward compatibility.
    pub async fn borders_for_resolution(
        &self,
        resolution: BorderResolution,
        _bounds: Bounds,
        spawn_cancel: Arc<AtomicBool>,
    ) -> Result<BorderLayer> {
        let start = Instant::now();
        let res_label = resolution.label().to_string();
        let log_path = self.dirs.log_path.clone();

        // Try loading from the dedup cache (single file read).
        {
            let maps = self.clone();
            let lines = tokio::task::spawn_blocking(move || maps.load_all_tiles(resolution))
                .await
                .map_err(|e| color_eyre::eyre::eyre!("spawn_blocking: {e}"))??;

            if let Some(lines) = lines {
                let grid = SpatialGrid::build(&lines);
                write_log(
                    &log_path,
                    format!(
                        "borders: {res_label} loaded from cache ({} lines) in {:?}",
                        lines.len(),
                        start.elapsed()
                    ),
                );
                return Ok(BorderLayer {
                    resolution,
                    lines,
                    grid: Some(grid),
                });
            }
        }

        // Cache miss: parse source data and return the resulting
        // BorderLayer immediately.  Dedup cache generation is deferred
        // to a background task so the user sees borders on screen
        // without waiting for the write.
        write_log(
            &log_path,
            format!("borders: {res_label} — parsing source (tile gen deferred)"),
        );
        let all_lines = self
            .all_source_lines(resolution, spawn_cancel.clone())
            .await?;
        let grid = SpatialGrid::build(&all_lines);
        write_log(
            &log_path,
            format!(
                "borders: {res_label} source parsed ({} lines) in {:?}",
                all_lines.len(),
                start.elapsed()
            ),
        );

        Ok(BorderLayer {
            resolution,
            lines: all_lines,
            grid: Some(grid),
        })
    }

    async fn region_detail(&self) -> Result<Option<Vec<u8>>> {
        self.cached_download(
            ADMIN_1_LINES_FILE,
            &detail_urls(ADMIN_1_LINES_FILE),
            "Natural Earth admin-1 boundaries",
        )
        .await
        .map(Some)
        .or(Ok(None))
    }

    async fn road_detail(&self) -> Result<Option<Vec<u8>>> {
        self.cached_download(ROADS_FILE, &detail_urls(ROADS_FILE), "Natural Earth roads")
            .await
            .map(Some)
            .or(Ok(None))
    }

    async fn cached_download(&self, file: &str, urls: &[String], label: &str) -> Result<Vec<u8>> {
        let path = self.dirs.maps_dir.join(file);
        if let Some(bytes) = read_if_exists(&path)? {
            return Ok(bytes);
        }
        let bytes = self.download_first(urls, label).await?;
        write_atomic(&path, &bytes)?;
        Ok(bytes)
    }

    async fn try_download(&self, url: &str) -> Result<Vec<u8>> {
        Ok(self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?
            .to_vec())
    }

    async fn download_first(&self, urls: &[String], label: &str) -> Result<Vec<u8>> {
        let mut last_error = None;
        for url in urls {
            match self.try_download(url).await {
                Ok(bytes) => return Ok(bytes),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error
            .unwrap_or_else(|| color_eyre::eyre::eyre!("no URLs configured"))
            .wrap_err(format!("download {label}")))
    }
}

fn country_urls(resolution: BorderResolution) -> Vec<String> {
    let scale = resolution.country_scale();
    let file = format!("ne_{scale}_admin_0_countries.geojson");
    detail_urls(&file)
}

fn detail_urls(file: &str) -> Vec<String> {
    vec![
        format!("{GEOJSON_BASE_URL}/{file}"),
        format!("{GEOJSON_RAW_BASE_URL}/{file}"),
    ]
}

/// Parse country boundary lines from GeoJSON and simplify using
/// Ramer-Douglas-Peucker.  Returns raw lines without a SpatialGrid.
fn parse_country_lines(
    resolution: BorderResolution,
    bytes: &[u8],
    cancel: &AtomicBool,
) -> Result<Vec<BorderLine>> {
    let mut lines = parse_lines(BorderLineKind::Country, bytes, cancel)?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(lines);
    }
    if lines.is_empty() {
        color_eyre::eyre::bail!("Natural Earth GeoJSON contained no drawable borders");
    }
    if let Some(eps) = simplification_epsilon(resolution) {
        for line in &mut lines {
            line.points = simplify_points(std::mem::take(&mut line.points), eps, cancel);
            line.compute_bbox();
        }
    }
    Ok(lines)
}

#[cfg(test)]
fn parse_borders(resolution: BorderResolution, bytes: &[u8]) -> Result<BorderLayer> {
    let cancel = AtomicBool::new(false);
    let lines = parse_country_lines(resolution, bytes, &cancel)?;
    let grid = SpatialGrid::build(&lines);
    Ok(BorderLayer {
        resolution,
        lines,
        grid: Some(grid),
    })
}

/// Returns the Ramer-Douglas-Peucker epsilon (in world units) for the
/// given resolution.  Each ε is chosen to keep detail to ≈ 2 subcells
/// at the maximum zoom of the resolution band, so the visual level of
/// detail is naturally proportional to zoom across the whole range.
///
///   Resolution   Band        Max zoom   Subcell width   ε       Precision
///   ────────     ──────────  ────────   ─────────────   ──────  ────────
///   Low110m      zoom 1–4    3.99       0.00035         0.0007  ~2 subcells
///   Medium50m    zoom 4–5.5  5.49       0.00012         0.00025 ~2 subcells
///   High10m      zoom 5.5–7  6.99       0.000044        0.0001  ~2 subcells
///   Regional10m  zoom 7–9    9          0.000011        0.000008 ~0.7 subcells
///
/// Subcell width assumes a typical 180‑cell terminal.
fn simplification_epsilon(resolution: BorderResolution) -> Option<f64> {
    match resolution {
        BorderResolution::Low110m => Some(0.0007),
        BorderResolution::Medium50m => Some(0.00025),
        BorderResolution::High10m => Some(0.0001),
        BorderResolution::Regional10m => Some(0.000008),
    }
}

/// Ramer-Douglas-Peucker line simplification.  Returns a new vector
/// containing only the points needed to stay within `epsilon` of the
/// original polyline.  Endpoints are always preserved.
fn simplify_points(points: Vec<WorldPoint>, epsilon: f64, cancel: &AtomicBool) -> Vec<WorldPoint> {
    if cancel.load(Ordering::Relaxed) {
        return points;
    }
    if points.len() < 3 {
        return points;
    }
    let eps2 = epsilon * epsilon;
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;
    simplify_recursive(&points, 0, points.len() - 1, eps2, &mut keep, cancel);
    points
        .into_iter()
        .zip(keep)
        .filter_map(|(p, k)| k.then_some(p))
        .collect()
}

fn simplify_recursive(
    points: &[WorldPoint],
    first: usize,
    last: usize,
    eps2: f64,
    keep: &mut [bool],
    cancel: &AtomicBool,
) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    if last <= first + 1 {
        return;
    }
    let a = points[first];
    let b = points[last];
    let mut max_d = 0.0_f64;
    let mut max_i = first;
    for (offset, p) in points.iter().enumerate().take(last).skip(first + 1) {
        let d = perpendicular_distance_sq(*p, a, b);
        if d > max_d {
            max_d = d;
            max_i = offset;
        }
    }
    if max_d > eps2 {
        keep[max_i] = true;
        simplify_recursive(points, first, max_i, eps2, keep, cancel);
        simplify_recursive(points, max_i, last, eps2, keep, cancel);
    }
}

fn perpendicular_distance_sq(p: WorldPoint, a: WorldPoint, b: WorldPoint) -> f64 {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let len2 = dx * dx + dy * dy;
    if len2 == 0.0 {
        // Degenerate segment: distance to `a`.
        let ex = p.x - a.x;
        let ey = p.y - a.y;
        return ex * ex + ey * ey;
    }
    // |(b-a) x (p-a)|² / |b-a|²  (cross product magnitude squared)
    let cross = dx * (p.y - a.y) - dy * (p.x - a.x);
    (cross * cross) / len2
}

fn parse_lines(kind: BorderLineKind, bytes: &[u8], cancel: &AtomicBool) -> Result<Vec<BorderLine>> {
    let root: Value = serde_json::from_slice(bytes).wrap_err("parse Natural Earth GeoJSON")?;
    let features = root
        .get("features")
        .and_then(Value::as_array)
        .ok_or_else(|| color_eyre::eyre::eyre!("Natural Earth GeoJSON missing features"))?;

    let mut lines = Vec::new();
    for (i, feature) in features.iter().enumerate() {
        if i % 20 == 0 && cancel.load(Ordering::Relaxed) {
            return Ok(lines);
        }
        let Some(geometry) = feature.get("geometry") else {
            continue;
        };
        let Some(geometry_kind) = geometry.get("type").and_then(Value::as_str) else {
            continue;
        };
        let Some(coords) = geometry.get("coordinates") else {
            continue;
        };
        match geometry_kind {
            "Polygon" => extract_polygon(kind, coords, &mut lines, cancel),
            "MultiPolygon" => {
                if let Some(polygons) = coords.as_array() {
                    for polygon in polygons {
                        extract_polygon(kind, polygon, &mut lines, cancel);
                    }
                }
            }
            "LineString" => extract_line_string(kind, coords, &mut lines, cancel),
            "MultiLineString" => {
                if let Some(line_strings) = coords.as_array() {
                    for line_string in line_strings {
                        extract_line_string(kind, line_string, &mut lines, cancel);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(lines)
}

fn extract_polygon(
    kind: BorderLineKind,
    value: &Value,
    lines: &mut Vec<BorderLine>,
    cancel: &AtomicBool,
) {
    let Some(rings) = value.as_array() else {
        return;
    };
    for (i, ring) in rings.iter().enumerate() {
        if i % 5 == 0 && cancel.load(Ordering::Relaxed) {
            return;
        }
        let Some(points) = ring.as_array() else {
            continue;
        };
        let mut line = Vec::with_capacity(points.len());
        for point in points {
            let Some(pair) = point.as_array() else {
                continue;
            };
            let (Some(lon), Some(lat)) = (
                pair.first().and_then(Value::as_f64),
                pair.get(1).and_then(Value::as_f64),
            ) else {
                continue;
            };
            line.push(GeoPoint::new(lon, lat).to_world());
        }
        if line.len() > 1 {
            let mut bl = BorderLine {
                kind,
                points: line,
                bbox: Bounds {
                    min_x: 0.0,
                    max_x: 0.0,
                    min_y: 0.0,
                    max_y: 0.0,
                },
            };
            bl.compute_bbox();
            lines.push(bl);
        }
    }
}

fn extract_line_string(
    kind: BorderLineKind,
    value: &Value,
    lines: &mut Vec<BorderLine>,
    cancel: &AtomicBool,
) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    let Some(points) = value.as_array() else {
        return;
    };
    let mut line = Vec::with_capacity(points.len());
    for point in points {
        let Some(pair) = point.as_array() else {
            continue;
        };
        let (Some(lon), Some(lat)) = (
            pair.first().and_then(Value::as_f64),
            pair.get(1).and_then(Value::as_f64),
        ) else {
            continue;
        };
        line.push(GeoPoint::new(lon, lat).to_world());
    }
    if line.len() > 1 {
        let mut bl = BorderLine {
            kind,
            points: line,
            bbox: Bounds {
                min_x: 0.0,
                max_x: 0.0,
                min_y: 0.0,
                max_y: 0.0,
            },
        };
        bl.compute_bbox();
        lines.push(bl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::WorldPoint;

    #[test]
    fn simplify_keeps_essential_points() {
        // Triangle: corners at (0,0), (0.1, 0), (0.1, 0.1)
        // ε = 0.001 — should keep all 3 points (corners are far apart)
        let points = vec![
            WorldPoint { x: 0.0, y: 0.0 },
            WorldPoint { x: 0.1, y: 0.0 },
            WorldPoint { x: 0.1, y: 0.1 },
        ];
        let cancel = AtomicBool::new(false);
        let simplified = simplify_points(points.clone(), 0.001, &cancel);
        assert_eq!(simplified.len(), 3, "corners should be kept");

        // With a very large ε, only endpoints survive.
        let cancel = AtomicBool::new(false);
        let simplified2 = simplify_points(points, 10.0, &cancel);
        assert_eq!(simplified2.len(), 2, "endpoints only");
    }

    #[test]
    fn parses_polygon_geojson() {
        // Polygon large enough to survive RDP simplification at ε=0.005
        // (corners span 0.05 world units, well above the threshold).
        let json = br#"{
          "type":"FeatureCollection",
          "features":[{"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[0,0],[18,0],[18,18],[0,0]]]}}]
        }"#;
        let layer = parse_borders(BorderResolution::Low110m, json).unwrap();
        assert_eq!(layer.resolution, BorderResolution::Low110m);
        assert_eq!(layer.lines.len(), 1);
        assert_eq!(layer.lines[0].kind, BorderLineKind::Country);
        // 18° corners at low-zoom RDP keep the corner vertices
        assert!(layer.lines[0].points.len() >= 3);
        // Bbox should be computed automatically
        assert!(layer.lines[0].bbox.max_x > layer.lines[0].bbox.min_x);
    }

    #[test]
    fn parses_line_string_geojson() {
        let json = br#"{
          "type":"FeatureCollection",
          "features":[{"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]}}]
        }"#;
        let lines = parse_lines(BorderLineKind::Region, json, &AtomicBool::new(false)).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].kind, BorderLineKind::Region);
        assert_eq!(lines[0].points.len(), 2);
    }

    #[test]
    fn parses_multi_line_string_geojson() {
        let json = br#"{
          "type":"FeatureCollection",
          "features":[{"type":"Feature","geometry":{"type":"MultiLineString","coordinates":[[[0,0],[1,0]],[[2,0],[3,0]]]}}]
        }"#;
        let lines = parse_lines(BorderLineKind::Road, json, &AtomicBool::new(false)).unwrap();
        assert_eq!(lines.len(), 2, "two line strings → two BorderLines");
        assert!(lines.iter().all(|l| l.kind == BorderLineKind::Road));
    }

    #[test]
    fn parses_multi_polygon_geojson() {
        let json = br#"{
          "type":"FeatureCollection",
          "features":[{"type":"Feature","geometry":{"type":"MultiPolygon","coordinates":[[[[0,0],[9,0],[9,9],[0,0]]],[[[10,0],[19,0],[19,9],[10,0]]]]}}]
        }"#;
        let layer = parse_borders(BorderResolution::Low110m, json).unwrap();
        assert_eq!(layer.lines.len(), 2, "two polygons → two BorderLines");
    }

    #[test]
    fn simplification_epsilon_ordering() {
        // Finer resolution → smaller epsilon (tighter fit).
        let eps_low = simplification_epsilon(BorderResolution::Low110m).unwrap();
        let eps_reg = simplification_epsilon(BorderResolution::Regional10m).unwrap();
        assert!(
            eps_reg < eps_low,
            "regional must have smaller epsilon than low"
        );
    }

    #[test]
    fn simplify_collinear_removes_middle_point() {
        // Three collinear points: middle is redundant at any ε > 0.
        let pts = vec![
            WorldPoint { x: 0.0, y: 0.0 },
            WorldPoint { x: 0.5, y: 0.0 },
            WorldPoint { x: 1.0, y: 0.0 },
        ];
        let cancel = AtomicBool::new(false);
        let result = simplify_points(pts, 1e-9, &cancel);
        assert_eq!(result.len(), 2, "collinear middle must be removed");
    }

    #[test]
    fn simplify_two_points_unchanged() {
        let pts = vec![WorldPoint { x: 0.0, y: 0.0 }, WorldPoint { x: 1.0, y: 1.0 }];
        let cancel = AtomicBool::new(false);
        let result = simplify_points(pts.clone(), 0.001, &cancel);
        assert_eq!(result.len(), 2, "two-point line cannot be reduced");
    }

    #[test]
    fn simplify_cancelled_returns_input_unchanged() {
        let pts = vec![
            WorldPoint { x: 0.0, y: 0.0 },
            WorldPoint { x: 0.5, y: 0.5 },
            WorldPoint { x: 1.0, y: 0.0 },
        ];
        let cancel = AtomicBool::new(true);
        let result = simplify_points(pts.clone(), 1e-9, &cancel);
        assert_eq!(
            result.len(),
            pts.len(),
            "cancelled simplify returns full input"
        );
    }

    #[test]
    fn perpendicular_distance_sq_on_axis_point() {
        // Point at (0, 1) against segment from (0,0) to (1,0): perp dist = 1.0.
        let p = WorldPoint { x: 0.0, y: 1.0 };
        let a = WorldPoint { x: 0.0, y: 0.0 };
        let b = WorldPoint { x: 1.0, y: 0.0 };
        let d2 = perpendicular_distance_sq(p, a, b);
        assert!((d2 - 1.0).abs() < 1e-9, "expected d²=1.0, got {d2}");
    }

    #[test]
    fn perpendicular_distance_sq_degenerate_segment() {
        // Degenerate segment (a==b): returns distance² to a.
        let p = WorldPoint { x: 3.0, y: 4.0 };
        let a = WorldPoint { x: 0.0, y: 0.0 };
        let b = WorldPoint { x: 0.0, y: 0.0 };
        let d2 = perpendicular_distance_sq(p, a, b);
        assert!((d2 - 25.0).abs() < 1e-9, "3-4-5 triangle: d²=25, got {d2}");
    }
}
