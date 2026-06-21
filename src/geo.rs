use std::f64::consts::PI;

use serde::{Deserialize, Serialize};

pub const MAX_LAT: f64 = 85.051_128_78;
pub const EUROPE_LAT: f64 = 46.05;
pub const EUROPE_LON: f64 = 14.51;
pub const EUROPE_ZOOM: f64 = 4.0;
pub const MIN_VIEW_ZOOM: f64 = 1.0;
pub const MAX_VIEW_ZOOM: f64 = 12.0;

/// Approximate km radius used to classify a station as "near" a city.
pub const CITY_MATCH_KM: f64 = 100.0;

/// City names matching `EUROPEAN_CAPITALS` (same order).
pub const EUROPEAN_CAPITAL_NAMES: &[&str] = &[
    "Vienna",
    "Brussels",
    "Sofia",
    "Zagreb",
    "Nicosia",
    "Prague",
    "Copenhagen",
    "Tallinn",
    "Helsinki",
    "Paris",
    "Berlin",
    "Athens",
    "Budapest",
    "Reykjavik",
    "Dublin",
    "Rome",
    "Riga",
    "Vilnius",
    "Luxembourg",
    "Valletta",
    "Amsterdam",
    "Oslo",
    "Warsaw",
    "Lisbon",
    "Bucharest",
    "Bratislava",
    "Ljubljana",
    "Madrid",
    "Stockholm",
    "Bern",
    "Ankara",
    "London",
    "Kyiv",
    "Belgrade",
    "Tirana",
    "Sarajevo",
    "Podgorica",
    "Skopje",
    "Chisinau",
    "Minsk",
    "Tbilisi",
    "Baku",
    "Yerevan",
    "Moscow",
    "Saint Petersburg",
];

/// European capitals (lat, lon).
pub const EUROPEAN_CAPITALS: &[(f64, f64)] = &[
    (48.21, 16.37),  // Vienna, AT
    (50.85, 4.35),   // Brussels, BE
    (42.70, 23.32),  // Sofia, BG
    (45.81, 15.98),  // Zagreb, HR
    (35.17, 33.37),  // Nicosia, CY
    (50.08, 14.44),  // Prague, CZ
    (55.68, 12.57),  // Copenhagen, DK
    (59.44, 24.75),  // Tallinn, EE
    (60.17, 24.94),  // Helsinki, FI
    (48.86, 2.35),   // Paris, FR
    (52.52, 13.40),  // Berlin, DE
    (37.98, 23.73),  // Athens, GR
    (47.50, 19.04),  // Budapest, HU
    (64.14, -21.90), // Reykjavik, IS
    (53.33, -6.25),  // Dublin, IE
    (41.90, 12.50),  // Rome, IT
    (56.95, 24.11),  // Riga, LV
    (54.69, 25.28),  // Vilnius, LT
    (49.61, 6.13),   // Luxembourg, LU
    (35.90, 14.51),  // Valletta, MT
    (52.37, 4.90),   // Amsterdam, NL
    (59.91, 10.75),  // Oslo, NO
    (52.23, 21.01),  // Warsaw, PL
    (38.72, -9.14),  // Lisbon, PT
    (44.43, 26.10),  // Bucharest, RO
    (48.15, 17.11),  // Bratislava, SK
    (46.05, 14.51),  // Ljubljana, SI
    (40.42, -3.70),  // Madrid, ES
    (59.33, 18.07),  // Stockholm, SE
    (46.95, 7.45),   // Bern, CH
    (39.93, 32.85),  // Ankara, TR
    (51.51, -0.13),  // London, GB
    (50.45, 30.52),  // Kyiv, UA
    (44.82, 20.46),  // Belgrade, RS
    (41.33, 19.82),  // Tirana, AL
    (43.85, 18.42),  // Sarajevo, BA
    (42.44, 19.27),  // Podgorica, ME
    (42.00, 21.43),  // Skopje, MK
    (47.00, 28.86),  // Chisinau, MD
    (53.90, 27.57),  // Minsk, BY
    (41.69, 44.83),  // Tbilisi, GE
    (40.41, 49.87),  // Baku, AZ
    (40.18, 44.51),  // Yerevan, AM
    (55.75, 37.62),  // Moscow, RU
    (59.95, 30.32),  // Saint Petersburg, RU
];

/// Additional major European cities (lat, lon).  Shown at medium zoom
/// alongside capitals.
pub const EUROPEAN_MAJOR_CITIES: &[(f64, f64)] = &[
    (48.14, 11.58), // Munich, DE
    (53.57, 10.02), // Hamburg, DE
    (51.22, 6.78),  // Dusseldorf, DE
    (50.94, 6.96),  // Cologne, DE
    (50.11, 8.68),  // Frankfurt, DE
    (48.78, 9.18),  // Stuttgart, DE
    (51.34, 12.38), // Leipzig, DE
    (51.03, 13.73), // Dresden, DE
    (49.45, 11.08), // Nuremberg, DE
    (53.08, 8.81),  // Bremen, DE
    (52.37, 9.73),  // Hannover, DE
    (51.51, 7.46),  // Dortmund, DE
    (41.39, 2.16),  // Barcelona, ES
    (39.47, -0.38), // Valencia, ES
    (37.39, -5.99), // Seville, ES
    (41.66, -0.88), // Zaragoza, ES
    (45.47, 9.19),  // Milan, IT
    (40.84, 14.25), // Naples, IT
    (45.07, 7.69),  // Turin, IT
    (43.85, 7.31),  // Nice, FR
    (45.75, 4.84),  // Lyon, FR
    (43.30, 5.37),  // Marseille, FR
    (47.22, -1.55), // Nantes, FR
    (44.84, -0.58), // Bordeaux, FR
    (50.63, 3.06),  // Lille, FR
    (50.06, 19.94), // Krakow, PL
    (51.11, 17.04), // Wroclaw, PL
    (52.41, 16.93), // Poznan, PL
    (51.77, 19.46), // Lodz, PL
    (54.35, 18.65), // Gdansk, PL
    (49.99, 36.23), // Kharkiv, UA
    (46.49, 30.73), // Odessa, UA
    (48.46, 35.04), // Dnipro, UA
    (41.01, 28.97), // Istanbul, TR
    (38.42, 27.14), // Izmir, TR
    (40.77, 29.92), // Bursa, TR
    (36.89, 30.69), // Antalya, TR
    (37.00, 35.32), // Adana, TR
    (51.92, 4.48),  // Rotterdam, NL
    (51.22, 4.40),  // Antwerp, BE
    (55.86, -4.25), // Glasgow, GB
    (53.48, -2.24), // Manchester, GB
    (52.49, -1.89), // Birmingham, GB
    (53.80, -1.55), // Leeds, GB
    (55.95, -3.19), // Edinburgh, GB
    (51.46, -2.60), // Bristol, GB
    (47.38, 8.54),  // Zurich, CH
    (46.21, 6.14),  // Geneva, CH
    (47.07, 15.43), // Graz, AT
    (47.80, 13.04), // Salzburg, AT
    (57.71, 11.97), // Gothenburg, SE
    (55.61, 13.00), // Malmo, SE
    (61.50, 23.77), // Tampere, FI
    (65.01, 25.47), // Oulu, FI
    (49.20, 16.61), // Brno, CZ
    (47.16, 27.59), // Iasi, RO
    (46.77, 23.59), // Cluj-Napoca, RO
    (45.75, 21.23), // Timisoara, RO
    (44.31, 23.80), // Craiova, RO
    (43.21, 27.92), // Varna, BG
    (42.14, 24.75), // Plovdiv, BG
    (40.66, 22.93), // Thessaloniki, GR
    (35.34, 25.13), // Heraklion, GR
    (45.27, 19.83), // Novi Sad, RS
    (43.32, 21.89), // Nis, RS
    (47.50, 21.62), // Debrecen, HU
    (54.89, 23.90), // Kaunas, LT
    (58.38, 26.73), // Tartu, EE
];

/// Returns `true` when `(lat, lon)` is within `CITY_MATCH_KM` of any
/// European capital.  Used by both the display filter and density clipping
/// so that capital-area stations are never dropped.
pub fn near_european_capital(lat: f64, lon: f64) -> bool {
    let cos_lat = lat.to_radians().cos();
    let threshold_sq = (CITY_MATCH_KM / 111.0).powi(2);
    EUROPEAN_CAPITALS.iter().any(|&(clat, clon)| {
        let dlat = lat - clat;
        let dlon = (lon - clon) * cos_lat;
        dlat * dlat + dlon * dlon < threshold_sq
    })
}
const TILE_SIZE: f64 = 256.0;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GeoPoint {
    pub lon: f64,
    pub lat: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WorldPoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    pub center: WorldPoint,
    pub zoom: f64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Bounds {
    pub min_x: f64,
    pub max_x: f64,
    pub min_y: f64,
    pub max_y: f64,
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub struct TileCoord {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

impl GeoPoint {
    pub fn new(lon: f64, lat: f64) -> Self {
        Self {
            lon: lon.clamp(-180.0, 180.0),
            lat: lat.clamp(-MAX_LAT, MAX_LAT),
        }
    }

    pub fn to_world(self) -> WorldPoint {
        lat_lon_to_world(self.lat, self.lon)
    }
}

impl Viewport {
    pub fn europe() -> Self {
        Self::from_lat_lon(EUROPE_LAT, EUROPE_LON, EUROPE_ZOOM)
    }

    pub fn from_lat_lon(lat: f64, lon: f64, zoom: f64) -> Self {
        Self {
            center: lat_lon_to_world(lat, lon),
            zoom: zoom.clamp(MIN_VIEW_ZOOM, MAX_VIEW_ZOOM),
        }
    }

    pub fn bounds(self, width: u16, height: u16) -> Bounds {
        let width = width.max(1) as f64;
        let height = height.max(1) as f64;
        let span_x = 1.8 / self.zoom.exp2();
        let span_y = span_x * (height / width) * 2.0;
        Bounds {
            min_x: (self.center.x - span_x).clamp(0.0, 1.0),
            max_x: (self.center.x + span_x).clamp(0.0, 1.0),
            min_y: (self.center.y - span_y).clamp(0.0, 1.0),
            max_y: (self.center.y + span_y).clamp(0.0, 1.0),
        }
    }

    /// Constrain centre so the viewport stays within the Mercator
    /// projection.  Returns `((min_x, max_x), (min_y, max_y))`.
    fn centre_range(zoom: f64, width: u16, height: u16) -> ((f64, f64), (f64, f64)) {
        let w = f64::from(width.saturating_sub(1).max(1));
        let h = f64::from(height.saturating_sub(1).max(1));
        let span_x = 1.8 / zoom.exp2();
        let span_y = span_x * (h / w) * 2.0;

        let (min_cx, max_cx) = if span_x < 0.5 {
            (span_x, 1.0 - span_x)
        } else {
            (0.0, 1.0)
        };
        let (min_cy, max_cy) = if span_y < 0.5 {
            (span_y, 1.0 - span_y)
        } else {
            (0.0, 1.0)
        };
        ((min_cx, max_cx), (min_cy, max_cy))
    }

    pub fn pan(&mut self, dx: f64, dy: f64) {
        let scale = 0.25 / self.zoom.exp2();
        let ((min_cx, max_cx), (min_cy, max_cy)) = Self::centre_range(self.zoom, 100, 50);
        // Pre-clamp so an out-of-range center (e.g. from saved state
        // at a different zoom) doesn't jump in the wrong direction on
        // the first pan.
        self.center.x = self.center.x.clamp(min_cx, max_cx) + dx * scale;
        self.center.y = self.center.y.clamp(min_cy, max_cy) + dy * scale;
        self.center.x = self.center.x.clamp(min_cx, max_cx);
        self.center.y = self.center.y.clamp(min_cy, max_cy);
    }

    pub fn pan_screen_delta(&mut self, width: u16, height: u16, delta_x: f64, delta_y: f64) {
        let w = f64::from(width.saturating_sub(1).max(1));
        let h = f64::from(height.saturating_sub(1).max(1));
        let bounds = self.bounds(width, height);
        let ((min_cx, max_cx), (min_cy, max_cy)) = Self::centre_range(self.zoom, width, height);
        self.center.x = self.center.x.clamp(min_cx, max_cx) + delta_x / w * bounds.width();
        self.center.y = self.center.y.clamp(min_cy, max_cy) + delta_y / h * bounds.height();
        self.center.x = self.center.x.clamp(min_cx, max_cx);
        self.center.y = self.center.y.clamp(min_cy, max_cy);
    }

    pub fn zoom_by(&mut self, delta: f64) {
        self.zoom = (self.zoom + delta).clamp(MIN_VIEW_ZOOM, MAX_VIEW_ZOOM);
    }

    pub fn zoom_around_screen(
        &mut self,
        width: u16,
        height: u16,
        column: u16,
        row: u16,
        delta: f64,
    ) {
        let before = self.world_at_screen(width, height, column, row);
        self.zoom_by(delta);
        let after = self.world_at_screen(width, height, column, row);
        let ((min_cx, max_cx), (min_cy, max_cy)) = Self::centre_range(self.zoom, width, height);
        self.center.x = self.center.x.clamp(min_cx, max_cx) + before.x - after.x;
        self.center.y = self.center.y.clamp(min_cy, max_cy) + before.y - after.y;
        self.center.x = self.center.x.clamp(min_cx, max_cx);
        self.center.y = self.center.y.clamp(min_cy, max_cy);
    }

    pub fn world_at_screen(&self, width: u16, height: u16, column: u16, row: u16) -> WorldPoint {
        let bounds = self.bounds(width, height);
        let width = f64::from(width.saturating_sub(1).max(1));
        let height = f64::from(height.saturating_sub(1).max(1));
        let x = f64::from(column.min(width as u16)) / width;
        let y = f64::from(row.min(height as u16)) / height;
        WorldPoint {
            x: bounds.min_x + x * bounds.width(),
            y: bounds.min_y + y * bounds.height(),
        }
    }
}

impl Bounds {
    pub fn width(self) -> f64 {
        self.max_x - self.min_x
    }

    pub fn height(self) -> f64 {
        self.max_y - self.min_y
    }

    pub fn intersects_tile(self, tile: TileCoord) -> bool {
        let b = tile_bounds(tile);
        self.min_x <= b.max_x
            && self.max_x >= b.min_x
            && self.min_y <= b.max_y
            && self.max_y >= b.min_y
    }

    /// Returns `true` if this `Bounds` overlaps `other`.
    pub fn intersects(self, other: Bounds) -> bool {
        self.min_x <= other.max_x
            && self.max_x >= other.min_x
            && self.min_y <= other.max_y
            && self.max_y >= other.min_y
    }

    /// Returns `true` if `other` lies entirely within this `Bounds`.
    pub fn contains(self, other: Bounds) -> bool {
        self.min_x <= other.min_x
            && self.max_x >= other.max_x
            && self.min_y <= other.min_y
            && self.max_y >= other.max_y
    }

    /// Inflate this `Bounds` by the given fraction of its own width and
    /// height, centered on the current centre.  Useful for prefetching
    /// data slightly outside the viewport so small pans don't need a
    /// network round-trip.  Result is clamped to `[0, 1]²`.
    pub fn expanded(self, fraction: f64) -> Bounds {
        let dx = self.width() * fraction * 0.5;
        let dy = self.height() * fraction * 0.5;
        Bounds {
            min_x: (self.min_x - dx).max(0.0),
            max_x: (self.max_x + dx).min(1.0),
            min_y: (self.min_y - dy).max(0.0),
            max_y: (self.max_y + dy).min(1.0),
        }
    }
}

pub fn lat_lon_to_world(lat: f64, lon: f64) -> WorldPoint {
    let lat = lat.clamp(-MAX_LAT, MAX_LAT);
    let x = (lon + 180.0) / 360.0;
    let lat_rad = lat.to_radians();
    let y = (1.0 - ((lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / PI)) / 2.0;
    WorldPoint {
        x: x.clamp(0.0, 1.0),
        y: y.clamp(0.0, 1.0),
    }
}

pub fn world_to_lat_lon(point: WorldPoint) -> GeoPoint {
    let lon = point.x * 360.0 - 180.0;
    let n = PI - 2.0 * PI * point.y;
    let lat = n.sinh().atan().to_degrees();
    GeoPoint::new(lon, lat)
}

pub fn tile_for_world(point: WorldPoint, z: u8) -> TileCoord {
    let n = 2_u32.pow(z as u32) as f64;
    TileCoord {
        z,
        x: (point.x * n).floor().clamp(0.0, n - 1.0) as u32,
        y: (point.y * n).floor().clamp(0.0, n - 1.0) as u32,
    }
}

pub fn visible_tiles(bounds: Bounds, z: u8) -> Vec<TileCoord> {
    let n = 2_u32.pow(z as u32);
    let min_x = (bounds.min_x * n as f64).floor().clamp(0.0, (n - 1) as f64) as u32;
    let max_x = (bounds.max_x * n as f64).floor().clamp(0.0, (n - 1) as f64) as u32;
    let min_y = (bounds.min_y * n as f64).floor().clamp(0.0, (n - 1) as f64) as u32;
    let max_y = (bounds.max_y * n as f64).floor().clamp(0.0, (n - 1) as f64) as u32;
    let mut tiles = Vec::new();
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            tiles.push(TileCoord { z, x, y });
        }
    }
    tiles
}

pub fn tile_bounds(tile: TileCoord) -> Bounds {
    let n = 2_u32.pow(tile.z as u32) as f64;
    Bounds {
        min_x: tile.x as f64 / n,
        max_x: (tile.x + 1) as f64 / n,
        min_y: tile.y as f64 / n,
        max_y: (tile.y + 1) as f64 / n,
    }
}

pub fn tile_pixel_to_world(tile: TileCoord, px: u32, py: u32, size: u32) -> WorldPoint {
    let n = 2_u32.pow(tile.z as u32) as f64;
    WorldPoint {
        x: (tile.x as f64 + px as f64 / size as f64) / n,
        y: (tile.y as f64 + py as f64 / size as f64) / n,
    }
}

pub fn world_to_tile_pixel(point: WorldPoint, tile: TileCoord, size: u32) -> (f64, f64) {
    let n = 2_u32.pow(tile.z as u32) as f64;
    (
        (point.x * n - tile.x as f64) * size as f64,
        (point.y * n - tile.y as f64) * size as f64,
    )
}

pub fn world_span_at_zoom(z: u8) -> f64 {
    TILE_SIZE / (TILE_SIZE * 2_u32.pow(z as u32) as f64)
}

/// Returns tile coordinates covering `bounds` at zoom `z`, ordered
/// center-first in clockwise concentric rings.  The center tile is
/// determined from `center` (viewport centre in world coords).
/// This ensures tiles closest to the viewport centre are streamed
/// and rendered first during zoom/pan transitions.
pub fn tiles_spiral_from(bounds: Bounds, z: u8, center: WorldPoint) -> Vec<TileCoord> {
    let n = 2u64.pow(z as u32) as f64;
    let ct = tile_for_world(center, z);
    let cx = ct.x as i64;
    let cy = ct.y as i64;

    let min_tx = (bounds.min_x * n).floor().max(0.0) as i64;
    let max_tx = (bounds.max_x * n).floor().min(n - 1.0) as i64;
    let min_ty = (bounds.min_y * n).floor().max(0.0) as i64;
    let max_ty = (bounds.max_y * n).floor().min(n - 1.0) as i64;

    let max_ring = (cx - min_tx)
        .max(max_tx - cx)
        .max(cy - min_ty)
        .max(max_ty - cy);

    let mut tiles = Vec::new();

    // Centre tile first
    if cx >= min_tx && cx <= max_tx && cy >= min_ty && cy <= max_ty {
        tiles.push(TileCoord {
            z,
            x: cx as u32,
            y: cy as u32,
        });
    }

    for ring in 1..=max_ring {
        let top = cy - ring;
        let bottom = cy + ring;
        let left = cx - ring;
        let right = cx + ring;

        // Top edge left → right
        if top >= min_ty && top <= max_ty {
            let x0 = left.max(min_tx);
            let x1 = right.min(max_tx);
            for x in x0..=x1 {
                tiles.push(TileCoord {
                    z,
                    x: x as u32,
                    y: top as u32,
                });
            }
        }

        // Right edge top → bottom (skip first — already covered by top edge)
        if right >= min_tx && right <= max_tx {
            let y0 = (top + 1).max(min_ty);
            let y1 = bottom.min(max_ty);
            for y in y0..=y1 {
                tiles.push(TileCoord {
                    z,
                    x: right as u32,
                    y: y as u32,
                });
            }
        }

        // Bottom edge right → left (skip first)
        if bottom >= min_ty && bottom <= max_ty {
            let x0 = left.max(min_tx);
            let x1 = (right - 1).min(max_tx);
            for x in (x0..=x1).rev() {
                tiles.push(TileCoord {
                    z,
                    x: x as u32,
                    y: bottom as u32,
                });
            }
        }

        // Left edge bottom → top (skip first and last)
        if left >= min_tx && left <= max_tx {
            let y0 = (top + 1).max(min_ty);
            let y1 = (bottom - 1).min(max_ty);
            for y in (y0..=y1).rev() {
                tiles.push(TileCoord {
                    z,
                    x: left as u32,
                    y: y as u32,
                });
            }
        }
    }

    tiles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lat_lon_to_world_world_to_lat_lon_roundtrip() {
        for (lat, lon) in [(46.05, 14.51), (0.0, 0.0), (-33.9, 151.2), (51.5, -0.1)] {
            let w = lat_lon_to_world(lat, lon);
            let g = world_to_lat_lon(w);
            assert!((g.lat - lat).abs() < 0.001, "lat roundtrip {lat}");
            assert!((g.lon - lon).abs() < 0.001, "lon roundtrip {lon}");
        }
    }

    #[test]
    fn test_lat_lon_to_world_clamps_beyond_max_lat() {
        let p = lat_lon_to_world(MAX_LAT + 10.0, 0.0);
        assert_eq!(p.y, 0.0, "north pole clamps to y=0");
        let p2 = lat_lon_to_world(-(MAX_LAT + 10.0), 0.0);
        assert_eq!(p2.y, 1.0, "south pole clamps to y=1");
    }

    #[test]
    fn test_geopoint_new_clamps_lat_and_lon() {
        let g = GeoPoint::new(200.0, 100.0);
        assert_eq!(g.lon, 180.0);
        assert_eq!(g.lat, MAX_LAT);
        let g2 = GeoPoint::new(-200.0, -100.0);
        assert_eq!(g2.lon, -180.0);
        assert_eq!(g2.lat, -MAX_LAT);
    }

    #[test]
    fn test_near_european_capital_true_for_ljubljana() {
        // Ljubljana (46.05, 14.51) is in EUROPEAN_CAPITALS — must be near itself.
        assert!(near_european_capital(46.05, 14.51));
    }

    #[test]
    fn test_near_european_capital_false_for_remote_point() {
        // Mid-Atlantic, far from any capital.
        assert!(!near_european_capital(40.0, -40.0));
    }

    #[test]
    fn test_bounds_contains_subset() {
        let outer = Bounds {
            min_x: 0.0,
            max_x: 1.0,
            min_y: 0.0,
            max_y: 1.0,
        };
        let inner = Bounds {
            min_x: 0.1,
            max_x: 0.9,
            min_y: 0.1,
            max_y: 0.9,
        };
        assert!(outer.contains(inner));
        assert!(!inner.contains(outer));
    }

    #[test]
    fn test_bounds_contains_equal() {
        let b = Bounds {
            min_x: 0.2,
            max_x: 0.8,
            min_y: 0.2,
            max_y: 0.8,
        };
        assert!(b.contains(b));
    }

    #[test]
    fn test_tile_bounds_at_z1_covers_quadrant() {
        // At z=1 there are 4 tiles: (0,0), (1,0), (0,1), (1,1).
        // Tile (0,0) should cover [0,0.5]×[0,0.5].
        let b = tile_bounds(TileCoord { z: 1, x: 0, y: 0 });
        assert!((b.min_x - 0.0).abs() < 1e-9);
        assert!((b.max_x - 0.5).abs() < 1e-9);
        assert!((b.min_y - 0.0).abs() < 1e-9);
        assert!((b.max_y - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_tile_pixel_to_world_and_world_to_tile_pixel_roundtrip() {
        let tc = TileCoord { z: 4, x: 5, y: 3 };
        let size = 256u32;
        let px = 64u32;
        let py = 128u32;
        let world = tile_pixel_to_world(tc, px, py, size);
        let (rx, ry) = world_to_tile_pixel(world, tc, size);
        assert!((rx - px as f64).abs() < 1e-6, "pixel x roundtrip");
        assert!((ry - py as f64).abs() < 1e-6, "pixel y roundtrip");
    }

    #[test]
    fn test_world_span_at_zoom_halves_per_zoom_step() {
        let s0 = world_span_at_zoom(0);
        let s1 = world_span_at_zoom(1);
        assert!((s0 / s1 - 2.0).abs() < 1e-9, "span halves per zoom step");
    }

    #[test]
    fn test_viewport_pan_stays_in_bounds() {
        let mut vp = Viewport::from_lat_lon(EUROPE_LAT, EUROPE_LON, 4.0);
        for _ in 0..100 {
            vp.pan(10.0, 0.0);
        }
        assert!(vp.center.x <= 1.0);
        assert!(vp.center.x >= 0.0);
    }

    #[test]
    fn test_viewport_zoom_by_clamps_to_limits() {
        let mut vp = Viewport::from_lat_lon(0.0, 0.0, 4.0);
        vp.zoom_by(100.0);
        assert_eq!(vp.zoom, MAX_VIEW_ZOOM);
        vp.zoom_by(-200.0);
        assert_eq!(vp.zoom, MIN_VIEW_ZOOM);
    }

    #[test]
    fn projects_origin_to_world_center() {
        let point = lat_lon_to_world(0.0, 0.0);
        assert!((point.x - 0.5).abs() < 0.0001);
        assert!((point.y - 0.5).abs() < 0.0001);
    }

    #[test]
    fn tile_math_clamps_to_valid_range() {
        assert_eq!(tile_for_world(WorldPoint { x: 1.0, y: 1.0 }, 2).x, 3);
        assert_eq!(
            visible_tiles(
                Bounds {
                    min_x: 0.0,
                    max_x: 1.0,
                    min_y: 0.0,
                    max_y: 1.0,
                },
                1
            )
            .len(),
            4
        );
    }

    #[test]
    fn tiles_spiral_center_first() {
        // A 3×3 tile set at zoom 3, centre at (1, 1).
        let bounds = Bounds {
            min_x: 0.0,
            max_x: 0.3,
            min_y: 0.0,
            max_y: 0.3,
        };
        let center = WorldPoint { x: 0.15, y: 0.15 };
        let tiles = super::tiles_spiral_from(bounds, 3, center);
        assert!(!tiles.is_empty(), "should produce tile coords");
        // First tile must be the centre tile
        assert_eq!(tiles[0].x, 1, "first tile must be centre x");
        assert_eq!(tiles[0].y, 1, "first tile must be centre y");
        // All tiles must be unique
        let mut seen = std::collections::HashSet::new();
        for t in &tiles {
            assert!(seen.insert((t.x, t.y)), "duplicate tile ({}, {})", t.x, t.y);
        }
    }

    #[test]
    fn tiles_spiral_single_tile() {
        let bounds = Bounds {
            min_x: 0.0,
            max_x: 0.05,
            min_y: 0.0,
            max_y: 0.05,
        };
        let center = WorldPoint { x: 0.025, y: 0.025 };
        let tiles = super::tiles_spiral_from(bounds, 3, center);
        assert_eq!(tiles.len(), 1, "single tile at zoom 3 for tiny bounds");
    }

    #[test]
    fn cursor_anchored_zoom_keeps_world_point_under_cursor() {
        let mut viewport = Viewport::from_lat_lon(46.05, 14.51, 4.0);
        let before = viewport.world_at_screen(100, 50, 75, 20);
        viewport.zoom_around_screen(100, 50, 75, 20, 1.0);
        let after = viewport.world_at_screen(100, 50, 75, 20);
        assert!((before.x - after.x).abs() < 0.0001);
        assert!((before.y - after.y).abs() < 0.0001);
    }

    #[test]
    fn bounds_intersects_detects_overlap_and_disjoint() {
        let a = Bounds {
            min_x: 0.0,
            max_x: 0.5,
            min_y: 0.0,
            max_y: 0.5,
        };
        let b = Bounds {
            min_x: 0.4,
            max_x: 0.9,
            min_y: 0.4,
            max_y: 0.9,
        };
        let c = Bounds {
            min_x: 0.6,
            max_x: 1.0,
            min_y: 0.6,
            max_y: 1.0,
        };
        assert!(a.intersects(b), "touching edges should overlap");
        assert!(!a.intersects(c), "diagonal gap should be disjoint");
        assert!(
            !a.intersects(Bounds {
                min_x: 0.6,
                max_x: 1.0,
                min_y: 0.0,
                max_y: 0.4
            }),
            "x-gap disjoint"
        );
    }

    #[test]
    fn bounds_expanded_grows_centred() {
        let b = Bounds {
            min_x: 0.4,
            max_x: 0.6,
            min_y: 0.4,
            max_y: 0.6,
        };
        let e = b.expanded(1.0);
        assert!((e.width() - 0.4).abs() < 0.0001, "doubles width");
        assert!(
            ((e.min_x + e.max_x) / 2.0 - 0.5).abs() < 0.0001,
            "stays centred"
        );
    }

    #[test]
    fn bounds_expanded_clamps_to_unit() {
        let b = Bounds {
            min_x: 0.0,
            max_x: 0.1,
            min_y: 0.0,
            max_y: 0.1,
        };
        // fraction=20 → expand by 10× width (5 on each side), saturating [0,1]²
        let e = b.expanded(20.0);
        assert_eq!(e.min_x, 0.0);
        assert_eq!(e.max_x, 1.0);
        assert_eq!(e.min_y, 0.0);
        assert_eq!(e.max_y, 1.0);
    }
}
