use std::collections::HashMap;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::geo::{visible_tiles, Bounds, GeoPoint, TileCoord, WorldPoint};

/// Global render modes.  Each mode can be assigned to at most one layer
/// at a time so that different rendering techniques (braille dots,
/// background colour fills, text glyphs) never conflict on the same
/// terminal cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RenderMode {
    Braille,
    Color,
    Text,
}

impl RenderMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Braille => "b",
            Self::Color => "c",
            Self::Text => "l",
        }
    }
}

/// Tracks which layer (if any) currently owns each render mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderModeState {
    pub braille: Option<LayerId>,
    pub color: Option<LayerId>,
    pub text: Option<LayerId>,
}

impl Default for RenderModeState {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderModeState {
    pub fn new() -> Self {
        Self { braille: None, color: None, text: None }
    }

    /// Returns the layer assigned to `mode`, if any.
    pub fn get(&self, mode: RenderMode) -> Option<LayerId> {
        match mode {
            RenderMode::Braille => self.braille,
            RenderMode::Color => self.color,
            RenderMode::Text => self.text,
        }
    }

    /// Returns `true` when `layer` owns `mode`.
    pub fn has(&self, mode: RenderMode, layer: LayerId) -> bool {
        self.get(mode) == Some(layer)
    }

    /// Returns `true` when `layer` owns at least one mode.
    pub fn has_any(&self, layer: LayerId) -> bool {
        self.braille == Some(layer)
            || self.color == Some(layer)
            || self.text == Some(layer)
    }

    /// Returns all modes currently owned by `layer`.
    pub fn modes_for(&self, layer: LayerId) -> Vec<RenderMode> {
        let mut out = Vec::with_capacity(3);
        if self.braille == Some(layer) { out.push(RenderMode::Braille); }
        if self.color == Some(layer) { out.push(RenderMode::Color); }
        if self.text == Some(layer) { out.push(RenderMode::Text); }
        out
    }

    /// Assign `mode` to `layer`, removing it from any previous owner.
    /// Returns the previous owner, if any.
    pub fn assign(&mut self, mode: RenderMode, layer: LayerId) -> Option<LayerId> {
        
        match mode {
            RenderMode::Braille => self.braille.replace(layer),
            RenderMode::Color => self.color.replace(layer),
            RenderMode::Text => self.text.replace(layer),
        }
    }

    /// Toggle `mode` for `layer`: if `layer` already owns `mode` it is
    /// removed; otherwise it is assigned (removed from the previous owner).
    /// Returns the previous owner after the toggle, if any.
    pub fn toggle(&mut self, mode: RenderMode, layer: LayerId) -> Option<LayerId> {
        if self.has(mode, layer) {
            self.unassign(mode);
            None
        } else {
            self.assign(mode, layer)
        }
    }

    /// Remove whatever layer owns `mode`.
    pub fn unassign(&mut self, mode: RenderMode) {
        match mode {
            RenderMode::Braille => self.braille = None,
            RenderMode::Color => self.color = None,
            RenderMode::Text => self.text = None,
        }
    }

    /// Remove all render modes from `layer`.
    pub fn remove_all(&mut self, layer: LayerId) {
        if self.braille == Some(layer) { self.braille = None; }
        if self.color == Some(layer) { self.color = None; }
        if self.text == Some(layer) { self.text = None; }
    }

    /// Try to find the "best" (highest-information) mode for `layer`
    /// that is not already assigned to a different layer.  Returns
    /// `None` when all candidate modes are taken.
    pub fn best_available(&self, layer: LayerId) -> Option<RenderMode> {
        for &mode in preferred_modes(layer) {
            let owner = self.get(mode);
            if owner.is_none() || owner == Some(layer) {
                return Some(mode);
            }
        }
        None
    }
}

fn preferred_modes(id: LayerId) -> &'static [RenderMode] {
    match id {
        LayerId::Radar => &[RenderMode::Braille, RenderMode::Color, RenderMode::Text],
        LayerId::MeteoAlarm => &[RenderMode::Color, RenderMode::Braille, RenderMode::Text],
        id if id.is_observation() => {
            &[RenderMode::Text, RenderMode::Braille, RenderMode::Color]
        }
        _ => &[],
    }
}

/// Minimum world-coordinate width or height for a bounding box to be
/// considered non-degenerate.  Segments smaller than this are treated as
/// zero-area and will be ignored during spatial pre-filtering.
const MIN_BBOX_SPAN: f64 = 1e-12;

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct Rgb8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb8 {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub const WHITE: Self = Self::new(255, 255, 255);
    pub const GRAY: Self = Self::new(128, 128, 128);
    pub const DARK_GRAY: Self = Self::new(80, 80, 80);
    pub const GREEN: Self = Self::new(0, 255, 0);
    pub const RED: Self = Self::new(255, 0, 0);
    pub const AMBER: Self = Self::new(255, 191, 0);
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum LayerId {
    MapBorders,
    RegionBorders,
    MajorRoads,
    Radar,
    MeteoAlarm,
    SurfTemp,
    SurfWind,
    SurfHumidity,
    SurfPressure,
}

/// Top-level layer groups that contain sub-layers.
#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub enum LayerGroup {
    Observations,
}

impl LayerGroup {
    pub fn children(&self) -> [LayerId; 4] {
        match self {
            LayerGroup::Observations => [
                LayerId::SurfTemp,
                LayerId::SurfWind,
                LayerId::SurfHumidity,
                LayerId::SurfPressure,
            ],
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            LayerGroup::Observations => "Observations",
        }
    }
}

/// An entry in the top-level layer list — either a single togglable layer
/// or a group header that can be expanded to reveal sub-layers on the right.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MainItem {
    Single(LayerId),
    Group(LayerGroup),
}

impl MainItem {
    pub fn label(&self) -> &'static str {
        match self {
            MainItem::Single(id) => id.label(),
            MainItem::Group(g) => g.label(),
        }
    }

    pub fn is_single(&self) -> bool {
        matches!(self, MainItem::Single(_))
    }

    pub fn single_id(&self) -> Option<LayerId> {
        match self {
            MainItem::Single(id) => Some(*id),
            MainItem::Group(_) => None,
        }
    }
}

impl LayerId {
    pub fn label(self) -> &'static str {
        match self {
            Self::MapBorders => "Countries",
            Self::RegionBorders => "Regions",
            Self::MajorRoads => "Roads",
            Self::Radar => "Radar",
            Self::MeteoAlarm => "Warnings",
            Self::SurfTemp => "Temperature",
            Self::SurfWind => "Wind Speed",
            Self::SurfHumidity => "Humidity",
            Self::SurfPressure => "Pressure",
        }
    }

    pub fn observation_property(self) -> Option<ObservationProperty> {
        match self {
            Self::SurfTemp => Some(ObservationProperty::Temperature),
            Self::SurfWind => Some(ObservationProperty::WindSpeed),
            Self::SurfHumidity => Some(ObservationProperty::Humidity),
            Self::SurfPressure => Some(ObservationProperty::Pressure),
            _ => None,
        }
    }

    /// True for the station-observation layers (temperature, wind, humidity,
    /// pressure).  These share one near-real-time data source and render as
    /// text labels only.
    pub fn is_observation(self) -> bool {
        matches!(
            self,
            Self::SurfTemp | Self::SurfWind | Self::SurfHumidity | Self::SurfPressure
        )
    }

    /// Geographic layers (borders, roads) keep the old on/off toggle.
    /// Rendered/data layers (radar, warnings, observations) use the
    /// render-mode system instead.
    pub fn is_geographic(self) -> bool {
        matches!(
            self,
            Self::MapBorders | Self::RegionBorders | Self::MajorRoads
        )
    }

    pub fn is_rendered(self) -> bool {
        !self.is_geographic()
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ObservationProperty {
    Temperature,
    WindSpeed,
    Humidity,
    Pressure,
}

#[derive(Debug, Clone)]
pub struct LayerState {
    pub id: LayerId,
    pub enabled: bool,
    pub locked: bool,
    pub status: LayerStatus,
    pub updated_at: Option<SystemTime>,
}

#[derive(Debug, Clone)]
pub enum LayerStatus {
    Idle,
    Loading,
    Ready,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct LayerRegistry {
    states: HashMap<LayerId, LayerState>,
    selected: LayerId,
    selected_main: usize,
    expanded_group: Option<LayerGroup>,
    sub_cursor: usize,
    render_modes: RenderModeState,
}

impl Default for LayerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl LayerRegistry {
    fn layer_state(id: LayerId, enabled: bool, locked: bool) -> LayerState {
        LayerState {
            id,
            enabled,
            locked,
            status: LayerStatus::Idle,
            updated_at: None,
        }
    }

    pub fn new() -> Self {
        let mut states = HashMap::new();
        states.insert(
            LayerId::MapBorders,
            Self::layer_state(LayerId::MapBorders, true, true),
        );
        states.insert(
            LayerId::Radar,
            Self::layer_state(LayerId::Radar, true, false),
        );
        states.insert(
            LayerId::RegionBorders,
            Self::layer_state(LayerId::RegionBorders, true, false),
        );
        states.insert(
            LayerId::MajorRoads,
            Self::layer_state(LayerId::MajorRoads, false, false),
        );
        states.insert(
            LayerId::MeteoAlarm,
            Self::layer_state(LayerId::MeteoAlarm, false, false),
        );
        for id in [
            LayerId::SurfTemp,
            LayerId::SurfWind,
            LayerId::SurfHumidity,
            LayerId::SurfPressure,
        ] {
            states.insert(id, Self::layer_state(id, false, false));
        }
        let mut render_modes = RenderModeState::new();
        // Default: show temperature observations (text mode).
        // Without this, any_obs_enabled() is false on fresh installs and
        // observations never load until the user manually enables a layer.
        render_modes.assign(RenderMode::Text, LayerId::SurfTemp);

        Self {
            states,
            selected: LayerId::MapBorders,
            selected_main: 0,
            expanded_group: None,
            sub_cursor: 0,
            render_modes,
        }
    }

    pub fn selected_layer(&self) -> LayerId {
        if let Some(g) = self.expanded_group {
            let children = g.children();
            if self.sub_cursor > 0 && self.sub_cursor <= children.len() {
                return children[self.sub_cursor - 1];
            }
        }
        self.selected
    }

    pub fn select_next(&mut self) {
        if let Some(g) = self.expanded_group {
            let children = g.children();
            // sub_cursor 0=back, 1..=children.len()=sub-layers
            if self.sub_cursor < children.len() {
                self.sub_cursor += 1;
                return;
            }
            self.expanded_group = None;
            self.sub_cursor = 0;
            self.selected_main = (self.selected_main + 1) % Self::MAIN_ORDER.len();
        } else {
            self.selected_main = (self.selected_main + 1) % Self::MAIN_ORDER.len();
        }
        self.sync_selected();
    }

    pub fn select_previous(&mut self) {
        if self.expanded_group.is_some() {
            if self.sub_cursor > 0 {
                self.sub_cursor -= 1;
                return;
            }
            self.expanded_group = None;
            self.sub_cursor = 0;
            let len = Self::MAIN_ORDER.len();
            self.selected_main = (self.selected_main + len - 1) % len;
        } else {
            let len = Self::MAIN_ORDER.len();
            self.selected_main = (self.selected_main + len - 1) % len;
        }
        self.sync_selected();
    }

    pub fn handle_space(&mut self) -> Option<LayerId> {
        if let Some(g) = self.expanded_group {
            if self.sub_cursor == 0 {
                self.expanded_group = None;
                self.sub_cursor = 0;
                return None;
            }
            let children = g.children();
            let id = children[self.sub_cursor - 1];
            self.activate(id);
            return Some(id);
        }
        let item = Self::MAIN_ORDER[self.selected_main];
        match item {
            MainItem::Group(g) => {
                self.expanded_group = Some(g);
                self.sub_cursor = 0;
                None
            }
            MainItem::Single(id) => {
                self.activate(id);
                Some(id)
            }
        }
    }

    fn activate(&mut self, id: LayerId) {
        if id.is_geographic() {
            self.toggle(id);
        } else if id.is_observation() {
            // Observation layers: text mode only.  Space toggles Text
            // on/off – braille/color are not supported for station data.
            // Explicitly remove Text from all other obs layers so only
            // this one renders — the assign call alone should handle it,
            // but be paranoid about any path that might leave stale
            // modes on other layers (e.g. load_state).
            if self.render_modes.has(RenderMode::Text, id) {
                self.render_modes.remove_all(id);
            } else {
                for other in LayerRegistry::ORDER {
                    if other != id && other.is_observation() {
                        self.render_modes.remove_all(other);
                    }
                }
                self.render_modes.assign(RenderMode::Text, id);
            }
        } else if self.render_modes.has_any(id) {
            self.render_modes.remove_all(id);
        } else if let Some(mode) = self.render_modes.best_available(id) {
            self.render_modes.assign(mode, id);
        }
    }

    pub fn current_main(&self) -> MainItem {
        Self::MAIN_ORDER[self.selected_main]
    }

    pub fn selected_main_index(&self) -> usize {
        self.selected_main
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded_group.is_some()
    }

    pub fn expanded_group(&self) -> Option<LayerGroup> {
        self.expanded_group
    }

    pub fn sub_cursor(&self) -> usize {
        self.sub_cursor
    }

    fn sync_selected(&mut self) {
        if let Some(g) = self.expanded_group {
            let children = g.children();
            if self.sub_cursor > 0 && self.sub_cursor <= children.len() {
                self.selected = children[self.sub_cursor - 1];
                return;
            }
        }
        let item = Self::MAIN_ORDER[self.selected_main];
        self.selected = match item {
            MainItem::Single(id) => id,
            MainItem::Group(g) => g.children()[0],
        };
    }

    pub fn set_status(&mut self, id: LayerId, status: LayerStatus) {
        if let Some(state) = self.states.get_mut(&id) {
            state.status = status;
            state.updated_at = Some(SystemTime::now());
        }
    }

    pub fn toggle(&mut self, id: LayerId) {
        if let Some(state) = self.states.get_mut(&id) {
            if state.locked {
                return;
            }
            state.enabled = !state.enabled;
        }
    }

    pub fn enabled(&self, id: LayerId) -> bool {
        if id.is_geographic() {
            self.states
                .get(&id)
                .map(|state| state.enabled)
                .unwrap_or(false)
        } else {
            self.render_modes.has_any(id)
        }
    }

    pub fn mode_state(&self) -> &RenderModeState {
        &self.render_modes
    }

    pub fn mode_state_mut(&mut self) -> &mut RenderModeState {
        &mut self.render_modes
    }

    pub fn get_state(&self, id: LayerId) -> Option<&LayerState> {
        self.states.get(&id)
    }

    pub fn set_selected(&mut self, id: LayerId) {
        self.selected = id;
        for (i, item) in Self::MAIN_ORDER.iter().enumerate() {
            match item {
                MainItem::Single(lid) if *lid == id => {
                    self.selected_main = i;
                    self.expanded_group = None;
                    self.sub_cursor = 0;
                    return;
                }
                MainItem::Group(g) => {
                    let children = g.children();
                    if let Some(pos) = children.iter().position(|c| *c == id) {
                        self.selected_main = i;
                        self.expanded_group = Some(*g);
                        self.sub_cursor = pos + 1; // +1 for back button
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    /// Restore enabled states from saved data. Locked layers are preserved.
    pub fn restore_enabled(&mut self, enabled: &[LayerId]) {
        for state in self.states.values_mut() {
            if !state.locked {
                state.enabled = enabled.contains(&state.id);
            }
        }
    }

    /// Return the list of enabled layer IDs for serialization.
    pub fn saved_enabled(&self) -> Vec<LayerId> {
        Self::ORDER
            .into_iter()
            .filter(|id| self.states.get(id).is_some_and(|state| state.enabled))
            .collect()
    }

    pub const MAIN_ORDER: [MainItem; 6] = [
        MainItem::Single(LayerId::MapBorders),
        MainItem::Single(LayerId::RegionBorders),
        MainItem::Single(LayerId::MajorRoads),
        MainItem::Single(LayerId::Radar),
        MainItem::Group(LayerGroup::Observations),
        MainItem::Single(LayerId::MeteoAlarm),
    ];

    pub const ORDER: [LayerId; 9] = [
        LayerId::MapBorders,
        LayerId::RegionBorders,
        LayerId::MajorRoads,
        LayerId::Radar,
        LayerId::MeteoAlarm,
        LayerId::SurfTemp,
        LayerId::SurfWind,
        LayerId::SurfHumidity,
        LayerId::SurfPressure,
    ];

    pub fn ordered(&self) -> Vec<&LayerState> {
        Self::ORDER
            .into_iter()
            .filter_map(|id| self.states.get(&id))
            .collect()
    }

    /// Returns `true` when at least one enabled layer is currently
    /// loading.  The UI uses this to keep redrawing for animations.
    pub fn any_loading(&self) -> bool {
        for id in Self::ORDER {
            if let Some(s) = self.states.get(&id) {
                if s.enabled && matches!(s.status, LayerStatus::Loading) {
                    return true;
                }
            }
        }
        false
    }

    pub fn status_line(&self) -> String {
        for id in Self::ORDER {
            if let Some(LayerState {
                status: LayerStatus::Error(error),
                ..
            }) = self.states.get(&id)
            {
                if id != LayerId::MapBorders {
                    return format!("{}: {}", id.label(), error);
                }
            }
        }
        // Show a loading indicator only for layers the user can perceive
        // (radar data is the slowest to fetch).  Static layers (borders)
        // are loaded once at startup and don't need a re-load badge.
        for id in [LayerId::Radar, LayerId::MeteoAlarm] {
            if self
                .states
                .get(&id)
                .is_some_and(|s| matches!(s.status, LayerStatus::Loading) && s.enabled)
            {
                return format!("{}: refreshing…", id.label());
            }
        }
        let obs_loading = self
            .states
            .iter()
            .any(|(id, s)| {
                matches!(s.status, LayerStatus::Loading)
                    && s.enabled
                    && id.is_observation()
            });
        if obs_loading {
            return "Observations: refreshing…".to_string();
        }
        "Ready".to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BorderLayer {
    pub resolution: BorderResolution,
    pub lines: Vec<BorderLine>,
    /// Spatial grid index over `lines`.  A 16×16 fixed grid in world-
    /// coordinate space [0,1].  Each cell stores the indices of lines
    /// whose bbox overlaps that cell.  Used during mask recompute to
    /// skip lines that cannot intersect the viewport, reducing the
    /// per-frame scan from O(all_lines) to O(viewport_lines).
    pub grid: Option<SpatialGrid>,
}

/// Fixed-size spatial grid for fast viewport‑to‑line lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpatialGrid {
    /// Number of cells per axis (e.g. 16 → 16×16 = 256 cells).
    pub cells: u32,
    /// Concatenated `line_index` values for all cells, indexed by
    /// the offset table below.
    pub line_ids: Vec<u32>,
    /// Prefix‑sum offsets into `line_ids`.  Cell `i` spans
    /// `line_ids[offsets[i]..offsets[i+1]]`.  Length = cells*cells + 1.
    pub offsets: Vec<u32>,
}

impl SpatialGrid {
    pub fn build(lines: &[BorderLine]) -> Self {
        const CELLS: u32 = 16;
        let cell_w = 1.0 / CELLS as f64;
        let cell_h = 1.0 / CELLS as f64;
        let total_cells = (CELLS * CELLS) as usize;

        // First pass: count lines per cell.
        let mut counts = vec![0u32; total_cells];
        for line in lines.iter() {
            let min_cx = (line.bbox.min_x / cell_w).floor() as i32;
            let max_cx = (line.bbox.max_x / cell_w).ceil() as i32;
            let min_cy = (line.bbox.min_y / cell_h).floor() as i32;
            let max_cy = (line.bbox.max_y / cell_h).ceil() as i32;
            for cy in min_cy.max(0)..max_cy.min(CELLS as i32) {
                for cx in min_cx.max(0)..max_cx.min(CELLS as i32) {
                    counts[cy as usize * CELLS as usize + cx as usize] += 1;
                }
            }
        }

        // Build prefix-sum offsets.
        let mut offsets = vec![0u32; total_cells + 1];
        for i in 0..total_cells {
            offsets[i + 1] = offsets[i] + counts[i];
        }
        let total = offsets[total_cells] as usize;

        // Second pass: fill line_ids.
        let mut line_ids = vec![0u32; total];
        let mut cursor = offsets[..total_cells].to_vec(); // write position per cell
        for (line_idx, line) in lines.iter().enumerate() {
            let idx = line_idx as u32;
            let min_cx = (line.bbox.min_x / cell_w).floor() as i32;
            let max_cx = (line.bbox.max_x / cell_w).ceil() as i32;
            let min_cy = (line.bbox.min_y / cell_h).floor() as i32;
            let max_cy = (line.bbox.max_y / cell_h).ceil() as i32;
            for cy in min_cy.max(0)..max_cy.min(CELLS as i32) {
                for cx in min_cx.max(0)..max_cx.min(CELLS as i32) {
                    let cell = cy as usize * CELLS as usize + cx as usize;
                    line_ids[cursor[cell] as usize] = idx;
                    cursor[cell] += 1;
                }
            }
        }

        Self {
            cells: CELLS,
            line_ids,
            offsets,
        }
    }

    /// Returns the set of line indices whose bbox may overlap `bounds`,
    /// deduplicated via a small bitset (`seen` is a `u8` bitset).
    pub fn lines_for_bounds(&self, bounds: Bounds, out: &mut Vec<u32>, seen: &mut [u8]) {
        let cell_w = 1.0 / self.cells as f64;
        let cell_h = 1.0 / self.cells as f64;
        let min_cx = (bounds.min_x / cell_w).floor() as i32;
        let max_cx = (bounds.max_x / cell_w).ceil() as i32;
        let min_cy = (bounds.min_y / cell_h).floor() as i32;
        let max_cy = (bounds.max_y / cell_h).ceil() as i32;

        out.clear();
        for cy in min_cy.max(0)..max_cy.min(self.cells as i32) {
            for cx in min_cx.max(0)..max_cx.min(self.cells as i32) {
                let cell = cy as usize * self.cells as usize + cx as usize;
                let start = self.offsets[cell] as usize;
                let end = self.offsets[cell + 1] as usize;
                for &id in &self.line_ids[start..end] {
                    let idx = id as usize;
                    let byte = idx >> 3;
                    let bit = 1 << (idx & 7);
                    if seen[byte] & bit == 0 {
                        seen[byte] |= bit;
                        out.push(id);
                    }
                }
            }
        }
        // Reset seen bits for next use.
        for &id in out.iter() {
            let idx = id as usize;
            seen[idx >> 3] &= !(1 << (idx & 7));
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BorderLine {
    pub kind: BorderLineKind,
    pub points: Vec<WorldPoint>,
    /// Axis-aligned bounding box in world coordinates, used as a spatial
    /// pre-filter to skip lines wholly outside the viewport.
    #[serde(default)]
    pub bbox: Bounds,
}

impl BorderLine {
    /// Build `bbox` from `points` via min/max scan.
    pub fn compute_bbox(&mut self) {
        let mut min_x = f64::MAX;
        let mut max_x = f64::MIN;
        let mut min_y = f64::MAX;
        let mut max_y = f64::MIN;
        for p in &self.points {
            if p.x < min_x {
                min_x = p.x;
            }
            if p.x > max_x {
                max_x = p.x;
            }
            if p.y < min_y {
                min_y = p.y;
            }
            if p.y > max_y {
                max_y = p.y;
            }
        }
        self.bbox = Bounds {
            min_x,
            max_x: max_x.max(min_x + MIN_BBOX_SPAN),
            min_y,
            max_y: max_y.max(min_y + MIN_BBOX_SPAN),
        };
    }

    /// Returns `true` if the bbox is degenerate (collapsed to a point
    /// or to a zero-area default).  Cached border data loaded before
    /// the bbox field existed has `Bounds::default()` and looks
    /// degenerate — callers can use this to decide whether to fall
    /// back to the un-pre-filtered path.
    pub fn is_bbox_degenerate(&self) -> bool {
        self.bbox.width() < MIN_BBOX_SPAN || self.bbox.height() < MIN_BBOX_SPAN
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum BorderLineKind {
    Country,
    Region,
    Road,
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum BorderResolution {
    Low110m,
    Medium50m,
    High10m,
    Regional10m,
}

impl BorderResolution {
    /// Tile zoom level used for caching borders at this resolution.
    /// Each resolution covers a zoom band; the tile zoom determines how
    /// many tiles cover the viewport:
    ///   Low110m  → z=4 (~16 tiles cover the world)
    ///   Medium50m → z=5 (~64 tiles)
    ///   High10m   → z=6 (~256 tiles)
    ///   Regional10m → z=7 (~1024 tiles)
    pub fn tile_zoom(self) -> u8 {
        match self {
            Self::Low110m => 4,
            Self::Medium50m => 5,
            Self::High10m => 6,
            Self::Regional10m => 7,
        }
    }

    pub fn for_zoom(zoom: f64) -> Self {
        if zoom >= 7.0 {
            Self::Regional10m
        } else if zoom >= 4.5 {
            Self::High10m
        } else if zoom >= 3.5 {
            Self::Medium50m
        } else {
            Self::Low110m
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Low110m => "110m",
            Self::Medium50m => "50m",
            Self::High10m => "10m",
            Self::Regional10m => "10m+",
        }
    }

    pub fn country_scale(self) -> &'static str {
        match self {
            Self::Low110m => "110m",
            Self::Medium50m => "50m",
            Self::High10m | Self::Regional10m => "10m",
        }
    }

    pub fn includes_regions(self) -> bool {
        matches!(self, Self::High10m | Self::Regional10m)
    }
}

pub fn resolution_distance(a: BorderResolution, b: BorderResolution) -> u32 {
    fn rep_zoom(r: BorderResolution) -> u32 {
        match r {
            BorderResolution::Low110m => 30,
            BorderResolution::Medium50m => 45,
            BorderResolution::High10m => 60,
            BorderResolution::Regional10m => 80,
        }
    }
    rep_zoom(a).abs_diff(rep_zoom(b))
}

impl RadarFrame {
    pub fn merge_tiles(&mut self, frame: Self) {
        if self.time != frame.time || self.path != frame.path {
            *self = frame;
            return;
        }

        // If the user changed zoom and the new frame is at a different
        // tile-zoom, evict the stale tiles before merging.  Without
        // this, zoom-N and zoom-(N±1) tiles would render together
        // and produce the "huge squares + fine details" artifact.
        if self.target_zoom != frame.target_zoom {
            self.tiles.retain(|t| t.coord.z == frame.target_zoom);
        }
        self.target_zoom = frame.target_zoom;

        for tile in frame.tiles {
            if !self
                .tiles
                .iter()
                .any(|existing| existing.coord == tile.coord)
            {
                self.tiles.push(tile);
            }
        }
        self.missing_tiles = frame.missing_tiles;
    }

    pub fn covers_bounds(&self, bounds: Bounds, z: u8) -> bool {
        let tiles = visible_tiles(bounds, z);
        tiles
            .into_iter()
            .all(|coord| self.tiles.iter().any(|tile| tile.coord == coord))
    }
}

#[derive(Debug, Clone)]
pub struct RadarFrame {
    pub time: i64,
    pub path: String,
    pub tiles: Vec<RadarTile>,
    pub missing_tiles: usize,
    /// The discrete Web-Mercator zoom level that all tiles in this
    /// frame were built at.  Used by `merge_tiles` to evict tiles from
    /// a previous zoom level when the user zooms in or out.
    pub target_zoom: u8,
}

#[derive(Debug, Clone)]
pub struct RadarBatch {
    pub color: Rgb8,
    pub coords: Vec<(f64, f64)>,
}

#[derive(Debug, Clone)]
pub struct RadarTile {
    pub coord: TileCoord,
    pub size: u32,
    pub rows: Vec<Vec<RadarRun>>,
}

#[derive(Debug, Clone, Copy)]
pub struct RadarRun {
    pub start_x: u16,
    pub end_x: u16,
    pub color: Rgb8,
    pub intensity: u8,
}

#[derive(Debug, Clone)]
pub struct LocationFix {
    pub point: GeoPoint,
    pub label: String,
}

/// A single MeteoAlarm weather warning feature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WarningFeature {
    /// World-coordinate polygon vertices (exterior ring).
    pub polygon: Vec<WorldPoint>,
    /// Severity level: "green", "yellow", "orange", "red"
    pub awareness_level: String,
    /// Human-readable event type (e.g. "thunderstorm", "wind")
    pub event: String,
    /// ISO 3166-1 alpha-2 country code
    pub country_code: String,
    /// UTC onset timestamp
    pub onset: Option<i64>,
    /// UTC expiry timestamp
    pub expires: Option<i64>,
}

/// Collection of active warnings for rendering.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WarningLayer {
    pub features: Vec<WarningFeature>,
    pub updated_at: Option<i64>,
}

/// A single observation from a weather station.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationPoint {
    /// Geographic position.
    pub point: GeoPoint,
    /// World-coordinate position (pre-computed).
    pub world: WorldPoint,
    /// Station identifier or name.
    pub station_id: String,
    /// Air temperature in °C.
    pub temperature: Option<f64>,
    /// Wind speed in m/s.
    pub wind_speed: Option<f64>,
    /// Wind direction in degrees (0-360).
    pub wind_direction: Option<f64>,
    /// Relative humidity in %.
    pub humidity: Option<f64>,
    /// Atmospheric pressure in hPa.
    pub pressure: Option<f64>,
}

/// Collection of observation points for rendering.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObservationLayer {
    pub points: Vec<ObservationPoint>,
    pub updated_at: Option<i64>,
}

impl WarningFeature {
    /// Return a display color based on awareness level.
    pub fn color(&self) -> Rgb8 {
        match self.awareness_level.as_str() {
            "red" | "4; red; Extreme" => Rgb8::RED,
            "orange" | "3; orange; Severe" => Rgb8::new(255, 165, 0),
            "yellow" | "2; yellow; Moderate" => Rgb8::AMBER,
            _ => Rgb8::GREEN,
        }
    }

    /// Return a human-readable severity label.
    pub fn severity_label(&self) -> &str {
        if self.awareness_level.contains("red") {
            "Red"
        } else if self.awareness_level.contains("orange") {
            "Orange"
        } else if self.awareness_level.contains("yellow") {
            "Yellow"
        } else {
            "Green"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_line(pts: &[(f64, f64)]) -> BorderLine {
        let points: Vec<WorldPoint> = pts.iter().map(|&(x, y)| WorldPoint { x, y }).collect();
        let mut line = BorderLine {
            kind: BorderLineKind::Country,
            points,
            bbox: Bounds::default(),
        };
        line.compute_bbox();
        line
    }

    fn tile(z: u8, x: u32, y: u32) -> RadarTile {
        RadarTile {
            coord: TileCoord { z, x, y },
            size: 256,
            rows: Vec::new(),
        }
    }

    fn frame(time: i64, z: u8, tiles: Vec<RadarTile>) -> RadarFrame {
        RadarFrame {
            time,
            path: format!("DBZH/{time}"),
            tiles,
            missing_tiles: 0,
            target_zoom: z,
        }
    }

    #[test]
    fn merge_tiles_at_same_zoom_merges() {
        let mut a = frame(100, 4, vec![tile(4, 0, 0), tile(4, 1, 0)]);
        let b = frame(100, 4, vec![tile(4, 1, 0), tile(4, 2, 0)]);
        a.merge_tiles(b);
        assert_eq!(a.tiles.len(), 3, "duplicate (1,0) is deduped");
        assert_eq!(a.target_zoom, 4);
    }

    #[test]
    fn merge_tiles_at_different_zoom_evicts_stale() {
        // Regression: previously, zoom-3 and zoom-4 tiles would coexist
        // in the same frame, producing the "huge squares + fine
        // details" visual artifact.  Merging a higher-zoom frame
        // should evict the lower-zoom tiles.
        let mut a = frame(100, 3, vec![tile(3, 0, 0), tile(3, 1, 1)]);
        let b = frame(100, 4, vec![tile(4, 5, 5), tile(4, 6, 6)]);
        a.merge_tiles(b);
        let zooms: Vec<u8> = a.tiles.iter().map(|t| t.coord.z).collect();
        assert!(zooms.iter().all(|&z| z == 4), "stale zoom-3 tiles must be evicted");
        assert_eq!(a.tiles.len(), 2);
        assert_eq!(a.target_zoom, 4);
    }

    #[test]
    fn merge_tiles_with_different_time_replaces() {
        let mut a = frame(100, 4, vec![tile(4, 0, 0)]);
        let b = frame(200, 4, vec![tile(4, 1, 1)]);
        a.merge_tiles(b);
        assert_eq!(a.time, 200);
        assert_eq!(a.tiles.len(), 1);
    }

    #[test]
    fn regions_visible_at_high_and_regional_zoom() {
        // Regions/roads should be loaded and drawn at zoom ≥ 5.5.
        assert!(!BorderResolution::Low110m.includes_regions());
        assert!(!BorderResolution::Medium50m.includes_regions());
        assert!(BorderResolution::High10m.includes_regions());
        assert!(BorderResolution::Regional10m.includes_regions());
    }

    // -----------------------------------------------------------------
    // SpatialGrid tests
    // -----------------------------------------------------------------

    #[test]
    fn spatial_grid_empty_lines() {
        let grid = SpatialGrid::build(&[]);
        let mut out = Vec::new();
        let mut seen = vec![0u8; 1];
        let bounds = Bounds { min_x: 0.0, max_x: 1.0, min_y: 0.0, max_y: 1.0 };
        grid.lines_for_bounds(bounds, &mut out, &mut seen);
        assert!(out.is_empty(), "no lines → no candidates");
    }

    #[test]
    fn spatial_grid_single_line_found() {
        let lines = vec![make_line(&[(0.1, 0.2), (0.3, 0.4)])];
        let grid = SpatialGrid::build(&lines);
        let mut out = Vec::new();
        let mut seen = vec![0u8; 1];
        grid.lines_for_bounds(
            Bounds { min_x: 0.0, max_x: 0.5, min_y: 0.0, max_y: 0.5 },
            &mut out, &mut seen,
        );
        assert_eq!(out, vec![0], "line inside bounds must be found");
    }

    #[test]
    fn spatial_grid_single_line_missed() {
        let lines = vec![make_line(&[(0.8, 0.8), (0.9, 0.9)])];
        let grid = SpatialGrid::build(&lines);
        let mut out = Vec::new();
        let mut seen = vec![0u8; 1];
        grid.lines_for_bounds(
            Bounds { min_x: 0.0, max_x: 0.5, min_y: 0.0, max_y: 0.5 },
            &mut out, &mut seen,
        );
        assert!(out.is_empty(), "line outside bounds must be skipped");
    }

    #[test]
    fn spatial_grid_dedup_crossing_cell_boundary() {
        // A single long line that spans multiple grid cells — must
        // appear exactly once in the candidate set.
        let lines = vec![make_line(&[(0.0, 0.0), (1.0, 1.0)])];
        let grid = SpatialGrid::build(&lines);
        let mut out = Vec::new();
        let mut seen = vec![0u8; 1];
        grid.lines_for_bounds(
            Bounds { min_x: 0.0, max_x: 1.0, min_y: 0.0, max_y: 1.0 },
            &mut out, &mut seen,
        );
        assert_eq!(out, vec![0], "line crossing cells must be deduped");
    }

    #[test]
    fn spatial_grid_multiple_lines() {
        let lines = vec![
            make_line(&[(0.1, 0.1), (0.2, 0.2)]),
            make_line(&[(0.8, 0.8), (0.9, 0.9)]),
            make_line(&[(0.4, 0.4), (0.6, 0.6)]),
        ];
        let grid = SpatialGrid::build(&lines);
        let mut out = Vec::new();
        let mut seen = vec![0u8; 1];
        grid.lines_for_bounds(
            Bounds { min_x: 0.15, max_x: 0.7, min_y: 0.15, max_y: 0.7 },
            &mut out, &mut seen,
        );
        // Line 0 ends at 0.2 → partial overlap with query (0.15-0.7).
        // Line 1 is in 0.8-0.9 → no overlap.
        // Line 2 is entirely inside 0.4-0.6 → full overlap.
        assert_eq!(out, vec![0, 2], "only lines 0 and 2 intersect the query");
    }

    #[test]
    fn spatial_grid_reuse_clears_seen() {
        // Second call must not be contaminated by the first call's
        // seen bits (the bitset reset in lines_for_bounds must work).
        let lines = vec![make_line(&[(0.1, 0.1), (0.2, 0.2)])];
        let grid = SpatialGrid::build(&lines);
        let mut out = Vec::new();
        let mut seen = vec![0u8; 1];

        grid.lines_for_bounds(
            Bounds { min_x: 0.0, max_x: 0.5, min_y: 0.0, max_y: 0.5 },
            &mut out, &mut seen,
        );
        assert_eq!(out, vec![0], "first call finds line 0");

        grid.lines_for_bounds(
            Bounds { min_x: 0.6, max_x: 1.0, min_y: 0.6, max_y: 1.0 },
            &mut out, &mut seen,
        );
        assert!(out.is_empty(), "second call with disjoint bounds must return empty");
    }
}
