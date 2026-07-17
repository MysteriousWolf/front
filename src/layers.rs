use std::collections::HashMap;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::geo::{tile_bounds, visible_tiles, Bounds, GeoPoint, TileCoord, WorldPoint};

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
    /// Every render mode, for iterating the primary/overlay slots.
    pub const ALL: [RenderMode; 3] = [RenderMode::Braille, RenderMode::Color, RenderMode::Text];

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
    /// Non-exclusive overlay owners, as (mode, layer) pairs.
    ///
    /// A *list*, not one slot per mode: several layers may overlay the same
    /// mode at once and none of them evicts the primary owner. The location
    /// marker and the search pin both overlay `Text`, and they answer
    /// different questions — "where am I" versus "where is the place I
    /// searched" — so a search must never blank the marker. Lightning's
    /// braille dots sit in here alongside radar's primary braille.
    #[serde(default)]
    pub overlays: Vec<(RenderMode, LayerId)>,
}

impl Default for RenderModeState {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderModeState {
    pub fn new() -> Self {
        Self {
            braille: None,
            color: None,
            text: None,
            overlays: Vec::new(),
        }
    }

    /// Returns the *primary* layer assigned to `mode`, if any.
    /// Does not report the overlay braille layer.
    pub fn get(&self, mode: RenderMode) -> Option<LayerId> {
        match mode {
            RenderMode::Braille => self.braille,
            RenderMode::Color => self.color,
            RenderMode::Text => self.text,
        }
    }

    /// Every layer overlaying `mode`, in the order they were switched on.
    /// An overlay draws on top of the primary owner instead of evicting it.
    pub fn overlays_for(&self, mode: RenderMode) -> impl Iterator<Item = LayerId> + '_ {
        self.overlays
            .iter()
            .filter(move |(m, _)| *m == mode)
            .map(|(_, l)| *l)
    }

    /// True when `layer` overlays `mode`.
    pub fn has_overlay(&self, mode: RenderMode, layer: LayerId) -> bool {
        self.overlays.contains(&(mode, layer))
    }

    /// Add `layer` as an overlay of `mode`, leaving other overlays and the
    /// primary owner untouched.
    pub fn set_overlay(&mut self, mode: RenderMode, layer: LayerId) {
        if !self.has_overlay(mode, layer) {
            self.overlays.push((mode, layer));
        }
    }

    /// Remove `layer`'s overlay of `mode`, leaving other overlays alone.
    pub fn clear_overlay(&mut self, mode: RenderMode, layer: LayerId) {
        self.overlays.retain(|entry| *entry != (mode, layer));
    }

    /// Returns `true` when `layer` owns `mode` (primary or overlay).
    pub fn has(&self, mode: RenderMode, layer: LayerId) -> bool {
        self.get(mode) == Some(layer) || self.has_overlay(mode, layer)
    }

    /// Returns `true` when `layer` owns at least one mode (primary or overlay).
    pub fn has_any(&self, layer: LayerId) -> bool {
        RenderMode::ALL.iter().any(|&m| self.has(m, layer))
    }

    /// Returns all modes currently owned by `layer` (primary or overlay).
    pub fn modes_for(&self, layer: LayerId) -> Vec<RenderMode> {
        RenderMode::ALL
            .into_iter()
            .filter(|&m| self.has(m, layer))
            .collect()
    }

    /// Assign `mode` to `layer`, removing it from any previous primary owner.
    /// Returns the previous primary owner, if any.
    pub fn assign(&mut self, mode: RenderMode, layer: LayerId) -> Option<LayerId> {
        match mode {
            RenderMode::Braille => self.braille.replace(layer),
            RenderMode::Color => self.color.replace(layer),
            RenderMode::Text => self.text.replace(layer),
        }
    }

    /// Toggle overlay braille for `layer` without affecting the primary slot.
    pub fn toggle_braille_overlay(&mut self, layer: LayerId) {
        self.toggle_overlay(RenderMode::Braille, layer);
    }

    /// Toggle `layer` as an overlay of `mode`, leaving the primary owner and
    /// every other overlay untouched.
    pub fn toggle_overlay(&mut self, mode: RenderMode, layer: LayerId) {
        if self.has_overlay(mode, layer) {
            self.clear_overlay(mode, layer);
        } else {
            self.set_overlay(mode, layer);
        }
    }

    /// Toggle `mode` for `layer`: if `layer` already owns `mode` it is
    /// removed; otherwise it is assigned.
    ///
    /// Layers listed in `overlay_modes` for this mode go to the overlay slot,
    /// so they never evict the primary owner — the Location marker must not
    /// take Text away from the temperature layer just to draw one glyph.
    /// Returns the previous primary owner, if any.
    pub fn toggle(&mut self, mode: RenderMode, layer: LayerId) -> Option<LayerId> {
        if overlay_modes(layer).contains(&mode) {
            self.toggle_overlay(mode, layer);
            return None;
        }
        if self.has(mode, layer) {
            self.unassign(mode);
            None
        } else {
            self.assign(mode, layer)
        }
    }

    /// Put `layer` into the slot it belongs in for `mode` — overlay for the
    /// declared exceptions, primary otherwise.  Used when loading state, where
    /// both kinds are persisted with the same mode tag.
    pub fn restore(&mut self, mode: RenderMode, layer: LayerId) {
        if overlay_modes(layer).contains(&mode) {
            self.set_overlay(mode, layer);
        } else {
            self.assign(mode, layer);
        }
    }

    /// Remove the primary owner of `mode`.
    pub fn unassign(&mut self, mode: RenderMode) {
        match mode {
            RenderMode::Braille => self.braille = None,
            RenderMode::Color => self.color = None,
            RenderMode::Text => self.text = None,
        }
    }

    /// Remove all modes (primary and overlay) from `layer`, leaving every
    /// other layer's overlays in place.
    pub fn remove_all(&mut self, layer: LayerId) {
        for mode in RenderMode::ALL {
            if self.get(mode) == Some(layer) {
                self.unassign(mode);
            }
        }
        self.overlays.retain(|(_, l)| *l != layer);
    }

    /// Try to find the "best" (highest-information) primary mode for `layer`
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
        LayerId::Lightning => &[RenderMode::Braille, RenderMode::Color, RenderMode::Text],
        // Text (the red `x`) first: it marks the spot without hiding the cell
        // underneath the way the background does.
        id if id.is_pin() => &[RenderMode::Text, RenderMode::Color],
        id if id.is_observation() => &[RenderMode::Text, RenderMode::Braille, RenderMode::Color],
        _ => &[],
    }
}

/// Modes a layer draws as a non-evicting *overlay* rather than claiming the
/// exclusive primary slot.
///
/// These are the deliberate exceptions to "one layer per mode": Lightning's
/// braille dots coexist with radar braille, and the Location marker is a
/// single annotated cell that must not cost the map its temperature readings
/// or radar colour.  Overlays draw on top of whatever owns the primary slot.
fn overlay_modes(id: LayerId) -> &'static [RenderMode] {
    match id {
        LayerId::Lightning => &[RenderMode::Braille],
        id if id.is_pin() => &[RenderMode::Text, RenderMode::Color],
        _ => &[],
    }
}

/// A typed option value that the UI renders generically without knowing the
/// layer type.  Each option is paired with a stable `key` string used to
/// identify which option the user changed.
#[derive(Debug, Clone)]
pub enum LayerOption {
    /// A boolean on/off toggle.
    Toggle {
        label: &'static str,
        value: bool,
        /// True when the underlying layer is in an error state.
        has_error: bool,
    },
    /// A single value chosen from a static list of strings.
    Choice {
        label: &'static str,
        value: usize,
        options: &'static [&'static str],
    },
    /// An integer that steps between `min` and `max` (inclusive) in `step`
    /// increments.  Space cycles forward; wraps from `max` back to `min`.
    Range {
        label: &'static str,
        value: u32,
        min: u32,
        max: u32,
        step: u32,
        unit: &'static str,
    },
}

impl LayerOption {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Toggle { label, .. } | Self::Choice { label, .. } | Self::Range { label, .. } => {
                label
            }
        }
    }

    /// True when this option represents an active / enabled state.
    pub fn is_active(&self) -> bool {
        match self {
            Self::Toggle { value, .. } => *value,
            Self::Choice { .. } | Self::Range { .. } => false,
        }
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
    /// The blue already used by the humidity and pressure ramps, promoted to a
    /// named palette entry so the search pin matches the rest of the map
    /// rather than introducing a tone of its own.  A pure (0,0,255) would pair
    /// with `RED` on paper but is barely legible on the dark background.
    pub const BLUE: Self = Self::new(70, 130, 215);
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum LayerId {
    MapBorders,
    RegionBorders,
    MajorRoads,
    Radar,
    MeteoAlarm,
    Lightning,
    SurfTemp,
    SurfWind,
    SurfHumidity,
    SurfPressure,
    Location,
    SearchPin,
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
            LayerGroup::Observations => "Measurements",
        }
    }
}

/// An entry in the top-level layer list.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MainItem {
    /// Toggleable single layer.
    Single(LayerId),
    /// Expandable group whose sub-layers appear in the options panel.
    Group(LayerGroup),
    /// Non-interactive section divider (never selectable).
    Header(&'static str),
}

impl MainItem {
    pub fn label(&self) -> &'static str {
        match self {
            MainItem::Single(id) => id.label(),
            MainItem::Group(g) => g.label(),
            MainItem::Header(s) => s,
        }
    }

    pub fn is_single(&self) -> bool {
        matches!(self, MainItem::Single(_))
    }

    pub fn single_id(&self) -> Option<LayerId> {
        match self {
            MainItem::Single(id) => Some(*id),
            _ => None,
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
            Self::Lightning => "Lightning",
            Self::SurfTemp => "Temperature",
            Self::SurfWind => "Wind Speed",
            Self::SurfHumidity => "Humidity",
            Self::SurfPressure => "Pressure",
            Self::Location => "My Location",
            Self::SearchPin => "Search Pin",
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

    /// True for the lightning layer.
    pub fn is_lightning(self) -> bool {
        matches!(self, Self::Lightning)
    }

    /// True for the "my location" marker layer.
    pub fn is_location(self) -> bool {
        matches!(self, Self::Location)
    }

    /// True for the single-cell map pins: the location marker and the search
    /// pin.  They share rendering, options and overlay behaviour, differing
    /// only in colour and where the point comes from.
    pub fn is_pin(self) -> bool {
        matches!(self, Self::Location | Self::SearchPin)
    }

    /// Layers with a plain enabled/disabled flag and no render-mode options.
    /// Everything else is driven by the render-mode system, where "enabled"
    /// means "owns a mode".
    pub fn is_simple_toggle(self) -> bool {
        self.is_geographic()
    }

    pub fn is_rendered(self) -> bool {
        !self.is_simple_toggle()
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

/// Which of the two panels holds keyboard focus.
/// `Options(i)` = focus is in the right panel at option index `i`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelFocus {
    Main,
    Options(usize),
}

#[derive(Debug, Clone)]
pub struct LayerRegistry {
    states: HashMap<LayerId, LayerState>,
    selected: LayerId,
    selected_main: usize,
    focus: PanelFocus,
    render_modes: RenderModeState,
    /// Trail duration for the Lightning layer in minutes (1–30).
    pub lightning_trail_minutes: u8,
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
        states.insert(
            LayerId::Lightning,
            Self::layer_state(LayerId::Lightning, false, false),
        );
        states.insert(
            LayerId::Location,
            Self::layer_state(LayerId::Location, true, false),
        );
        // The search pin has nothing to show until `/` finds a place, so it
        // owns no mode until then; `App::set_search_pin` turns it on.
        states.insert(
            LayerId::SearchPin,
            Self::layer_state(LayerId::SearchPin, false, false),
        );
        // Temperature enabled by default: any_obs_enabled() must be true on
        // fresh installs so observation fetches trigger immediately.
        states.insert(
            LayerId::SurfTemp,
            Self::layer_state(LayerId::SurfTemp, true, false),
        );
        for id in [
            LayerId::SurfWind,
            LayerId::SurfHumidity,
            LayerId::SurfPressure,
        ] {
            states.insert(id, Self::layer_state(id, false, false));
        }

        let mut render_modes = RenderModeState::new();
        render_modes.assign(RenderMode::Braille, LayerId::Radar);
        render_modes.assign(RenderMode::Text, LayerId::SurfTemp);
        // The marker is on by default as a text overlay: it costs SurfTemp
        // nothing and draws only once a fix arrives.
        render_modes.set_overlay(RenderMode::Text, LayerId::Location);

        Self {
            states,
            selected: LayerId::Radar, // first selectable item in MAIN_ORDER
            selected_main: 1,
            focus: PanelFocus::Main,
            render_modes,
            lightning_trail_minutes: 5,
        }
    }

    /// True when the main-list item at `index` can receive keyboard focus.
    /// Headers are never selectable; locked single layers are not selectable.
    pub fn is_main_selectable(&self, index: usize) -> bool {
        match Self::MAIN_ORDER[index] {
            MainItem::Header(_) => false,
            MainItem::Single(id) => !self.states.get(&id).is_some_and(|s| s.locked),
            MainItem::Group(_) => true,
        }
    }

    // ── Focus accessors ────────────────────────────────────────────────

    pub fn focus(&self) -> PanelFocus {
        self.focus
    }

    /// Returns the cursor position when the options panel has focus, or `None`
    /// when focus is in the main list.
    pub fn options_cursor(&self) -> Option<usize> {
        match self.focus {
            PanelFocus::Options(i) => Some(i),
            PanelFocus::Main => None,
        }
    }

    /// True when focus is in the options panel.
    pub fn is_in_options(&self) -> bool {
        matches!(self.focus, PanelFocus::Options(_))
    }

    /// When the options panel is focused and the current item is a group,
    /// returns that group — None otherwise.
    pub fn focused_group(&self) -> Option<LayerGroup> {
        match (self.focus, Self::MAIN_ORDER[self.selected_main]) {
            (PanelFocus::Options(_), MainItem::Group(g)) => Some(g),
            _ => None,
        }
    }

    /// Backward-compatible name: true when a group is open in the options panel.
    pub fn is_expanded(&self) -> bool {
        self.focused_group().is_some()
    }

    // ── Navigation ─────────────────────────────────────────────────────

    /// Number of options for the current main item (avoids allocating a Vec).
    fn current_option_count(&self) -> usize {
        match Self::MAIN_ORDER[self.selected_main] {
            MainItem::Header(_) => 0,
            MainItem::Single(id) if id.is_simple_toggle() => 0,
            MainItem::Single(id) if id.is_observation() => 1,
            // Pins: text (coloured x) and background — no braille.
            MainItem::Single(id) if id.is_pin() => 2,
            // Lightning has 3 render-mode toggles + 1 trail-duration range.
            MainItem::Single(id) if id.is_lightning() => 4,
            MainItem::Single(_) => 3,
            MainItem::Group(g) => g.children().len(),
        }
    }

    /// Step `index` forward by 1, skipping non-selectable items, with wrap-around.
    fn next_selectable(&self, from: usize) -> usize {
        let len = Self::MAIN_ORDER.len();
        let mut i = (from + 1) % len;
        while !self.is_main_selectable(i) && i != from {
            i = (i + 1) % len;
        }
        i
    }

    /// Step `index` backward by 1, skipping non-selectable items, with wrap-around.
    fn prev_selectable(&self, from: usize) -> usize {
        let len = Self::MAIN_ORDER.len();
        let mut i = (from + len - 1) % len;
        while !self.is_main_selectable(i) && i != from {
            i = (i + len - 1) % len;
        }
        i
    }

    /// Move down — wraps at the end of whichever panel has focus,
    /// skipping non-selectable items in the main list.
    pub fn select_next(&mut self) {
        match self.focus {
            PanelFocus::Options(i) => {
                let n = self.current_option_count();
                if n > 0 {
                    self.focus = PanelFocus::Options((i + 1) % n);
                }
            }
            PanelFocus::Main => {
                self.selected_main = self.next_selectable(self.selected_main);
            }
        }
        self.sync_selected();
    }

    /// Move up — wraps at the beginning of whichever panel has focus,
    /// skipping non-selectable items in the main list.
    pub fn select_previous(&mut self) {
        match self.focus {
            PanelFocus::Options(i) => {
                let n = self.current_option_count();
                if n > 0 {
                    self.focus = PanelFocus::Options((i + n - 1) % n);
                }
            }
            PanelFocus::Main => {
                self.selected_main = self.prev_selectable(self.selected_main);
            }
        }
        self.sync_selected();
    }

    /// Enter the options panel for any item that has options.  No-op when
    /// already in options or when the current item has no options.
    /// Returns `true` when focus actually moved.
    pub fn enter_options(&mut self) -> bool {
        if self.is_in_options() || self.current_option_count() == 0 {
            return false;
        }
        self.focus = PanelFocus::Options(0);
        self.sync_selected();
        true
    }

    /// Return focus to the main list.  Returns `true` when focus moved.
    pub fn exit_options(&mut self) -> bool {
        if !self.is_in_options() {
            return false;
        }
        self.focus = PanelFocus::Main;
        self.sync_selected();
        true
    }

    /// Toggle or activate the currently focused item.
    ///
    /// In the main list: toggle a single layer or enter a group.
    /// In the options panel: apply the focused option.
    /// Returns the affected `LayerId` when a layer's state changed.
    pub fn activate_selected(&mut self) -> Option<LayerId> {
        match self.focus {
            PanelFocus::Options(i) => {
                let item = Self::MAIN_ORDER[self.selected_main];
                self.apply_option_at(item, i)
            }
            PanelFocus::Main => {
                let item = Self::MAIN_ORDER[self.selected_main];
                match item {
                    MainItem::Header(_) => None,
                    MainItem::Group(_) => {
                        self.enter_options();
                        None
                    }
                    MainItem::Single(id) => {
                        self.activate(id);
                        Some(id)
                    }
                }
            }
        }
    }

    /// Apply the option at `index` for `item`.  Shared by `activate_selected` and
    /// any future direct-click path.
    fn apply_option_at(&mut self, item: MainItem, index: usize) -> Option<LayerId> {
        match item {
            MainItem::Header(_) => None,
            MainItem::Single(id) if id.is_observation() => {
                if index == 0 {
                    self.activate(id);
                    Some(id)
                } else {
                    None
                }
            }
            MainItem::Single(id) if id.is_pin() => {
                // Both are overlays, so neither evicts the temperature or
                // radar layers; `toggle` routes them by `overlay_modes`.
                let mode = if index == 0 {
                    RenderMode::Text
                } else {
                    RenderMode::Color
                };
                self.render_modes.toggle(mode, id);
                Some(id)
            }
            MainItem::Single(id) if id.is_lightning() => match index {
                0 => {
                    // Braille overlay — non-exclusive, coexists with radar braille.
                    self.render_modes.toggle_braille_overlay(id);
                    Some(id)
                }
                1 => {
                    self.render_modes.toggle(RenderMode::Color, id);
                    Some(id)
                }
                2 => {
                    self.render_modes.toggle(RenderMode::Text, id);
                    Some(id)
                }
                _ => {
                    // Cycle trail duration: increment by 1, wrap 30 → 1.
                    self.lightning_trail_minutes = if self.lightning_trail_minutes >= 30 {
                        1
                    } else {
                        self.lightning_trail_minutes + 1
                    };
                    Some(id)
                }
            },
            MainItem::Single(id) => {
                let mode = match index {
                    0 => RenderMode::Braille,
                    1 => RenderMode::Color,
                    _ => RenderMode::Text,
                };
                self.render_modes.toggle(mode, id);
                Some(id)
            }
            MainItem::Group(g) => {
                let children = g.children();
                if index < children.len() {
                    let id = children[index];
                    self.activate(id);
                    Some(id)
                } else {
                    None
                }
            }
        }
    }

    fn activate(&mut self, id: LayerId) {
        if id.is_simple_toggle() {
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
        } else if !overlay_modes(id).is_empty() {
            // Overlay layers (Lightning, Location) default to their first
            // preferred mode in the overlay slot so they never evict radar or
            // the temperature readings.  has_any checks overlays too.
            if self.render_modes.has_any(id) {
                self.render_modes.remove_all(id);
            } else if let Some(&mode) = preferred_modes(id).first() {
                self.render_modes.toggle_overlay(mode, id);
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

    pub fn selected_layer(&self) -> LayerId {
        self.selected
    }

    fn sync_selected(&mut self) {
        self.selected = match (self.focus, Self::MAIN_ORDER[self.selected_main]) {
            (PanelFocus::Options(i), MainItem::Group(g)) => {
                let children = g.children();
                children.get(i).copied().unwrap_or(children[0])
            }
            (_, MainItem::Single(id)) => id,
            (_, MainItem::Group(g)) => g.children()[0],
            (_, MainItem::Header(_)) => self.selected, // headers don't change the selection
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
        if id.is_simple_toggle() {
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
        for (i, item) in Self::MAIN_ORDER.iter().enumerate() {
            match item {
                MainItem::Single(lid) if *lid == id => {
                    // Only move selection to selectable items; locked layers
                    // (e.g. Countries) keep the current selection instead.
                    if self.is_main_selectable(i) {
                        self.selected_main = i;
                        self.focus = PanelFocus::Main;
                        self.selected = id;
                    }
                    return;
                }
                MainItem::Group(g) => {
                    let children = g.children();
                    if let Some(pos) = children.iter().position(|c| *c == id) {
                        self.selected_main = i;
                        self.focus = PanelFocus::Options(pos);
                        self.selected = id;
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    /// Restore enabled states from saved data. Locked layers are preserved.
    /// Layer set as of the last state.toml format that had no `known_layers`
    /// field.  Anything absent from this list postdates those files, so a
    /// legacy state.toml says nothing about it and it must keep its default.
    const LEGACY_KNOWN_LAYERS: [LayerId; 10] = [
        LayerId::MapBorders,
        LayerId::RegionBorders,
        LayerId::MajorRoads,
        LayerId::Radar,
        LayerId::MeteoAlarm,
        LayerId::Lightning,
        LayerId::SurfTemp,
        LayerId::SurfWind,
        LayerId::SurfHumidity,
        LayerId::SurfPressure,
    ];

    /// The layers a saved state file knew about.  An empty `known_layers`
    /// means the file predates the field, so fall back to the layer set of
    /// that era.
    pub fn known_from_state(known: &[LayerId]) -> Vec<LayerId> {
        if known.is_empty() {
            Self::LEGACY_KNOWN_LAYERS.to_vec()
        } else {
            known.to_vec()
        }
    }

    /// Apply saved enabled flags.
    ///
    /// `known` lists the layers the saved file actually knew about; a layer
    /// outside it keeps its default rather than being disabled for having been
    /// absent from a file written before it existed.  Pass an empty `known`
    /// for legacy files.
    pub fn restore_enabled(&mut self, enabled: &[LayerId], known: &[LayerId]) {
        let known = Self::known_from_state(known);
        for state in self.states.values_mut() {
            if state.locked || !known.contains(&state.id) {
                continue;
            }
            state.enabled = enabled.contains(&state.id);
        }
    }

    /// Every layer this build knows about — persisted as `known_layers` so a
    /// future release can tell "off" from "did not exist".
    pub fn known_layers(&self) -> Vec<LayerId> {
        Self::ORDER.to_vec()
    }

    /// Return the list of enabled layer IDs for serialization.
    pub fn saved_enabled(&self) -> Vec<LayerId> {
        Self::ORDER
            .into_iter()
            .filter(|id| self.states.get(id).is_some_and(|state| state.enabled))
            .collect()
    }

    /// Returns the options for a given main-list item, each paired with a
    /// stable key string.  Geographic layers have no options — they use
    /// the `[x]/[ ]` toggle in the main list directly.  Rendered and
    /// grouped layers expose their per-layer knobs through this method so
    /// the UI can render them generically without switching on layer type.
    pub fn options_for_item(&self, item: MainItem) -> Vec<(&'static str, LayerOption)> {
        match item {
            MainItem::Header(_) => vec![],
            MainItem::Single(id) if id.is_simple_toggle() => vec![],
            MainItem::Single(id) if id.is_observation() => vec![(
                "text",
                LayerOption::Toggle {
                    label: "Text",
                    value: self.render_modes.has(RenderMode::Text, id),
                    has_error: matches!(
                        self.states.get(&id).map(|s| &s.status),
                        Some(LayerStatus::Error(_))
                    ),
                },
            )],
            MainItem::Single(id) if id.is_pin() => vec![
                (
                    "text",
                    LayerOption::Toggle {
                        label: "Text",
                        value: self.render_modes.has(RenderMode::Text, id),
                        has_error: matches!(
                            self.states.get(&id).map(|s| &s.status),
                            Some(LayerStatus::Error(_))
                        ),
                    },
                ),
                (
                    "background",
                    LayerOption::Toggle {
                        label: "Background",
                        value: self.render_modes.has(RenderMode::Color, id),
                        has_error: matches!(
                            self.states.get(&id).map(|s| &s.status),
                            Some(LayerStatus::Error(_))
                        ),
                    },
                ),
            ],
            MainItem::Single(id) if id.is_lightning() => vec![
                (
                    "braille",
                    LayerOption::Toggle {
                        label: "Braille",
                        value: self.render_modes.has(RenderMode::Braille, id),
                        has_error: false,
                    },
                ),
                (
                    "color",
                    LayerOption::Toggle {
                        label: "Color",
                        value: self.render_modes.has(RenderMode::Color, id),
                        has_error: false,
                    },
                ),
                (
                    "text",
                    LayerOption::Toggle {
                        label: "Text",
                        value: self.render_modes.has(RenderMode::Text, id),
                        has_error: false,
                    },
                ),
                (
                    "trail",
                    LayerOption::Range {
                        label: "Trail",
                        value: u32::from(self.lightning_trail_minutes),
                        min: 1,
                        max: 30,
                        step: 1,
                        unit: "min",
                    },
                ),
            ],
            MainItem::Single(id) => vec![
                (
                    "braille",
                    LayerOption::Toggle {
                        label: "Braille",
                        value: self.render_modes.has(RenderMode::Braille, id),
                        has_error: false,
                    },
                ),
                (
                    "color",
                    LayerOption::Toggle {
                        label: "Color",
                        value: self.render_modes.has(RenderMode::Color, id),
                        has_error: false,
                    },
                ),
                (
                    "text",
                    LayerOption::Toggle {
                        label: "Text",
                        value: self.render_modes.has(RenderMode::Text, id),
                        has_error: false,
                    },
                ),
            ],
            MainItem::Group(g) => g
                .children()
                .iter()
                .map(|&id| {
                    (
                        id.label(),
                        LayerOption::Toggle {
                            label: id.label(),
                            value: self.render_modes.has(RenderMode::Text, id),
                            has_error: matches!(
                                self.states.get(&id).map(|s| &s.status),
                                Some(LayerStatus::Error(_))
                            ),
                        },
                    )
                })
                .collect(),
        }
    }

    pub const MAIN_ORDER: [MainItem; 11] = [
        // Weather layers first
        MainItem::Header("Weather"),
        MainItem::Single(LayerId::Radar),
        MainItem::Group(LayerGroup::Observations),
        MainItem::Single(LayerId::MeteoAlarm),
        MainItem::Single(LayerId::Lightning),
        // Geography layers below — Roads on top, Countries fixed at bottom
        MainItem::Header("Geography"),
        MainItem::Single(LayerId::Location),
        MainItem::Single(LayerId::SearchPin),
        MainItem::Single(LayerId::MajorRoads),
        MainItem::Single(LayerId::RegionBorders),
        MainItem::Single(LayerId::MapBorders), // Countries — locked, not selectable
    ];

    pub const ORDER: [LayerId; 12] = [
        LayerId::MapBorders,
        LayerId::RegionBorders,
        LayerId::MajorRoads,
        LayerId::Radar,
        LayerId::MeteoAlarm,
        LayerId::Lightning,
        LayerId::SurfTemp,
        LayerId::SurfWind,
        LayerId::SurfHumidity,
        LayerId::SurfPressure,
        LayerId::Location,
        LayerId::SearchPin,
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
        for id in [LayerId::Radar, LayerId::MeteoAlarm, LayerId::Lightning] {
            if self
                .states
                .get(&id)
                .is_some_and(|s| matches!(s.status, LayerStatus::Loading) && s.enabled)
            {
                return format!("{}: refreshing…", id.label());
            }
        }
        let obs_loading = self.states.iter().any(|(id, s)| {
            matches!(s.status, LayerStatus::Loading) && s.enabled && id.is_observation()
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
    /// coordinate space \[0,1\].  Each cell stores the indices of lines
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

    /// Remove tiles whose bounds don't intersect `bounds`.  Keeps the
    /// tile list bounded during extended panning sessions.
    pub fn trim_to_bounds(&mut self, bounds: Bounds) {
        self.tiles
            .retain(|t| bounds.intersects(tile_bounds(t.coord)));
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

    // ── RenderModeState ────────────────────────────────────────────────

    #[test]
    fn test_render_mode_state_assign_and_get() {
        let mut state = RenderModeState::new();
        state.assign(RenderMode::Braille, LayerId::Radar);
        assert_eq!(state.get(RenderMode::Braille), Some(LayerId::Radar));
        assert_eq!(state.get(RenderMode::Color), None);
    }

    #[test]
    fn test_render_mode_state_assign_replaces_previous_owner() {
        let mut state = RenderModeState::new();
        let prev = state.assign(RenderMode::Braille, LayerId::Radar);
        assert_eq!(prev, None);
        let prev2 = state.assign(RenderMode::Braille, LayerId::MeteoAlarm);
        assert_eq!(prev2, Some(LayerId::Radar));
        assert_eq!(state.get(RenderMode::Braille), Some(LayerId::MeteoAlarm));
    }

    #[test]
    fn test_render_mode_state_toggle_on_then_off() {
        let mut state = RenderModeState::new();
        state.toggle(RenderMode::Color, LayerId::Radar);
        assert!(state.has(RenderMode::Color, LayerId::Radar));
        state.toggle(RenderMode::Color, LayerId::Radar);
        assert!(!state.has(RenderMode::Color, LayerId::Radar));
    }

    #[test]
    fn test_render_mode_state_remove_all_clears_all_modes() {
        let mut state = RenderModeState::new();
        state.assign(RenderMode::Braille, LayerId::Radar);
        state.assign(RenderMode::Color, LayerId::Radar);
        state.assign(RenderMode::Text, LayerId::Radar);
        state.set_overlay(RenderMode::Braille, LayerId::Radar);
        state.remove_all(LayerId::Radar);
        assert!(!state.has_any(LayerId::Radar));
    }

    #[test]
    fn test_render_mode_state_has_any_braille_overlay() {
        let mut state = RenderModeState::new();
        state.set_overlay(RenderMode::Braille, LayerId::Lightning);
        assert!(state.has_any(LayerId::Lightning));
        assert!(state.has(RenderMode::Braille, LayerId::Lightning));
    }

    #[test]
    fn test_render_mode_state_toggle_braille_overlay_on_off() {
        let mut state = RenderModeState::new();
        state.toggle_braille_overlay(LayerId::Lightning);
        assert!(state.has_overlay(RenderMode::Braille, LayerId::Lightning));
        state.toggle_braille_overlay(LayerId::Lightning);
        assert!(!state.has_overlay(RenderMode::Braille, LayerId::Lightning));
    }

    #[test]
    fn test_render_mode_state_best_available_prefers_braille_for_radar() {
        let state = RenderModeState::new();
        assert_eq!(
            state.best_available(LayerId::Radar),
            Some(RenderMode::Braille)
        );
    }

    #[test]
    fn test_render_mode_state_best_available_skips_taken_mode() {
        let mut state = RenderModeState::new();
        state.assign(RenderMode::Braille, LayerId::MeteoAlarm);
        // Radar prefers Braille first but it's taken by another layer — falls back to Color.
        assert_eq!(
            state.best_available(LayerId::Radar),
            Some(RenderMode::Color)
        );
    }

    #[test]
    fn test_render_mode_state_modes_for_returns_owned_modes() {
        let mut state = RenderModeState::new();
        state.assign(RenderMode::Braille, LayerId::Radar);
        state.assign(RenderMode::Color, LayerId::Radar);
        let modes = state.modes_for(LayerId::Radar);
        assert!(modes.contains(&RenderMode::Braille));
        assert!(modes.contains(&RenderMode::Color));
        assert!(!modes.contains(&RenderMode::Text));
    }

    // ── LayerRegistry ──────────────────────────────────────────────────

    #[test]
    fn test_layer_registry_geographic_layer_enabled_by_state() {
        let reg = LayerRegistry::new();
        assert!(reg.enabled(LayerId::MapBorders), "MapBorders on by default");
        assert!(
            !reg.enabled(LayerId::MajorRoads),
            "MajorRoads off by default"
        );
    }

    #[test]
    fn test_layer_registry_rendered_layer_enabled_via_render_mode() {
        let reg = LayerRegistry::new();
        // Radar is assigned Braille in new() → enabled returns true.
        assert!(reg.enabled(LayerId::Radar));
        // MeteoAlarm has no mode by default → disabled.
        assert!(!reg.enabled(LayerId::MeteoAlarm));
    }

    #[test]
    fn test_layer_registry_toggle_geographic_layer() {
        let mut reg = LayerRegistry::new();
        // RegionBorders starts enabled.
        assert!(reg.get_state(LayerId::RegionBorders).unwrap().enabled);
        reg.toggle(LayerId::RegionBorders);
        assert!(!reg.get_state(LayerId::RegionBorders).unwrap().enabled);
        reg.toggle(LayerId::RegionBorders);
        assert!(reg.get_state(LayerId::RegionBorders).unwrap().enabled);
    }

    #[test]
    fn test_layer_registry_toggle_locked_layer_is_noop() {
        let mut reg = LayerRegistry::new();
        let before = reg.get_state(LayerId::MapBorders).unwrap().enabled;
        reg.toggle(LayerId::MapBorders);
        assert_eq!(reg.get_state(LayerId::MapBorders).unwrap().enabled, before);
    }

    #[test]
    fn test_layer_registry_lightning_trail_wraps_from_30_to_1() {
        let mut reg = LayerRegistry::new();
        reg.lightning_trail_minutes = 30;
        // Activating Lightning option index 3 increments trail.
        let item = LayerRegistry::MAIN_ORDER
            .iter()
            .find(|i| matches!(i, MainItem::Single(LayerId::Lightning)))
            .copied()
            .unwrap();
        reg.apply_option_at(item, 3);
        assert_eq!(reg.lightning_trail_minutes, 1, "30 wraps to 1");
    }

    #[test]
    fn test_layer_registry_lightning_trail_increments() {
        let mut reg = LayerRegistry::new();
        let initial = reg.lightning_trail_minutes;
        let item = LayerRegistry::MAIN_ORDER
            .iter()
            .find(|i| matches!(i, MainItem::Single(LayerId::Lightning)))
            .copied()
            .unwrap();
        reg.apply_option_at(item, 3);
        assert_eq!(reg.lightning_trail_minutes, initial + 1);
    }

    #[test]
    fn test_layer_registry_navigate_skips_headers() {
        let mut reg = LayerRegistry::new();
        let start = reg.selected_main_index();
        reg.select_next();
        // We must not land on a Header item.
        let cur = LayerRegistry::MAIN_ORDER[reg.selected_main_index()];
        assert!(!matches!(cur, MainItem::Header(_)));
        let _ = start; // suppress unused-variable warning
    }

    #[test]
    fn test_layer_registry_enter_and_exit_options() {
        let mut reg = LayerRegistry::new();
        // Navigate to Radar (index 1) — it has 3 options.
        while LayerRegistry::MAIN_ORDER[reg.selected_main_index()]
            != MainItem::Single(LayerId::Radar)
        {
            reg.select_next();
        }
        assert!(reg.enter_options(), "entering options for Radar");
        assert!(reg.is_in_options());
        assert!(reg.exit_options(), "exiting options");
        assert!(!reg.is_in_options());
    }

    #[test]
    fn test_layer_registry_observation_only_one_active_at_a_time() {
        let mut reg = LayerRegistry::new();
        // SurfTemp starts with Text mode by default.
        assert!(reg.mode_state().has(RenderMode::Text, LayerId::SurfTemp));
        // Activating SurfWind via the group options (index 1 = SurfWind) should
        // move Text from SurfTemp to SurfWind.
        reg.apply_option_at(MainItem::Group(LayerGroup::Observations), 1);
        assert!(!reg.mode_state().has(RenderMode::Text, LayerId::SurfTemp));
        assert!(reg.mode_state().has(RenderMode::Text, LayerId::SurfWind));
    }

    // ── WarningFeature ─────────────────────────────────────────────────

    #[test]
    fn test_warning_feature_color_by_severity() {
        let make = |level: &str| WarningFeature {
            polygon: vec![],
            awareness_level: level.to_string(),
            event: "storm".to_string(),
            country_code: "SI".to_string(),
            onset: None,
            expires: None,
        };
        assert_eq!(make("red").color(), Rgb8::RED);
        assert_eq!(make("4; red; Extreme").color(), Rgb8::RED);
        assert_eq!(make("orange").color(), Rgb8::new(255, 165, 0));
        assert_eq!(make("yellow").color(), Rgb8::AMBER);
        assert_eq!(make("green").color(), Rgb8::GREEN);
    }

    #[test]
    fn test_warning_feature_severity_label() {
        let make = |level: &str| WarningFeature {
            polygon: vec![],
            awareness_level: level.to_string(),
            event: "wind".to_string(),
            country_code: "DE".to_string(),
            onset: None,
            expires: None,
        };
        assert_eq!(make("red").severity_label(), "Red");
        assert_eq!(make("3; orange; Severe").severity_label(), "Orange");
        assert_eq!(make("2; yellow; Moderate").severity_label(), "Yellow");
        assert_eq!(make("green").severity_label(), "Green");
    }

    // ── BorderLine / BorderResolution ─────────────────────────────────

    #[test]
    fn test_border_line_compute_bbox_from_points() {
        let mut line = BorderLine {
            kind: BorderLineKind::Country,
            points: vec![
                WorldPoint { x: 0.1, y: 0.3 },
                WorldPoint { x: 0.7, y: 0.2 },
                WorldPoint { x: 0.4, y: 0.8 },
            ],
            bbox: Bounds::default(),
        };
        line.compute_bbox();
        assert!((line.bbox.min_x - 0.1).abs() < 1e-9);
        assert!((line.bbox.max_x - 0.7).abs() < 1e-9);
        assert!((line.bbox.min_y - 0.2).abs() < 1e-9);
        assert!((line.bbox.max_y - 0.8).abs() < 1e-9);
    }

    #[test]
    fn test_border_resolution_for_zoom_thresholds() {
        assert_eq!(BorderResolution::for_zoom(3.0), BorderResolution::Low110m);
        assert_eq!(BorderResolution::for_zoom(3.5), BorderResolution::Medium50m);
        assert_eq!(BorderResolution::for_zoom(4.5), BorderResolution::High10m);
        assert_eq!(
            BorderResolution::for_zoom(7.0),
            BorderResolution::Regional10m
        );
    }

    // ── LayerRegistry: status_line / any_loading / saved_enabled ──────

    #[test]
    fn test_layer_registry_status_line_ready_by_default() {
        let reg = LayerRegistry::new();
        assert_eq!(reg.status_line(), "Ready");
    }

    #[test]
    fn test_layer_registry_status_line_shows_loading_radar() {
        let mut reg = LayerRegistry::new();
        reg.set_status(LayerId::Radar, LayerStatus::Loading);
        assert!(
            reg.status_line().contains("Radar"),
            "loading radar must appear in status line"
        );
    }

    #[test]
    fn test_layer_registry_status_line_shows_error() {
        let mut reg = LayerRegistry::new();
        reg.set_status(LayerId::MeteoAlarm, LayerStatus::Error("timeout".into()));
        let line = reg.status_line();
        assert!(
            line.contains("timeout"),
            "error message must appear in status line"
        );
    }

    #[test]
    fn test_layer_registry_any_loading_false_when_idle() {
        let reg = LayerRegistry::new();
        assert!(!reg.any_loading());
    }

    #[test]
    fn test_layer_registry_any_loading_true_when_enabled_layer_loads() {
        let mut reg = LayerRegistry::new();
        // Radar is enabled by default (braille mode assigned).
        reg.set_status(LayerId::Radar, LayerStatus::Loading);
        assert!(reg.any_loading());
    }

    #[test]
    fn test_layer_registry_saved_enabled_and_restore_roundtrip() {
        let mut reg = LayerRegistry::new();
        let saved = reg.saved_enabled();
        // Disable everything we can, then restore.
        reg.toggle(LayerId::RegionBorders);
        assert!(!reg.get_state(LayerId::RegionBorders).unwrap().enabled);
        let known = reg.known_layers();
        reg.restore_enabled(&saved, &known);
        assert!(reg.get_state(LayerId::RegionBorders).unwrap().enabled);
    }

    /// The marker is one annotated cell — enabling it must never cost the map
    /// its temperature readings, which own the exclusive Text slot.
    #[test]
    fn location_text_does_not_evict_the_temperature_layer() {
        let mut reg = LayerRegistry::new();
        // Location defaults to the text overlay; start from a clean slate.
        reg.mode_state_mut().remove_all(LayerId::Location);
        reg.mode_state_mut().assign(RenderMode::Text, LayerId::SurfTemp);
        reg.mode_state_mut().toggle(RenderMode::Text, LayerId::Location);
        assert!(reg.mode_state().has(RenderMode::Text, LayerId::Location));
        assert!(
            reg.mode_state().has(RenderMode::Text, LayerId::SurfTemp),
            "SurfTemp must keep the primary Text slot"
        );
    }

    /// The marker ships on, drawn as a text overlay, and must not have taken
    /// the exclusive Text slot from the temperature readings to do it.
    #[test]
    fn location_is_a_text_overlay_by_default_and_temps_keep_primary_text() {
        let reg = LayerRegistry::new();
        assert!(reg.enabled(LayerId::Location), "marker on by default");
        assert!(reg.mode_state().has_overlay(RenderMode::Text, LayerId::Location));
        assert_eq!(reg.mode_state().text, Some(LayerId::SurfTemp));
    }

    /// The two pins answer different questions — "where am I" and "where is
    /// the place I searched for" — so showing one must never hide the other.
    #[test]
    fn location_marker_and_search_pin_coexist() {
        let mut modes = RenderModeState::new();
        modes.toggle_overlay(RenderMode::Text, LayerId::Location);
        modes.toggle_overlay(RenderMode::Text, LayerId::SearchPin);
        assert!(
            modes.has(RenderMode::Text, LayerId::Location),
            "the location marker must survive a search"
        );
        assert!(modes.has(RenderMode::Text, LayerId::SearchPin));
    }

    /// …and the same for their backgrounds, alongside the primary owner.
    #[test]
    fn both_pins_overlay_color_without_evicting_radar() {
        let mut modes = RenderModeState::new();
        modes.assign(RenderMode::Color, LayerId::Radar);
        modes.toggle_overlay(RenderMode::Color, LayerId::Location);
        modes.toggle_overlay(RenderMode::Color, LayerId::SearchPin);
        assert!(modes.has(RenderMode::Color, LayerId::Radar));
        assert!(modes.has(RenderMode::Color, LayerId::Location));
        assert!(modes.has(RenderMode::Color, LayerId::SearchPin));
    }

    /// Turning one pin off must leave the other alone.
    #[test]
    fn clearing_one_pin_leaves_the_other_showing() {
        let mut modes = RenderModeState::new();
        modes.toggle_overlay(RenderMode::Text, LayerId::Location);
        modes.toggle_overlay(RenderMode::Text, LayerId::SearchPin);
        modes.remove_all(LayerId::SearchPin);
        assert!(modes.has(RenderMode::Text, LayerId::Location));
        assert!(!modes.has(RenderMode::Text, LayerId::SearchPin));
    }

    /// Lightning's braille overlay must still coexist with the pins' modes.
    #[test]
    fn lightning_overlay_is_unaffected_by_the_pins() {
        let mut modes = RenderModeState::new();
        modes.assign(RenderMode::Braille, LayerId::Radar);
        modes.toggle_overlay(RenderMode::Braille, LayerId::Lightning);
        modes.toggle_overlay(RenderMode::Text, LayerId::Location);
        modes.toggle_overlay(RenderMode::Text, LayerId::SearchPin);
        assert!(modes.has(RenderMode::Braille, LayerId::Lightning));
        assert!(modes.has(RenderMode::Braille, LayerId::Radar));
    }

    /// Likewise the red background must not take Color away from radar.
    #[test]
    fn location_background_does_not_evict_the_color_owner() {
        let mut reg = LayerRegistry::new();
        reg.mode_state_mut().assign(RenderMode::Color, LayerId::Radar);
        reg.mode_state_mut().toggle(RenderMode::Color, LayerId::Location);
        assert!(reg.mode_state().has(RenderMode::Color, LayerId::Location));
        assert!(
            reg.mode_state().has(RenderMode::Color, LayerId::Radar),
            "Radar must keep the primary Color slot"
        );
    }

    #[test]
    fn location_modes_toggle_off_again() {
        let mut reg = LayerRegistry::new();
        reg.mode_state_mut().toggle(RenderMode::Color, LayerId::Location);
        assert!(reg.mode_state().has(RenderMode::Color, LayerId::Location));
        reg.mode_state_mut().toggle(RenderMode::Color, LayerId::Location);
        assert!(!reg.mode_state().has(RenderMode::Color, LayerId::Location));
    }

    /// Both pins persist as `Text` entries, so the load path must keep them
    /// both rather than letting the second win.
    #[test]
    fn both_pins_survive_a_save_load_round_trip() {
        let mut saved = RenderModeState::new();
        saved.assign(RenderMode::Text, LayerId::SurfTemp);
        saved.set_overlay(RenderMode::Text, LayerId::Location);
        saved.set_overlay(RenderMode::Text, LayerId::SearchPin);

        // What save_state writes: primaries, then every overlay pair.
        let mut persisted: Vec<(RenderMode, LayerId)> = Vec::new();
        for mode in RenderMode::ALL {
            if let Some(id) = saved.get(mode) {
                persisted.push((mode, id));
            }
        }
        persisted.extend(saved.overlays.iter().copied());

        // What load_state does: clear the known layers, then restore.
        let mut loaded = RenderModeState::new();
        for id in LayerRegistry::ORDER {
            loaded.remove_all(id);
        }
        for (mode, layer) in persisted {
            loaded.restore(mode, layer);
        }

        assert!(loaded.has(RenderMode::Text, LayerId::SurfTemp), "primary");
        assert!(loaded.has(RenderMode::Text, LayerId::Location));
        assert!(loaded.has(RenderMode::Text, LayerId::SearchPin));
    }

    /// Overlays are persisted with the same mode tag as primaries, so the
    /// load path must route them back to the overlay slot rather than
    /// evicting whoever owns the primary.
    #[test]
    fn restoring_an_overlay_layer_does_not_evict_the_primary_owner() {
        let mut modes = RenderModeState::new();
        modes.restore(RenderMode::Text, LayerId::SurfTemp);
        modes.restore(RenderMode::Text, LayerId::Location);
        assert_eq!(modes.text, Some(LayerId::SurfTemp));
        assert!(modes.has_overlay(RenderMode::Text, LayerId::Location));

        modes.restore(RenderMode::Braille, LayerId::Radar);
        modes.restore(RenderMode::Braille, LayerId::Lightning);
        assert_eq!(modes.braille, Some(LayerId::Radar));
        assert!(modes.has_overlay(RenderMode::Braille, LayerId::Lightning));
    }

    /// A state.toml written before a layer existed must not switch it off:
    /// `Location` is absent from every legacy file but defaults to on.
    #[test]
    fn test_layer_absent_from_legacy_state_keeps_its_default() {
        let mut reg = LayerRegistry::new();
        assert!(reg.enabled(LayerId::Location), "on by default");
        // A legacy file: no known_layers, and Location predates it.
        let legacy_enabled = vec![LayerId::MapBorders, LayerId::RegionBorders, LayerId::Radar];
        reg.restore_enabled(&legacy_enabled, &[]);
        assert!(
            reg.enabled(LayerId::Location),
            "legacy state.toml says nothing about Location, so it stays on"
        );
    }

    /// Once a file does know about a layer, "off" must be honoured.
    /// `MajorRoads` is a simple-toggle layer, so `enabled` tracks the flag.
    #[test]
    fn test_layer_known_to_state_file_can_be_restored_disabled() {
        let mut reg = LayerRegistry::new();
        let known = reg.known_layers();
        assert!(reg.enabled(LayerId::RegionBorders), "on by default");
        reg.restore_enabled(&[LayerId::MapBorders], &known);
        assert!(
            !reg.enabled(LayerId::RegionBorders),
            "the file knew RegionBorders and left it out, so it is genuinely off"
        );
    }

    // ── RadarFrame::covers_bounds ──────────────────────────────────────

    #[test]
    fn test_radar_frame_covers_bounds_true_when_all_tiles_present() {
        use crate::geo::{Bounds, TileCoord};
        // At z=1 the full world needs tiles (0,0), (1,0), (0,1), (1,1).
        let mut frame = RadarFrame {
            time: 0,
            path: "test".into(),
            tiles: vec![
                RadarTile {
                    coord: TileCoord { z: 1, x: 0, y: 0 },
                    size: 256,
                    rows: vec![],
                },
                RadarTile {
                    coord: TileCoord { z: 1, x: 1, y: 0 },
                    size: 256,
                    rows: vec![],
                },
                RadarTile {
                    coord: TileCoord { z: 1, x: 0, y: 1 },
                    size: 256,
                    rows: vec![],
                },
                RadarTile {
                    coord: TileCoord { z: 1, x: 1, y: 1 },
                    size: 256,
                    rows: vec![],
                },
            ],
            missing_tiles: 0,
            target_zoom: 1,
        };
        let full = Bounds {
            min_x: 0.0,
            max_x: 1.0,
            min_y: 0.0,
            max_y: 1.0,
        };
        assert!(frame.covers_bounds(full, 1));

        // Remove one tile → no longer covers.
        frame.tiles.pop();
        assert!(!frame.covers_bounds(full, 1));
    }

    // ── resolution_distance ────────────────────────────────────────────

    #[test]
    fn test_resolution_distance_same_is_zero() {
        assert_eq!(
            resolution_distance(BorderResolution::Low110m, BorderResolution::Low110m),
            0
        );
    }

    #[test]
    fn test_resolution_distance_increases_with_gap() {
        let d_adj = resolution_distance(BorderResolution::Low110m, BorderResolution::Medium50m);
        let d_far = resolution_distance(BorderResolution::Low110m, BorderResolution::Regional10m);
        assert!(d_far > d_adj, "farther apart → larger distance");
    }

    // ── BorderLine::is_bbox_degenerate ────────────────────────────────

    #[test]
    fn test_border_line_is_bbox_degenerate_on_default() {
        let line = BorderLine {
            kind: BorderLineKind::Country,
            points: vec![],
            bbox: Bounds::default(),
        };
        assert!(
            line.is_bbox_degenerate(),
            "zero-area default bbox is degenerate"
        );
    }

    #[test]
    fn test_border_line_is_bbox_degenerate_false_after_compute() {
        let mut line = BorderLine {
            kind: BorderLineKind::Country,
            points: vec![WorldPoint { x: 0.1, y: 0.2 }, WorldPoint { x: 0.5, y: 0.8 }],
            bbox: Bounds::default(),
        };
        line.compute_bbox();
        assert!(!line.is_bbox_degenerate());
    }

    // ── options_for_item ──────────────────────────────────────────────

    #[test]
    fn test_options_for_item_geographic_has_no_options() {
        let reg = LayerRegistry::new();
        let opts = reg.options_for_item(MainItem::Single(LayerId::MapBorders));
        assert!(opts.is_empty(), "geographic layers have no options");
    }

    #[test]
    fn test_options_for_item_radar_has_three_options() {
        let reg = LayerRegistry::new();
        let opts = reg.options_for_item(MainItem::Single(LayerId::Radar));
        assert_eq!(opts.len(), 3, "Radar exposes Braille/Color/Text options");
        let keys: Vec<&str> = opts.iter().map(|(k, _)| *k).collect();
        assert!(keys.contains(&"braille") && keys.contains(&"color") && keys.contains(&"text"));
    }

    #[test]
    fn test_options_for_item_lightning_has_four_options() {
        let reg = LayerRegistry::new();
        let opts = reg.options_for_item(MainItem::Single(LayerId::Lightning));
        assert_eq!(opts.len(), 4, "Lightning exposes Braille/Color/Text/Trail");
        assert_eq!(opts[3].0, "trail");
    }

    #[test]
    fn test_options_for_item_observation_has_one_option() {
        let reg = LayerRegistry::new();
        let opts = reg.options_for_item(MainItem::Single(LayerId::SurfTemp));
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0].0, "text");
    }

    #[test]
    fn test_options_for_item_group_has_four_children() {
        let reg = LayerRegistry::new();
        let opts = reg.options_for_item(MainItem::Group(LayerGroup::Observations));
        assert_eq!(opts.len(), 4, "Observations group has 4 children");
    }

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
        assert!(
            zooms.iter().all(|&z| z == 4),
            "stale zoom-3 tiles must be evicted"
        );
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
        let bounds = Bounds {
            min_x: 0.0,
            max_x: 1.0,
            min_y: 0.0,
            max_y: 1.0,
        };
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
            Bounds {
                min_x: 0.0,
                max_x: 0.5,
                min_y: 0.0,
                max_y: 0.5,
            },
            &mut out,
            &mut seen,
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
            Bounds {
                min_x: 0.0,
                max_x: 0.5,
                min_y: 0.0,
                max_y: 0.5,
            },
            &mut out,
            &mut seen,
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
            Bounds {
                min_x: 0.0,
                max_x: 1.0,
                min_y: 0.0,
                max_y: 1.0,
            },
            &mut out,
            &mut seen,
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
            Bounds {
                min_x: 0.15,
                max_x: 0.7,
                min_y: 0.15,
                max_y: 0.7,
            },
            &mut out,
            &mut seen,
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
            Bounds {
                min_x: 0.0,
                max_x: 0.5,
                min_y: 0.0,
                max_y: 0.5,
            },
            &mut out,
            &mut seen,
        );
        assert_eq!(out, vec![0], "first call finds line 0");

        grid.lines_for_bounds(
            Bounds {
                min_x: 0.6,
                max_x: 1.0,
                min_y: 0.6,
                max_y: 1.0,
            },
            &mut out,
            &mut seen,
        );
        assert!(
            out.is_empty(),
            "second call with disjoint bounds must return empty"
        );
    }
}
