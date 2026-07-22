use std::cell::RefCell;
use std::io;
use std::time::{Duration, Instant};

use color_eyre::eyre::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, MouseButton, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use rayon::prelude::*;

use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Terminal;

use crate::app::{
    ActiveTask, App, BorderMask, BorderMaskPoint, BorderMaskStamp, PlaybackMode, TaskState,
    VerifyTarget, LIGHTNING_IMPACT_MS,
};
use crate::cache::write_log;
use crate::geo::{
    lat_lon_to_world, tile_bounds, world_to_lat_lon, Bounds, GeoPoint, WorldPoint, CITY_MATCH_KM,
    EUROPEAN_CAPITALS, EUROPEAN_CAPITAL_NAMES, EUROPEAN_MAJOR_CITIES, OBS_TIER_ZOOM_CUTOFF,
};
use crate::keys::{self, Action, Category};
use crate::layers::{
    BorderLine, BorderLineKind, BorderResolution, LayerId, LayerOption, LayerRegistry, LayerStatus,
    MainItem, ObservationPoint, ObservationProperty, RadarFrame, RenderMode, RenderModeState, Rgb8,
};
use crate::providers::location::LocationFix;
use crate::providers::verify::VerifyOutcome;

const INTERACTION_REFRESH_DEBOUNCE_MS: u64 = 60;

/// Debounce for radar tile requests triggered by scroll-wheel zoom.
/// Batches rapid scroll events so we don't abort a useful in-flight
/// task on every tick — borders are refreshed immediately (cache hit),
/// only the network-bound radar fetch is delayed.
const ZOOM_RADAR_DEBOUNCE_MS: u64 = 80;

// ── Observation zoom-level display modes ────────────────────────────
//
// At low zoom (many countries visible) only stations near European
// capitals are shown.  As the user zooms in, major-city stations are
// added, and once a single country dominates the viewport all cached
// stations are shown.

/// Horizontal+vertical cell radius for non-capital stations.
/// Wider at low zoom to prevent "wall of values" patterns.
/// Capital-adjacent stations use a much smaller fixed radius (2) so they
/// are never pushed out by nearby non-capital stations.
fn declutter_radius(zoom: f64) -> usize {
    if zoom < 4.0 {
        6
    } else if zoom < 5.0 {
        5
    } else if zoom < 6.0 {
        4
    } else {
        3
    }
}
/// At/above this zoom every station in view is eligible; the render-side
/// declutter keeps them readable.
const ALL_OBS_ZOOM_CUTOFF: f64 = 6.5;
/// Station name labels (capital names) appear from this zoom up.
const STATION_NAMES_ZOOM: f64 = 5.5;

// Local aliases for the shared city lists from geo.rs.
const CAPITALS: &[(f64, f64)] = EUROPEAN_CAPITALS;
const MAJOR_CITIES: &[(f64, f64)] = EUROPEAN_MAJOR_CITIES;

#[derive(Clone, Copy)]
enum ObsMode {
    /// Wide view — only stations near European capitals.
    Capitals,
    /// Medium view — capitals plus major cities.
    MajorCities,
    /// Close view — all density-clipped stations.
    All,
}

fn obs_display_mode(zoom: f64) -> ObsMode {
    if zoom >= ALL_OBS_ZOOM_CUTOFF {
        ObsMode::All
    } else if zoom >= OBS_TIER_ZOOM_CUTOFF {
        ObsMode::MajorCities
    } else {
        ObsMode::Capitals
    }
}

/// `true` when a station should be shown at this zoom.
/// Capital-adjacent stations are ALWAYS shown regardless of mode so they
/// never disappear when zooming in or out.  At wider zoom levels other
/// stations are progressively hidden.
fn obs_point_visible(lat: f64, lon: f64, mode: ObsMode) -> bool {
    let cos_lat = lat.to_radians().cos();
    let threshold_sq = (CITY_MATCH_KM / 111.0).powi(2);
    // Capitals: always visible at every zoom level.
    let near_capital = CAPITALS.iter().any(|&(clat, clon)| {
        let dlat = lat - clat;
        let dlon = (lon - clon) * cos_lat;
        dlat * dlat + dlon * dlon < threshold_sq
    });
    if near_capital {
        return true;
    }
    match mode {
        ObsMode::All => true,
        ObsMode::MajorCities => MAJOR_CITIES.iter().any(|&(clat, clon)| {
            let dlat = lat - clat;
            let dlon = (lon - clon) * cos_lat;
            dlat * dlat + dlon * dlon < threshold_sq
        }),
        ObsMode::Capitals => false,
    }
}

/// `true` when `(lat, lon)` is within `CITY_MATCH_KM` of a European capital.
/// Used for the two-pass placement priority (capitals claim cells first).
fn is_capital_station(lat: f64, lon: f64) -> bool {
    let cos_lat = lat.to_radians().cos();
    let threshold_sq = (CITY_MATCH_KM / 111.0).powi(2);
    CAPITALS.iter().any(|&(clat, clon)| {
        let dlat = lat - clat;
        let dlon = (lon - clon) * cos_lat;
        dlat * dlat + dlon * dlon < threshold_sq
    })
}

/// For each European capital, find the index of the nearest observation point
/// within `CITY_MATCH_KM` and map it to the hardcoded capital name.  Only that
/// station gets a name label, and the label shows the capital name (not the
/// obscure station name returned by the API).
/// Draw each European capital's name at the city's own coordinates.
///
/// The name marks *the city*, so it is positioned from the hardcoded
/// lat/lon — never from a nearby weather station.  Stations can sit tens of km
/// away (and the upstream metadata sometimes names an Estonian station
/// "Abidjan"), so anchoring the name to one put city names in the wrong place
/// or dropped them entirely when the close station had no data.  The
/// temperature readings stay at their own stations; the two are independent.
fn raster_capital_names(cells: &mut [RasterCell], bounds: Bounds, width: u16, height: u16) {
    const NAME_COLOR: Rgb8 = Rgb8::new(105, 105, 105);
    let sub_width = u32::from(width) * 2;
    let sub_height = u32::from(height) * 4;

    for (&(clat, clon), &name) in CAPITALS.iter().zip(EUROPEAN_CAPITAL_NAMES.iter()) {
        let world = lat_lon_to_world(clat, clon);
        if world.x < bounds.min_x
            || world.x > bounds.max_x
            || world.y < bounds.min_y
            || world.y > bounds.max_y
        {
            continue;
        }
        let sx = ((world.x - bounds.min_x) / bounds.width().max(f64::EPSILON)
            * f64::from(sub_width))
        .floor()
        .clamp(0.0, f64::from(sub_width.saturating_sub(1))) as u32;
        let sy = ((world.y - bounds.min_y) / bounds.height().max(f64::EPSILON)
            * f64::from(sub_height))
        .floor()
        .clamp(0.0, f64::from(sub_height.saturating_sub(1))) as u32;

        // One row below the city so the name never covers the marker or a
        // reading sitting on the city itself.
        let name_sy = (sy / 4 + 1) * 4;
        let name_cell_x = (sx / 2) as usize;
        let name_cell_y = (name_sy / 4) as usize;
        if name_cell_y >= usize::from(height) {
            continue;
        }
        let row_base = name_cell_y * usize::from(width);
        let end = (name_cell_x + name.len()).min(usize::from(width));
        let cells_free = (name_cell_x..end)
            .all(|cx| cells.get(row_base + cx).is_some_and(|c| c.glyph.is_none()));
        if cells_free {
            write_obs_str(cells, width, sx, name_sy, name, NAME_COLOR, false);
        }
    }
}

/// Show station name labels (capitals only) when sufficiently zoomed in.
fn show_station_names(zoom: f64) -> bool {
    zoom >= STATION_NAMES_ZOOM
}

/// How often the (locally computed) radar frame list is re-checked for
/// a newly opened 5-minute slot.
const RADAR_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Minimum time between observation / warning refresh attempts.
/// Matches the providers' cache TTLs so polling faster would only burn
/// requests on data that hasn't changed.
const DATA_REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// Minimum time between state.toml writes (state is also saved on exit).
const STATE_SAVE_DEBOUNCE: Duration = Duration::from_secs(2);

/// Ceiling on redraw rate.  Background results (notably lightning, which
/// streams strikes continuously) each mark the UI dirty, so without a cap
/// a busy feed would drive a full re-raster per arrival.  Coalescing to
/// 30 fps bounds that cost while staying well above what reads as smooth.
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// Redraw cadence for fast animations (spinners, lightning impact flash).
const ANIM_FAST: Duration = Duration::from_millis(50);

/// How long one indeterminate-task marquee sweep takes to bounce end to end.
const MARQUEE_PERIOD: Duration = Duration::from_millis(1200);

/// Redraw cadence for slow animations (lightning trails fading over minutes).
const ANIM_SLOW: Duration = Duration::from_millis(250);

/// Longest the event loop will block when nothing is animating.  Bounds how
/// long a background result waits to be drained; input wakes `poll` at once.
const IDLE_POLL: Duration = Duration::from_millis(100);

pub async fn run(mut app: App) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    // Enable keyboard enhancement where supported (iTerm2, WezTerm, Kitty, …)
    // so Alt+arrow keys are decoded as Alt+Left/Right rather than ESC sequences.
    // On unsupported terminals (Terminal.app) we fall back to Alt+b/Alt+f.
    let kbd_enhanced = supports_keyboard_enhancement().unwrap_or(false);
    if kbd_enhanced {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut app).await;

    if kbd_enhanced {
        execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags)?;
    }
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    result
}

/// A time-based visual that needs periodic redrawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Anim {
    /// How often it must be redrawn to look smooth.
    interval: Duration,
    /// Whether the *map* changes with time, or only the surrounding chrome
    /// (header, layer list, task overlay).  Chrome-only animations reuse the
    /// cached raster, which is what makes them nearly free.
    needs_raster: bool,
}

/// What is currently animating and how often it must redraw, or `None` when
/// the display is fully static and no redraw is needed at all.
///
/// Only genuinely time-based visuals belong here.  Everything driven by data
/// or input already sets `dirty` at its source.
fn animation_interval(app: &App) -> Option<Anim> {
    // Strike impact flash — brief, fast, and drawn on the map.
    if app.has_lightning_impact() {
        return Some(Anim {
            interval: ANIM_FAST,
            needs_raster: true,
        });
    }
    // Trails fade over `lightning_trail_minutes`; a few frames a second is
    // far more than enough to make a minutes-long fade look smooth.
    if !app.lightning_strikes.is_empty() {
        return Some(Anim {
            interval: ANIM_SLOW,
            needs_raster: true,
        });
    }
    // Spinners and progress bars live in the chrome.  The map data they are
    // waiting on arrives through the drains, which mark the map dirty itself.
    if app.layers.any_loading() || !app.active_tasks.is_empty() {
        return Some(Anim {
            interval: ANIM_FAST,
            needs_raster: false,
        });
    }
    // The live indicator pulses on a 2 s cosine, and the timeline bar fills
    // in as frames cache.  Both are header-only.
    if app.playback_mode == PlaybackMode::Live {
        return Some(Anim {
            interval: ANIM_FAST,
            needs_raster: false,
        });
    }
    None
}

/// How long to block waiting for input.
///
/// This is the loop's idle cost: `event::poll` returns the instant input
/// arrives, so a longer timeout never adds input latency — it only bounds
/// how long a background result can sit in its channel before being
/// drained.  When nothing is animating the loop idles at `IDLE_POLL`
/// instead of spinning at 60 Hz.
fn poll_timeout(
    app: &App,
    dirty: bool,
    last_render: Instant,
    last_playback_step: Instant,
    next_interaction_refresh: Option<Instant>,
    next_zoom_refresh: Option<Instant>,
) -> Duration {
    // A pending redraw was held back by the frame cap — wait out the
    // remainder and nothing longer.
    if dirty {
        return MIN_FRAME_INTERVAL.saturating_sub(last_render.elapsed());
    }

    let mut timeout = IDLE_POLL;

    if let Some(anim) = animation_interval(app) {
        timeout = timeout.min(anim.interval.saturating_sub(last_render.elapsed()));
    }
    if app.playback_mode == PlaybackMode::Playing && !app.timestamps.is_empty() {
        let base = Duration::from_millis(app.playback_speed.interval_ms());
        let interval = if app.frame_index == 0 { base * 3 } else { base };
        timeout = timeout.min(interval.saturating_sub(last_playback_step.elapsed()));
    }
    let now = Instant::now();
    for deadline in [next_interaction_refresh, next_zoom_refresh]
        .into_iter()
        .flatten()
    {
        timeout = timeout.min(deadline.saturating_duration_since(now));
    }
    timeout
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut last_mouse: Option<(u16, u16)> = None;
    let mut dirty = true;
    let mut pending_interaction_refresh = false;
    let mut next_interaction_refresh: Option<Instant> = None;
    let mut pending_zoom_refresh = false;
    let mut next_zoom_refresh: Option<Instant> = None;
    let mut last_render = Instant::now();
    let mut last_radar_poll = Instant::now();
    let mut state_dirty = false;
    let mut last_state_save = Instant::now();
    let mut last_playback_step = Instant::now();
    loop {
        // Any of these can change what the map shows, so a redraw they cause
        // must re-rasterise rather than reuse the cached frame.
        let drained = app.drain_refresh_results()
            | app.drain_preload_results()
            | app.drain_frame_list()
            | app.drain_task_messages()
            | app.drain_obs_results()
            | app.drain_warning_results()
            | app.drain_verify_results()
            | app.drain_lightning_results()
            | app.drain_location_updates()
            | app.drain_search_results()
            | app.drain_pin_labels();
        if drained {
            dirty = true;
        }
        // Re-request radar slots whose backoff has expired, so a frame that
        // failed to load recovers on its own instead of waiting for the user
        // to pan or zoom.
        app.retry_due_frames();
        if app.pending_warning_refresh {
            app.pending_warning_refresh = false;
            app.warn_last_attempt = None; // force an immediate fetch
        }
        // Periodic radar refresh: every minute, check (locally) whether
        // a new 5-minute slot has opened and reload the displayed frame
        // if so.  The provider's probe cache keeps the network cost to
        // at most one HEAD per new slot.
        if last_radar_poll.elapsed() >= RADAR_POLL_INTERVAL {
            last_radar_poll = Instant::now();
            if app.poll_radar_timestamps() {
                app.request_meteogate_refresh(app.map_width, app.map_height);
                dirty = true;
            }
        }
        // Playback: when Playing, advance one frame per speed interval.
        // Hold 3x longer on the live (newest) frame before looping back to oldest.
        if app.playback_mode == PlaybackMode::Playing && !app.timestamps.is_empty() {
            let base = Duration::from_millis(app.playback_speed.interval_ms());
            let interval = if app.frame_index == 0 { base * 3 } else { base };
            if last_playback_step.elapsed() >= interval {
                last_playback_step = Instant::now();
                app.playback_step();
                app.request_meteogate_refresh(app.map_width, app.map_height);
                dirty = true;
            }
        }
        // Staleness: re-fetch observations and warnings while their
        // layers are enabled.  Based on the last *attempt* so a failing
        // endpoint is retried once per interval, not once per tick.
        {
            let obs_stale = app
                .obs_last_attempt
                .is_none_or(|t| t.elapsed() >= DATA_REFRESH_INTERVAL);
            let obs_enabled = app.any_obs_enabled();
            if obs_stale && obs_enabled && !app.has_obs_task() {
                app.request_obs_refresh();
                dirty = true;
            }
            let warn_stale = app
                .warn_last_attempt
                .is_none_or(|t| t.elapsed() >= DATA_REFRESH_INTERVAL);
            if warn_stale && app.layers.enabled(LayerId::MeteoAlarm) {
                app.request_warning_refresh();
                dirty = true;
            }
        }
        // Persist viewport / layer state at most every couple of
        // seconds — mouse drags fire dozens of events per second and
        // each save is a file write.
        if state_dirty && last_state_save.elapsed() >= STATE_SAVE_DEBOUNCE {
            app.save_state();
            state_dirty = false;
            last_state_save = Instant::now();
        }
        if pending_interaction_refresh
            && next_interaction_refresh.is_some_and(|deadline| Instant::now() >= deadline)
        {
            let area = terminal.size()?;
            let map_area = map_rect(Rect::new(0, 0, area.width, area.height));
            app.map_width = map_area.width;
            app.map_height = map_area.height;
            app.request_meteogate_refresh(map_area.width, map_area.height);
            app.request_border_refresh();
            if app.any_obs_enabled() {
                app.request_obs_refresh();
            }
            pending_interaction_refresh = false;
            next_interaction_refresh = None;
            dirty = true;
        }
        // Zoom debounce: radar tiles are requested once the user pauses
        // scrolling instead of on every tick, avoiding repeated task
        // aborts when scrolling through multiple zoom levels quickly.
        // Border data was already requested immediately in the scroll handler.
        if pending_zoom_refresh
            && next_zoom_refresh.is_some_and(|deadline| Instant::now() >= deadline)
        {
            let area = terminal.size()?;
            let map_area = map_rect(Rect::new(0, 0, area.width, area.height));
            app.map_width = map_area.width;
            app.map_height = map_area.height;
            app.request_meteogate_refresh(map_area.width, map_area.height);
            if app.any_obs_enabled() {
                app.request_obs_refresh();
            }
            pending_zoom_refresh = false;
            next_zoom_refresh = None;
            dirty = true;
        }
        if dirty && last_render.elapsed() >= MIN_FRAME_INTERVAL {
            // State changed — always re-rasterise.
            terminal.draw(|frame| render(frame, app, false))?;
            dirty = false;
            last_render = Instant::now();
        } else if !dirty {
            // Nothing changed, but a time-based visual may be due a frame.
            // A chrome-only animation redraws over the cached map raster,
            // which is what keeps the live pulse essentially free.
            if let Some(anim) = animation_interval(app) {
                if last_render.elapsed() >= anim.interval {
                    terminal.draw(|frame| render(frame, app, !anim.needs_raster))?;
                    last_render = Instant::now();
                }
            }
        }
        if !event::poll(poll_timeout(
            app,
            dirty,
            last_render,
            last_playback_step,
            next_interaction_refresh,
            next_zoom_refresh,
        ))? {
            continue;
        }
        let area = terminal.size()?;
        let terminal_area = Rect::new(0, 0, area.width, area.height);
        let map_area = map_rect(terminal_area);
        app.map_width = map_area.width;
        app.map_height = map_area.height;
        let mut refresh = false;
        let mut quit = false;
        // Coalesce a burst of queued input events into a single render.
        // A fast drag or scroll delivers many events per frame; processing
        // them all before drawing (instead of one render per event) keeps
        // interaction smooth without dropping any input — every pan delta
        // and zoom step is still applied, just rendered once.
        const MAX_EVENT_BATCH: u32 = 64;
        let mut events_processed = 0u32;
        loop {
            match event::read()? {
                // While the search prompt is open it owns the keyboard: every
                // printable key is query text, so this must run before
                // `keys::resolve` or typing "quit" would quit.
                Event::Key(key) if key.kind == KeyEventKind::Press && app.search_is_open() => {
                    match key.code {
                        KeyCode::Esc => app.cancel_search(),
                        KeyCode::Enter => app.submit_search(),
                        KeyCode::Backspace => app.search_backspace(),
                        // Ctrl-C must still quit even mid-query.
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            app.shutdown();
                            quit = true;
                            break;
                        }
                        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            app.search_push_char(c);
                        }
                        _ => {}
                    }
                    dirty = true;
                }
                // While the settings modal is open it owns the keyboard, same
                // rationale as the search prompt above: printable keys edit
                // the focused secret, so this must run before `keys::resolve`.
                Event::Key(key) if key.kind == KeyEventKind::Press && app.settings_is_open() => {
                    if let Some(action) = keys::settings_key_action(key) {
                        match action {
                            keys::SettingsKeyAction::Quit => {
                                app.shutdown();
                                quit = true;
                                break;
                            }
                            keys::SettingsKeyAction::FocusPrev => app.settings_focus_prev(),
                            keys::SettingsKeyAction::FocusNext => app.settings_focus_next(),
                            keys::SettingsKeyAction::ToggleBool => app.settings_toggle_bool(),
                            keys::SettingsKeyAction::Confirm => app.settings_confirm(),
                            keys::SettingsKeyAction::Back => app.settings_back(),
                            keys::SettingsKeyAction::PushChar(c) => app.settings_push_char(c),
                            keys::SettingsKeyAction::Backspace => app.settings_backspace(),
                        }
                    }
                    dirty = true;
                }
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // Set a render mode on the selected layer, if it accepts one.
                    let set_mode = |app: &mut App, mode: RenderMode, refresh: &mut bool| {
                        app.layer_panel_focused = true;
                        let id = app.layers.selected_layer();
                        let allowed = id.is_rendered()
                            && (mode == RenderMode::Text || !id.is_observation())
                            // The marker is one cell: braille would be a
                            // single dot, so it offers text and background only.
                            && (mode != RenderMode::Braille || !id.is_location());
                        if allowed {
                            app.layers.mode_state_mut().toggle(mode, id);
                            handle_layer_enable(app, id, refresh);
                            return true;
                        }
                        false
                    };
                    // An unbound key falls through to the batch bookkeeping
                    // below: skipping it would block on the next `read` with
                    // the rest of the batch still unrendered.
                    if let Some(action) = keys::resolve(key) {
                        match action {
                            Action::Quit => {
                                if app.show_help {
                                    app.show_help = false;
                                    dirty = true;
                                } else {
                                    app.shutdown();
                                    quit = true;
                                    break;
                                }
                            }
                            Action::ToggleHelp => {
                                app.show_help = !app.show_help;
                                dirty = true;
                            }
                            Action::ToggleLegend => {
                                app.show_legend = !app.show_legend;
                                dirty = true;
                            }
                            Action::ToggleLayer => {
                                app.layer_panel_focused = true;
                                if let Some(id) = app.layers.activate_selected() {
                                    handle_layer_enable(app, id, &mut refresh);
                                }
                                dirty = true;
                            }
                            Action::ModeBraille => {
                                dirty |= set_mode(app, RenderMode::Braille, &mut refresh)
                            }
                            Action::ModeColor => {
                                dirty |= set_mode(app, RenderMode::Color, &mut refresh)
                            }
                            Action::ModeText => {
                                dirty |= set_mode(app, RenderMode::Text, &mut refresh)
                            }
                            Action::SelectPrevious => {
                                app.layer_panel_focused = true;
                                app.layers.select_previous();
                                dirty = true;
                            }
                            Action::SelectNext => {
                                app.layer_panel_focused = true;
                                app.layers.select_next();
                                dirty = true;
                            }
                            Action::EnterGroup => {
                                app.layer_panel_focused = true;
                                dirty |= app.layers.enter_options();
                            }
                            // Exit options → defocus the root list → refocus.
                            Action::ExitGroup => {
                                if !app.layer_panel_focused {
                                    app.layer_panel_focused = true;
                                    dirty = true;
                                } else if app.layers.is_in_options() {
                                    dirty |= app.layers.exit_options();
                                } else {
                                    app.layer_panel_focused = false;
                                    dirty = true;
                                }
                            }
                            Action::RefetchMap => {
                                app.request_border_refetch();
                                dirty = true;
                            }
                            Action::OpenSearch => {
                                app.open_search();
                                dirty = true;
                            }
                            Action::OpenSettings => {
                                if !app.search_is_open() && !app.show_help {
                                    app.open_settings();
                                    dirty = true;
                                }
                            }
                            // Zoom / pan — defocus the panel so the map fills the screen.
                            Action::ZoomIn | Action::ZoomOut => {
                                app.layer_panel_focused = false;
                                let delta = if action == Action::ZoomIn {
                                    0.25
                                } else {
                                    -0.25
                                };
                                app.viewport.zoom_by(delta);
                                refresh = true;
                                dirty = true;
                            }
                            Action::PanLeft
                            | Action::PanRight
                            | Action::PanUp
                            | Action::PanDown => {
                                app.layer_panel_focused = false;
                                let (dx, dy) = match action {
                                    Action::PanLeft => (-1.0, 0.0),
                                    Action::PanRight => (1.0, 0.0),
                                    Action::PanUp => (0.0, -1.0),
                                    _ => (0.0, 1.0),
                                };
                                app.viewport.pan(dx, dy);
                                refresh = true;
                                dirty = true;
                            }
                            Action::FrameBack => {
                                app.previous_frame();
                                refresh = true;
                                dirty = true;
                            }
                            Action::FrameForward => {
                                app.next_frame();
                                refresh = true;
                                dirty = true;
                            }
                            Action::TogglePlayback => {
                                app.toggle_play_pause();
                                last_playback_step = Instant::now();
                                dirty = true;
                            }
                            Action::JumpToLive => {
                                app.jump_to_live();
                                app.request_meteogate_refresh(app.map_width, app.map_height);
                                dirty = true;
                            }
                            Action::SpeedFaster => {
                                app.speed_faster();
                                dirty = true;
                            }
                            Action::SpeedSlower => {
                                app.speed_slower();
                                dirty = true;
                            }
                            Action::CycleHistory => {
                                app.cycle_history();
                                refresh = true;
                                dirty = true;
                            }
                        }
                    }
                }
                // While the settings modal is open it owns the mouse: no map
                // pan / zoom / drag. Links get hover + pressed feedback and open
                // in the platform browser on a click that lands on them.
                Event::Mouse(mouse) if app.settings_is_open() => {
                    let hit = app.settings_link_index_at(mouse.column, mouse.row);
                    match mouse.kind {
                        MouseEventKind::Moved | MouseEventKind::Drag(MouseButton::Left) => {
                            if app.settings_link_hover != hit {
                                app.settings_link_hover = hit;
                                dirty = true;
                            }
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            app.settings_link_hover = hit;
                            app.settings_link_pressed = hit;
                            dirty = true;
                        }
                        MouseEventKind::Up(MouseButton::Left) => {
                            // A click counts only when release lands on the same
                            // link the press started on.
                            let pressed = app.settings_link_pressed.take();
                            if let (Some(p), Some(h)) = (pressed, hit) {
                                if p == h {
                                    if let Some(&(_, _, _, url)) = app.settings_links.get(h) {
                                        open_url(url);
                                    }
                                }
                            }
                            app.settings_link_hover = hit;
                            dirty = true;
                        }
                        _ => {}
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        app.layer_panel_focused = false;
                        let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
                        let delta = if shift { 0.10 } else { 0.25 };
                        if let Some((column, row)) =
                            relative_mouse(map_area, mouse.column, mouse.row)
                        {
                            app.viewport.zoom_around_screen(
                                map_area.width,
                                map_area.height,
                                column,
                                row,
                                delta,
                            );
                        } else {
                            app.viewport.zoom_by(delta);
                        }
                        // Borders are cache-hit-fast; radar is debounced to avoid
                        // aborting in-flight tile tasks on every scroll tick.
                        app.request_border_refresh();
                        pending_zoom_refresh = true;
                        next_zoom_refresh =
                            Some(Instant::now() + Duration::from_millis(ZOOM_RADAR_DEBOUNCE_MS));
                        dirty = true;
                    }
                    MouseEventKind::ScrollDown => {
                        app.layer_panel_focused = false;
                        let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
                        let delta = if shift { -0.10 } else { -0.25 };
                        if let Some((column, row)) =
                            relative_mouse(map_area, mouse.column, mouse.row)
                        {
                            app.viewport.zoom_around_screen(
                                map_area.width,
                                map_area.height,
                                column,
                                row,
                                delta,
                            );
                        } else {
                            app.viewport.zoom_by(delta);
                        }
                        app.request_border_refresh();
                        pending_zoom_refresh = true;
                        next_zoom_refresh =
                            Some(Instant::now() + Duration::from_millis(ZOOM_RADAR_DEBOUNCE_MS));
                        dirty = true;
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        if contains(map_area, mouse.column, mouse.row) {
                            last_mouse = Some((mouse.column, mouse.row));
                            app.is_dragging = true;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        app.layer_panel_focused = false;
                        if let Some((last_col, last_row)) = last_mouse {
                            let dx = last_col as f64 - mouse.column as f64;
                            let dy = last_row as f64 - mouse.row as f64;
                            app.viewport
                                .pan_screen_delta(map_area.width, map_area.height, dx, dy);
                            // Live-load radar tiles as you drag: the guards inside
                            // request_meteogate_refresh skip the call when coverage
                            // is still adequate or a task is already in-flight, so
                            // this is cheap during small pans and only spawns a new
                            // task when you've panned outside the pre-fetched buffer.
                            app.request_meteogate_refresh(map_area.width, map_area.height);
                            pending_interaction_refresh = true;
                            next_interaction_refresh = Some(
                                Instant::now()
                                    + Duration::from_millis(INTERACTION_REFRESH_DEBOUNCE_MS),
                            );
                            dirty = true;
                        }
                        last_mouse = Some((mouse.column, mouse.row));
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        last_mouse = None;
                        app.is_dragging = false;
                        dirty = true;
                        if pending_interaction_refresh {
                            refresh = true;
                            pending_interaction_refresh = false;
                            next_interaction_refresh = None;
                        }
                    }
                    _ => {}
                },
                Event::Resize(width, height) => {
                    let map_area = map_rect(Rect::new(0, 0, width, height));
                    app.request_meteogate_refresh(map_area.width, map_area.height);
                    app.request_border_refresh();
                    if (app.any_obs_enabled()) && !app.has_obs_task() {
                        app.request_obs_refresh();
                    }
                    dirty = true;
                }
                _ => {}
            }
            events_processed += 1;
            // Keep draining while more input is already queued, up to a
            // bounded batch so a continuous drag can't starve the render.
            if quit || events_processed >= MAX_EVENT_BATCH || !event::poll(Duration::ZERO)? {
                break;
            }
        }
        if quit {
            break;
        }
        state_dirty = true;
        if refresh {
            app.request_viewport_refresh();
            dirty = true;
        }
    }
    app.save_state();
    Ok(())
}

/// Draw a frame.  `reuse_raster` skips re-rasterising the map and redraws
/// the cached cell grid instead — valid only when the caller knows nothing
/// affecting the map has changed since the last frame.
fn render(frame: &mut ratatui::Frame<'_>, app: &mut App, reuse_raster: bool) {
    let chunks = app_areas(frame.area());

    render_header(frame, chunks[0], app);
    render_map(frame, chunks[1], app, reuse_raster);
    // The search prompt takes over the footer line while it is open or has
    // something to report, like a shell's `/` prompt.
    if app.search_is_open() || app.search_status.is_some() {
        render_search_prompt(frame, chunks[2], app);
    } else {
        render_footer(frame, chunks[2], app);
    }

    if app.show_help {
        render_help(frame, frame.area());
    }
    if app.settings.is_some() {
        render_settings(frame, frame.area(), app);
    }
}

/// A keystroke, as shown in the footer and help.
///
/// Emphasis is carried by weight and colour rather than a background badge: a
/// `bg(DarkGray)` chip lands too close to the surrounding background on most
/// terminal themes to read as a key at all.  Bold on the default foreground
/// inherits whatever contrast the user's theme already guarantees.
fn key_span(key: &str) -> Span<'static> {
    Span::styled(
        key.to_string(),
        Style::default().add_modifier(Modifier::BOLD),
    )
}

/// The description paired with a [`key_span`], set back so the keys lead.
fn desc_span(desc: &str) -> Span<'static> {
    Span::styled(
        desc.to_string(),
        Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::ITALIC),
    )
}

/// Build the help modal's lines from the keybinding registry.
///
/// Every row comes from [`keys::BINDINGS`], so a binding added there appears
/// here automatically.  The key and name columns are each padded to their
/// widest entry, so all three columns line up across all four sections.
fn help_lines() -> Vec<TextLine<'static>> {
    let rows = || keys::BINDINGS.iter().filter(|b| b.help_keys.is_some());
    let key_w = rows()
        .filter_map(|b| b.help_keys)
        .map(|k| k.chars().count())
        .max()
        .unwrap_or(0);
    let name_w = rows().map(|b| b.name.chars().count()).max().unwrap_or(0);

    let mut lines = vec![
        TextLine::from(Span::styled(
            format!("  front {}", env!("CARGO_PKG_VERSION")),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        TextLine::from(Span::styled(
            "  Fancy Radar ObservatioN Tool — European weather radar in your terminal",
            Style::default().fg(Color::Gray),
        )),
    ];

    for category in Category::ORDER {
        lines.push(TextLine::from(""));
        lines.push(TextLine::from(Span::styled(
            format!("  {}", category.title().to_uppercase()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for (k, name, desc) in keys::help_rows(category) {
            lines.push(TextLine::from(vec![
                Span::raw("    "),
                key_span(k),
                Span::raw(" ".repeat(key_w - k.chars().count() + 2)),
                Span::raw(name.to_string()),
                Span::raw(" ".repeat(name_w - name.chars().count() + 2)),
                Span::styled(desc.to_string(), Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    lines.push(TextLine::from(""));
    lines.push(TextLine::from(Span::styled(
        "  ? or esc to close",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )));
    lines
}

fn render_help(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let content = help_lines();

    // `Clear` resets the cells the modal covers, so the block needs no fill of
    // its own — it inherits the terminal's own background, whatever that is.
    let block = Block::default()
        .title(" Help ")
        .title_alignment(ratatui::layout::Alignment::Center)
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Color::Cyan);

    // Size to the content and centre it, rather than covering the whole map.
    // Two columns of padding on the right keep descriptions off the border.
    let content_w = content
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.chars().count()).sum())
        .max()
        .unwrap_or(0) as u16;
    let panel = centered_rect(content_w + 4, content.len() as u16 + 2, area);

    let inner = block.inner(panel);
    frame.render_widget(Clear, panel);
    frame.render_widget(block, panel);
    frame.render_widget(Paragraph::new(content), inner);
}

/// Fixed inner width of the settings modal — the panel never resizes as you
/// browse or type, so the layout stays put. Sized to fit the widest footer.
const SETTINGS_INNER_WIDTH: u16 = 58;

/// Right-align the last `width` chars of `s` — a scrolling-textbox view that
/// keeps the tail (and, while editing, the cursor) visible for a long value.
fn value_window(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count <= width {
        s.to_string()
    } else {
        s.chars().skip(count - width).collect()
    }
}

/// The status icon shown at the right of a field: grayed when there's no value
/// to check, dim `?` when a value is present but unverified, red `✗` invalid,
/// yellow `?` unreachable, green `✓` verified-working. Non-secret fields (no
/// validation) get a blank.
fn settings_status_icon(
    field: &crate::settings::Field,
    last_verify: Option<(VerifyTarget, VerifyOutcome)>,
) -> (&'static str, Color) {
    if field.kind != crate::settings::FieldKind::Secret {
        return (" ", Color::Reset);
    }
    if field.is_secret_empty() {
        return ("–", Color::DarkGray);
    }
    match last_verify {
        Some((VerifyTarget::Eumetnet, VerifyOutcome::Valid)) => ("✓", Color::Green),
        Some((VerifyTarget::Eumetnet, VerifyOutcome::Invalid)) => ("✗", Color::Red),
        Some((VerifyTarget::Eumetnet, VerifyOutcome::Unreachable)) => ("?", Color::Yellow),
        _ => ("?", Color::DarkGray),
    }
}

/// The settings modal: reuses `render_help`'s Clear + rounded-border Block +
/// Paragraph pattern at a FIXED width so it never reshapes. Fields are browsed
/// with the arrows and edited in place (Enter to edit, Enter to save). The
/// focused secret is revealed in a fixed-width scrolling box; a status icon on
/// the right reflects verification. Provider URLs render as underlined "link"
/// text; their screen rects are recorded on `app.settings_links` so the event
/// loop can open the browser on a click (native OSC 8 links corrupt the cell
/// grid, so we handle the click ourselves).
fn render_settings(frame: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    let Some(state) = &app.settings else {
        app.settings_links.clear();
        app.settings_link_hover = None;
        app.settings_link_pressed = None;
        return;
    };

    let inner_w = SETTINGS_INNER_WIDTH;
    // Fixed columns: marker(2) + label + value box + space + icon(1) == inner_w.
    let label_w = state
        .model
        .fields
        .iter()
        .map(|f| f.label.chars().count())
        .max()
        .unwrap_or(0)
        + 2;
    let val_w = (inner_w as usize).saturating_sub(2 + label_w + 2);
    let link_indent = 4u16;

    let mut lines: Vec<TextLine> = Vec::new();
    let mut link_rows: Vec<(usize, &'static str)> = Vec::new();
    lines.push(TextLine::from(""));

    for (i, field) in state.model.fields.iter().enumerate() {
        let focused = i == state.model.focus;
        let editing = focused && state.editing;
        let marker = if focused { "› " } else { "  " };
        let label_style = if focused {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        // Value in a fixed-width, tail-scrolling box (focused secret revealed;
        // a cursor is appended while editing).
        let mut raw = field.display(focused);
        if editing {
            raw.push('▏');
        }
        let value = format!("{:<val_w$}", value_window(&raw, val_w));
        let value_style = if editing {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };

        let (icon, icon_color) = settings_status_icon(field, app.last_verify);

        lines.push(TextLine::from(vec![
            Span::raw(marker),
            Span::styled(format!("{:<label_w$}", field.label), label_style),
            Span::styled(value, value_style),
            Span::raw(" "),
            Span::styled(icon, Style::default().fg(icon_color)),
        ]));

        // Sub-line: a clickable provider link under a secret, a plain note
        // under the restart-only bool. Keeps rows uniform so the modal's shape
        // is constant.
        if let Some(url) = field.help_url {
            let idx = link_rows.len();
            let link_style = if app.settings_link_pressed == Some(idx) {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else if app.settings_link_hover == Some(idx) {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::UNDERLINED)
            };
            link_rows.push((lines.len(), url));
            lines.push(TextLine::from(vec![
                Span::raw(" ".repeat(link_indent as usize)),
                Span::styled(url, link_style),
            ]));
        } else if field.key == "location.ip_fallback" {
            lines.push(TextLine::from(Span::styled(
                "    applies on next launch",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    if let Some(err) = &state.apply_error {
        lines.push(TextLine::from(""));
        lines.push(TextLine::from(Span::styled(
            format!("  {err}"),
            Style::default().fg(Color::Red),
        )));
    }

    let legend = if state.editing {
        match state.model.focused().kind {
            crate::settings::FieldKind::Bool => "←→ toggle   enter save   esc cancel",
            crate::settings::FieldKind::Secret => "type to edit   enter save   esc cancel",
        }
    } else {
        "↑↓ move   enter edit   esc close   (click link to open)"
    };
    lines.push(TextLine::from(""));
    lines.push(TextLine::from(Span::styled(
        format!("  {legend}"),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )));

    let block = Block::default()
        .title(" Settings ")
        .title_alignment(ratatui::layout::Alignment::Center)
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Color::Cyan);

    let panel = centered_rect(inner_w + 2, lines.len() as u16 + 2, area);
    let inner = block.inner(panel);

    // Record link screen rects before the `state` borrow is released so the
    // click handler can hit-test them. `link_rows` holds owned data only.
    let link_screen_rects: Vec<(u16, u16, u16, &'static str)> = link_rows
        .iter()
        .filter_map(|&(row, url)| {
            let y = inner.y + row as u16;
            (y < inner.y.saturating_add(inner.height)).then_some((
                inner.x + link_indent,
                y,
                url.chars().count() as u16,
                url,
            ))
        })
        .collect();

    frame.render_widget(Clear, panel);
    frame.render_widget(block, panel);
    frame.render_widget(Paragraph::new(lines), inner);

    app.settings_links = link_screen_rects;
}

/// Open `url` in the platform's default browser, best-effort and detached.
///
/// `url` is always a hardcoded provider constant (never user input), so there
/// is no injection surface. A launch failure is ignored — the worst case is
/// nothing happens, which must never take down the TUI.
fn open_url(url: &str) {
    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        // `start` is a cmd builtin; the empty "" is the window title arg.
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    {
        use std::process::Stdio;
        let _ = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let _ = url;
}

/// Centre a `width` × `height` rect inside `area`, shrinking to fit when the
/// terminal is too small to hold it.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn app_areas(area: Rect) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area)
}

fn map_rect(area: Rect) -> Rect {
    app_areas(area)[1]
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn relative_mouse(area: Rect, column: u16, row: u16) -> Option<(u16, u16)> {
    contains(area, column, row).then(|| (column - area.x, row - area.y))
}

fn render_header(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let version = env!("CARGO_PKG_VERSION");
    // "FRONT" bold white, version dimmed — version is secondary info.
    let title = TextLine::from(vec![
        Span::styled("FRONT", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(format!(" v{version}"), Style::default().fg(Color::DarkGray)),
    ]);
    let title_width = (5 + 1 + 1 + version.len()) as u16; // "FRONT v" + version

    if area.width <= title_width + 16 {
        frame.render_widget(Paragraph::new(title), area);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(title_width), Constraint::Fill(1)])
        .split(area);

    let timeline = timeline_line(app, chunks[1].width);

    frame.render_widget(Paragraph::new(title), chunks[0]);
    frame.render_widget(
        Paragraph::new(timeline).alignment(ratatui::layout::Alignment::Right),
        chunks[1],
    );
}

/// Narrowest useful timeline bar; below this the bar is dropped entirely
/// rather than rendered as a stub.
const TIMELINE_BAR_MIN: usize = 12;

/// Widest the bar is allowed to grow.  Deeper windows map several slots per
/// cell instead of stretching: 288 cells (24 h) would not fit any header.
const TIMELINE_BAR_MAX: usize = 48;

/// Width of the timeline bar in cells, or `None` when there isn't room.
///
/// Sized from the *nominal* slot count for the selected history depth, never
/// from how many frames happen to be loaded — that quantity changes as the
/// list arrives and as unpublished slots are pruned, which is what made the
/// bar collapse mid-session.  For a given depth and terminal width the result
/// is constant.
///
/// Each cell carries two slots, since [`timeline_bar_spans`] splits it into
/// half-cells: asking for one cell per slot would leave every second half-cell
/// empty and render the bar as a dither of track and data.
fn timeline_bar_width(hours: u8, space: usize) -> Option<usize> {
    if space < TIMELINE_BAR_MIN {
        return None;
    }
    let cells = crate::providers::meteogate::frames_for_hours(hours).div_ceil(2);
    Some(cells.clamp(TIMELINE_BAR_MIN, TIMELINE_BAR_MAX).min(space))
}

/// Build the compact timeline right-aligned in the header.
///
/// Format: `● 13:50 ···░░░█░░`  (icon · sp · time · sp · fixed-width frame bar)
/// When playing: speed label appended after the bar (`▶ 13:50 ···░░░█░░ 2×`).
fn timeline_line(app: &App, avail: u16) -> TextLine<'static> {
    let frame_count = app.timestamps.len();

    let time_str = if app.radar_frame.is_some() {
        app.frame_label()
    } else {
        "--:--".to_string()
    };

    // Live icon pulses smoothly using a cosine wave over a 2-second period.
    // Other modes use flat colours.
    let (icon, accent) = match app.playback_mode {
        PlaybackMode::Live => {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as f64;
            let t = 0.5 - 0.5 * (std::f64::consts::TAU * ms / 2000.0).cos();
            let g = (80.0 + 175.0 * t).round() as u8;
            ('●', Color::Rgb(0, g, 0))
        }
        PlaybackMode::Paused => ('‖', Color::Yellow),
        PlaybackMode::Playing => ('▶', Color::Cyan),
    };
    let time_color = Color::Reset;

    // Speed label sits left of the icon when playing; absent otherwise.
    let speed_prefix: Option<&'static str> = if app.playback_mode == PlaybackMode::Playing {
        Some(app.playback_speed.label())
    } else {
        None
    };

    // Minimum: [speed ]icon sp time  (7 or 10 chars)
    let min_w = 1 + 1 + 5 + speed_prefix.map_or(0, |s| s.len() as u16 + 1);
    if avail < min_w {
        return TextLine::from(Span::styled(icon.to_string(), Style::default().fg(accent)));
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some(spd) = speed_prefix {
        spans.push(Span::styled(
            spd.to_string(),
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(icon.to_string(), Style::default().fg(accent)));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(time_str, Style::default().fg(time_color)));

    // Frames are mapped onto the bar. It is drawn even with none loaded yet, so
    // the header width doesn't jump once the frame list arrives.  The depth
    // label trails it so `i` has visible feedback.
    let hours_label = format!("{}h", app.history_hours);
    let trailer = hours_label.len() as u16 + 1; // label + its leading space
    let space = avail.saturating_sub(min_w + 1 + trailer) as usize;
    if let Some(bar_width) = timeline_bar_width(app.history_hours, space) {
        let states: Vec<SlotState> = app
            .timestamps
            .iter()
            .map(|ts| app.slot_state(*ts))
            .collect();
        spans.push(Span::raw(" "));
        spans.extend(timeline_bar_spans(
            frame_count,
            app.frame_index,
            &states,
            bar_width,
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            hours_label,
            Style::default().fg(Color::DarkGray),
        ));
    }

    TextLine::from(spans)
}

/// Map frame `i` (0 = newest) to a bar position (0 = left/oldest, width-1 = right/newest).
fn frame_to_bar_pos(i: usize, frame_count: usize, bar_width: usize) -> usize {
    if bar_width == 0 {
        return 0;
    }
    if frame_count <= 1 {
        return bar_width.saturating_sub(1);
    }
    let normalized = i as f64 / (frame_count - 1) as f64;
    let pos = ((1.0 - normalized) * (bar_width - 1) as f64).round() as usize;
    pos.min(bar_width - 1)
}

/// Nothing available here: no slot, or a slot not downloaded yet.
///
/// The two cases were once separate greys, but "no data" and "no data yet" look
/// the same to the eye and the distinction only appears before the frame list
/// arrives — one colour says it.
const BAR_MISSING: Color = Color::Rgb(96, 96, 96);
/// A slot whose grid is on disk.  This is the bar's "full" state: everything
/// downloaded reads as a solid white run.
const BAR_DOWNLOADED: Color = Color::Rgb(230, 230, 230);
/// Stipple marking slots also decoded in RAM.
///
/// In-RAM implies on-disk, so these dots always land on [`BAR_DOWNLOADED`];
/// drawing them dark makes RAM read as texture over the white rather than as a
/// third colour competing with it.
const BAR_RAM_DOTS: Color = Color::Rgb(78, 78, 78);
/// The playhead.  Amber rather than a grey so it stays findable against a bar
/// that is otherwise monochrome.
const BAR_PLAYHEAD: Color = Color::Rgb(255, 200, 60);

/// Braille dot bits per column, top row first.  Standard U+28xx layout: the
/// left column is dots 1,2,3,7 and the right column dots 4,5,6,8.
const BRAILLE_COL_BITS: [[u8; 4]; 2] = [[0, 1, 2, 6], [3, 4, 5, 7]];

/// Dot rows available per braille column — the sub-resolution the stipple buys.
const BRAILLE_ROWS: u16 = 4;

/// Braille glyph whose two columns are filled from the bottom to `heights`
/// dots each (0..=4).
fn braille_columns(heights: [u16; 2]) -> char {
    let mut bits = 0u8;
    for (col, &h) in heights.iter().enumerate() {
        // Fill upward from the bottom row so partial columns sit on the
        // baseline and read as a level rather than floating.
        for row in (BRAILLE_ROWS - h.min(BRAILLE_ROWS))..BRAILLE_ROWS {
            bits |= 1 << BRAILLE_COL_BITS[col][row as usize];
        }
    }
    char::from_u32(0x2800 + u32::from(bits)).unwrap_or(' ')
}

/// Availability of one radar slot, cheapest to costliest to display.
#[derive(Clone, Copy, PartialEq)]
pub enum SlotState {
    /// Neither in RAM nor on disk — displaying it needs a fetch.
    Missing,
    /// GeoTIFF on disk: no fetch, but still a decode.
    OnDisk,
    /// Decoded and ready.
    InRam,
}

/// The slots falling under one braille column (half a terminal cell).
#[derive(Clone, Copy, Default)]
struct SubCol {
    total: u16,
    downloaded: u16,
    in_ram: u16,
    playhead: bool,
}

impl SubCol {
    /// The one colour this column paints.
    ///
    /// Deliberately discrete: a column that is part downloaded resolves to
    /// whichever state most of it is in, rather than to a blended grey.  An
    /// in-between shade carries no meaning a reader can name — it just looks
    /// like a third kind of slot — and boundaries are better shown by splitting
    /// the cell than by mixing the colour.
    fn paint(self) -> Color {
        if self.playhead {
            BAR_PLAYHEAD
        } else if self.total > 0 && self.downloaded * 2 >= self.total {
            BAR_DOWNLOADED
        } else {
            BAR_MISSING
        }
    }

    /// Dots to raise for this column: the share of its slots held in RAM.
    fn ram_height(self) -> u16 {
        if self.total == 0 || self.in_ram == 0 {
            return 0;
        }
        let filled = f64::from(self.in_ram) / f64::from(self.total) * f64::from(BRAILLE_ROWS);
        // Never round a non-empty column down to nothing — one cached frame in
        // a busy column should still show.
        (filled.round() as u16).clamp(1, BRAILLE_ROWS)
    }
}

/// Build coloured spans for the timeline bar.
///
/// The bar shows two facts with four colours and no shades in between:
///
/// * **Background** — downloaded ([`BAR_DOWNLOADED`]) or not ([`BAR_MISSING`]),
///   with the playhead ([`BAR_PLAYHEAD`]) overriding its own half-cell.
/// * **Braille dots** ([`BAR_RAM_DOTS`]) — what is additionally decoded in RAM.
///   Two columns of four dots per cell, so the stipple resolves at twice the
///   cell count and shows how *much* of a column is resident rather than merely
///   whether any of it is.  That matters at depth: 24 h is 288 slots over at
///   most 48 cells.
///
/// Where a cell's two halves disagree — a download boundary, or the playhead —
/// it is drawn as `▌` split between the two colours instead of blending them.
/// The split cell gives up its dots, but there is only ever a handful of them,
/// and a sharp edge reads as a boundary where a mixed grey reads as a third
/// kind of slot.
fn timeline_bar_spans(
    frame_count: usize,
    frame_index: usize,
    states: &[SlotState],
    bar_width: usize,
) -> Vec<Span<'static>> {
    if bar_width == 0 {
        return vec![];
    }

    let sub_count = bar_width * 2;
    let mut subs = vec![SubCol::default(); sub_count];
    for i in 0..frame_count {
        let pos = frame_to_bar_pos(i, frame_count, sub_count);
        let state = states.get(i).copied().unwrap_or(SlotState::Missing);
        let s = &mut subs[pos];
        s.total += 1;
        // In-RAM implies downloaded, so it counts toward both layers.
        if matches!(state, SlotState::OnDisk | SlotState::InRam) {
            s.downloaded += 1;
        }
        if state == SlotState::InRam {
            s.in_ram += 1;
        }
    }
    if frame_count > 0 {
        subs[frame_to_bar_pos(frame_index, frame_count, sub_count)].playhead = true;
    }

    // Merge neighbours that resolve to the same glyph and style so the line
    // stays a handful of spans rather than one per cell.
    let cell = |i: usize| -> (char, Style) {
        let (l, r) = (subs[i * 2], subs[i * 2 + 1]);
        let (lp, rp) = (l.paint(), r.paint());
        if lp == rp {
            // Uniform: the whole cell is one colour, free to carry dots.
            let glyph = braille_columns([l.ram_height(), r.ram_height()]);
            (glyph, Style::default().fg(BAR_RAM_DOTS).bg(lp))
        } else {
            // Split: `▌` paints the left half in fg and the right half in bg.
            ('▌', Style::default().fg(lp).bg(rp))
        }
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut i = 0;
    while i < bar_width {
        let (glyph, style) = cell(i);
        let mut run = String::new();
        while i < bar_width && cell(i) == (glyph, style) {
            run.push(glyph);
            i += 1;
        }
        spans.push(Span::styled(run, style));
    }
    spans
}

fn render_map(frame: &mut ratatui::Frame<'_>, area: Rect, app: &mut App, reuse_raster: bool) {
    let width = area.width.max(1);
    let height = area.height.max(1);

    // A chrome-only animation frame: the map is unchanged, so redraw the
    // cells computed last frame instead of rasterising them again.  Guard on
    // the grid matching the current area — a resize invalidates it.
    if reuse_raster && app.braille_frame.cells().len() == usize::from(width) * usize::from(height) {
        blit_cells(
            app.braille_frame.cells(),
            area,
            width,
            height,
            frame.buffer_mut(),
        );
        if !app.is_dragging {
            render_layer_list(frame, area, app);
            let now = Instant::now();
            let reserved = task_queue_reserved_rows(&app.active_tasks, now);
            if app.show_legend {
                render_legend(frame, area, app.layers.mode_state(), reserved);
            }
            render_task_queue(frame, area, app, now);
        }
        return;
    }

    let bounds = app.viewport.bounds(area.width, area.height);

    let stamp = BorderMaskStamp {
        zoom_bits: (app.viewport.zoom * 1000.0).round().to_bits(),
        resolution: desired_border_resolution(app),
        show_regions: show_regions(app),
        show_roads: show_roads(app),
        width: area.width,
        height: area.height,
        layers_version: app.border_layers_version,
    };

    // On resolution cutover (zoom crossing a threshold) keep the old
    // mask alive for one frame so the user doesn't see a blank flash.
    if let Some((ref cached_stamp, _)) = app.border_mask_cache {
        if cached_stamp.resolution != stamp.resolution {
            app.fallback_mask_cache = app.border_mask_cache.take();
        }
    }

    let sub_width = u32::from(area.width.max(1)) * 2;
    let sub_height = u32::from(area.height.max(1)) * 4;

    // If pan exceeded 50 % of the viewport, invalidate so the next
    // call to `get_or_compute_border_mask` does a full recompute.
    // Skip this during active drag — the offset-shifted mask is good
    // enough and recompute would cause visible lag/stutter.
    if !app.is_dragging {
        if let Some((_, mask)) = app.border_mask_cache.as_ref() {
            let (dx_sub, dy_sub) = subcell_offset(
                app.viewport.center,
                mask.center,
                &bounds,
                sub_width,
                sub_height,
            );
            let max_dx = (sub_width as f64 * 0.5) as i32;
            let max_dy = (sub_height as f64 * 0.5) as i32;
            if dx_sub.abs() > max_dx || dy_sub.abs() > max_dy {
                app.border_mask_cache = None;
            }
        }
    }

    let had_mask = app.border_mask_cache.is_some();
    let mask_t0 = std::time::Instant::now();
    get_or_compute_border_mask(app, stamp, bounds, area.width, area.height);

    if app.border_mask_cache.is_some() && !had_mask {
        let dt = mask_t0.elapsed();
        if dt.as_millis() > 5 {
            write_log(
                &app.dirs.log_path,
                format!(
                    "perf: mask recompute {:.1?} ({} × {} subcells, z={:.2})",
                    dt, area.width, area.height, app.viewport.zoom,
                ),
            );
        }
    }

    // If nothing could be computed, promote the fallback (old resolution
    // held for one frame).  Otherwise discard it.
    if app.border_mask_cache.is_none() {
        if let Some(fallback) = app.fallback_mask_cache.take() {
            app.border_mask_cache = Some(fallback);
        }
    } else {
        app.fallback_mask_cache = None;
    }

    // Offset from whatever mask is now in the cache to the current
    // viewport centre — marks get shifted by this during rasterisation.
    let offset = app
        .border_mask_cache
        .as_ref()
        .map(|(_, mask)| {
            subcell_offset(
                app.viewport.center,
                mask.center,
                &bounds,
                sub_width,
                sub_height,
            )
        })
        .unwrap_or((0, 0));
    let mut braille_frame = std::mem::take(&mut app.braille_frame);
    raster_map_rows(
        app,
        bounds,
        area.width,
        area.height,
        offset,
        &mut braille_frame,
    );
    blit_cells(
        braille_frame.cells(),
        area,
        area.width.max(1),
        area.height.max(1),
        frame.buffer_mut(),
    );
    app.braille_frame = braille_frame;
    if !app.is_dragging {
        render_layer_list(frame, area, app);
        let now = Instant::now();
        let reserved = task_queue_reserved_rows(&app.active_tasks, now);
        if app.show_legend {
            render_legend(frame, area, app.layers.mode_state(), reserved);
        }
        render_task_queue(frame, area, app, now);
    }
}

fn get_or_compute_border_mask(
    app: &mut App,
    stamp: BorderMaskStamp,
    bounds: Bounds,
    width: u16,
    height: u16,
) {
    if let Some((ref cached_stamp, _)) = app.border_mask_cache {
        if *cached_stamp == stamp {
            return;
        }
    }

    // Use the zoom-appropriate layer for country borders so the
    // vertex count scales with the viewport size.  The `show_regions`
    // / `show_roads` stamp flags independently control whether
    // region/road lines are drawn — they do NOT gate which layer is
    // used, which avoids the "low-poly countries when regions off"
    // problem while keeping performance predictable.
    let sub_width = u32::from(width.max(1)) * 2;
    let sub_height = u32::from(height.max(1)) * 4;
    let cells: Option<Vec<Option<BorderLineKind>>> = app
        .border_layers
        .get(&stamp.resolution)
        .or_else(|| {
            // When the desired resolution isn't loaded yet, fall back
            // to the highest loaded resolution so regions and roads
            // keep rendering.  HashMap iteration order is arbitrary so
            // `values().next()` might pick Low110m which has no region
            // or road data, causing them to disappear.
            use BorderResolution::*;
            [Regional10m, High10m, Medium50m, Low110m]
                .into_iter()
                .find(|r| app.border_layers.contains_key(r))
                .and_then(|r| app.border_layers.get(&r))
        })
        .map(|layer| compute_mask_cells(layer, bounds, sub_width, sub_height, stamp));

    if let Some(cells) = cells {
        let marks = cells
            .iter()
            .enumerate()
            .filter_map(|(index, kind)| {
                kind.map(|kind| BorderMaskPoint {
                    sx: index as u32 % sub_width,
                    sy: index as u32 / sub_width,
                    kind,
                })
            })
            .collect();
        app.border_mask_cache = Some((
            stamp,
            BorderMask {
                cells,
                marks,
                center: app.viewport.center,
            },
        ));
    } else {
        // No data at all: drop any stale mask so we don't display
        // geometry for layers that have since been evicted.
        app.border_mask_cache = None;
    }
}

thread_local! {
    /// Scratch buffer for SpatialGrid candidate line indices, reused
    /// across `compute_mask_cells` calls to avoid per-frame allocation.
    static CANDIDATES: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    /// Scratch bitset for line dedup (1 bit per line), reused similarly.
    static SEEN: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

fn compute_mask_cells(
    borders: &crate::layers::BorderLayer,
    bounds: Bounds,
    sub_width: u32,
    sub_height: u32,
    stamp: BorderMaskStamp,
) -> Vec<Option<BorderLineKind>> {
    let mut mask = vec![None; (sub_width * sub_height) as usize];

    // Use the spatial grid index to collect only lines whose bbox
    // may intersect the viewport.  If no grid is present (old cache
    // data), fall back to a full scan.
    if let Some(grid) = &borders.grid {
        CANDIDATES.with_borrow_mut(|candidates| {
            SEEN.with_borrow_mut(|seen| {
                let n = borders.lines.len();
                let needed = n.div_ceil(8);
                seen.resize(needed, 0);
                grid.lines_for_bounds(bounds, candidates, seen);
                for &id in candidates.iter() {
                    rasterize_line(
                        &mut mask,
                        &borders.lines[id as usize],
                        bounds,
                        sub_width,
                        sub_height,
                        stamp,
                    );
                }
            });
        });
    } else {
        for line in &borders.lines {
            rasterize_line(&mut mask, line, bounds, sub_width, sub_height, stamp);
        }
    }
    mask
}

/// Rasterise a single border line into the mask.  Shared by the
/// grid-prefilter and full-scan paths.
fn rasterize_line(
    mask: &mut [Option<BorderLineKind>],
    line: &BorderLine,
    bounds: Bounds,
    sub_width: u32,
    sub_height: u32,
    stamp: BorderMaskStamp,
) {
    if !should_draw_border_line(line.kind, stamp) {
        return;
    }
    if !line.is_bbox_degenerate() && !bounds.intersects(line.bbox) {
        return;
    }
    // Per-country granularity: skip region/road lines whose bbox
    // occupies fewer than ~4 subcells in both dimensions.  Small
    // countries (e.g. Slovenia) have tiny subdivisions that would
    // look like noise at low zoom; large countries (e.g. Italy)
    // have larger features that appear earlier.  The threshold
    // falls out of the geometry naturally — no per-country tables.
    if line.kind != BorderLineKind::Country && !line.is_bbox_degenerate() {
        let sw = line.bbox.width() / bounds.width() * sub_width as f64;
        let sh = line.bbox.height() / bounds.height() * sub_height as f64;
        if sw < 4.0 && sh < 4.0 {
            return;
        }
    }
    for pair in line.points.windows(2) {
        let a = pair[0];
        let b = pair[1];
        let Some((x1, y1, x2, y2)) = clipped_segment(bounds, a.x, a.y, b.x, b.y) else {
            continue;
        };
        mark_border_segment(
            mask, bounds, sub_width, sub_height, x1, y1, x2, y2, line.kind,
        );
    }
}

#[derive(Debug, Default)]
pub struct BrailleFrame {
    cells: Vec<RasterCell>,
}

impl BrailleFrame {
    fn reset(&mut self, width: u16, height: u16) {
        let needed = usize::from(width) * usize::from(height);
        if self.cells.len() != needed {
            self.cells.resize(needed, RasterCell::default());
        }
        for cell in &mut self.cells {
            cell.clear();
        }
    }

    fn cells(&self) -> &[RasterCell] {
        &self.cells
    }

    fn cells_mut(&mut self) -> &mut [RasterCell] {
        &mut self.cells
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RasterCell {
    bits: u8,
    color: Option<Rgb8>,
    intensity: u8,
    glyph: Option<char>,
    bg: Option<Rgb8>,
    modifier: Modifier,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PackedBrailleCell {
    pub bits: u8,
    pub fg: Option<Rgb8>,
    pub bg: Option<Rgb8>,
    pub glyph: Option<char>,
    pub modifier: Modifier,
}

impl RasterCell {
    fn clear(&mut self) {
        self.bits = 0;
        self.color = None;
        self.intensity = 0;
        self.glyph = None;
        self.bg = None;
        self.modifier = Modifier::empty();
    }

    fn packed(self) -> PackedBrailleCell {
        PackedBrailleCell {
            bits: self.bits,
            fg: self.color,
            bg: self.bg,
            glyph: self.glyph,
            modifier: self.modifier,
        }
    }
}

fn handle_layer_enable(app: &mut App, id: LayerId, refresh: &mut bool) {
    match id {
        LayerId::Radar => {
            *refresh = app.layers.enabled(LayerId::Radar);
        }
        LayerId::RegionBorders | LayerId::MajorRoads => {
            app.border_mask_cache = None;
            if app.layers.enabled(id) {
                app.request_border_layer(BorderResolution::Regional10m);
            }
        }
        LayerId::MeteoAlarm => {
            app.pending_warning_refresh = app.layers.enabled(id);
        }
        LayerId::Lightning => {
            if app.layers.enabled(LayerId::Lightning) {
                app.request_lightning_connect();
            } else {
                app.abort_lightning();
                app.lightning_strikes.clear();
            }
        }
        // Switching the pin off is how you clear a search, so drop the point
        // and its footer line rather than just hiding them — otherwise
        // toggling back on would resurrect a stale pin.
        LayerId::SearchPin if !app.layers.enabled(LayerId::SearchPin) => {
            app.clear_search_pin();
        }
        id if (id.is_observation()) && app.layers.enabled(id) => {
            app.request_obs_refresh();
        }
        _ => {}
    }
}

fn raster_map_rows(
    app: &App,
    bounds: Bounds,
    width: u16,
    height: u16,
    mask_offset: (i32, i32),
    braille_frame: &mut BrailleFrame,
) {
    let width = width.max(1);
    let height = height.max(1);
    braille_frame.reset(width, height);
    let cells = braille_frame.cells_mut();

    // Draw order (bottom → top): regions → roads → country borders → radar.
    // Country borders sit on top so a national boundary always wins over a
    // coincident road or region line.
    if let Some((_, mask)) = &app.border_mask_cache {
        let (dx, dy) = mask_offset;
        let w_usize = usize::from(width);
        let cells_len = cells.len();
        for mark in &mask.marks {
            let sx = mark.sx as i32 + dx;
            let sy = mark.sy as i32 + dy;
            if sx < 0 || sy < 0 {
                continue;
            }
            let sx = sx as u32;
            let sy = sy as u32;
            let cell_x = (sx / 2) as usize;
            let cell_y = (sy / 4) as usize;
            let idx = cell_y * w_usize + cell_x;
            if idx >= cells_len {
                continue;
            }
            let cell = &mut cells[idx];
            let bit = braille_bit(sx % 2, sy % 4);
            cell.bits |= bit;
            let color = border_line_color(mark.kind);
            if mark.kind == BorderLineKind::Country || cell.intensity == 0 {
                cell.color = Some(color);
                cell.intensity = 1;
            }
        }
    }

    let modes = app.layers.mode_state();

    if modes.has_any(LayerId::Radar) {
        if let Some(radar) = &app.radar_frame {
            raster_radar(cells, radar, bounds, width, height, modes);
        }
    }

    if modes.has_any(LayerId::MeteoAlarm) {
        raster_warnings(cells, app, bounds, width, height);
    }

    raster_observations(cells, app, bounds, width, height);

    raster_lightning(cells, app, bounds, width, height);

    raster_location_marker(
        cells,
        app.location_fix(),
        app.location_label(),
        bounds,
        width,
        height,
        modes,
    );

    if let Some(pin) = app.search_pin() {
        raster_pin(
            cells,
            pin,
            bounds,
            width,
            height,
            modes,
            LayerId::SearchPin,
            Rgb8::BLUE,
            app.search_label(),
        );
    }
}

/// How far to look for a free cell when the `x` would land on existing text.
/// Beyond a couple of cells the marker would no longer read as marking *this*
/// spot, so it is better to overwrite than to drift.
const PIN_NUDGE_RADIUS: u16 = 3;

/// Draw a map pin: an `x` glyph and/or a coloured cell background.
///
/// Both modes are overlays, so they annotate the cell without taking a render
/// mode away from radar or the temperature readings:
/// - `Text` — a coloured `x`, leaving the cell's background alone.
/// - `Color` — a coloured background, leaving the foreground alone so radar
///   braille underneath still reads through.
///
/// Draws the "you are here" marker and its label, gated on [`LocationFix::is_displayable`].
///
/// A fix coarser than [`DISPLAY_ACCURACY_M`](crate::providers::location::DISPLAY_ACCURACY_M)
/// (or of unknown accuracy, except `Manual`) would draw a dot claiming a
/// precision the fix does not have — so the marker and its place-name label
/// are suppressed together rather than picking one to keep. This is a
/// display gate only; the viewport still centres on the first fix regardless
/// of accuracy (see `initial_viewport` in `app.rs`).
#[allow(clippy::too_many_arguments)]
fn raster_location_marker(
    cells: &mut [RasterCell],
    fix: Option<LocationFix>,
    label: Option<&str>,
    bounds: Bounds,
    width: u16,
    height: u16,
    modes: &RenderModeState,
) {
    let Some(fix) = fix else { return };
    if !fix.is_displayable() {
        return;
    }
    raster_pin(
        cells,
        fix.point,
        bounds,
        width,
        height,
        modes,
        LayerId::Location,
        Rgb8::RED,
        label,
    );
}

/// Shared by the "you are here" marker and the search pin, which differ only
/// in colour and which layer owns the modes.
#[allow(clippy::too_many_arguments)]
fn raster_pin(
    cells: &mut [RasterCell],
    point: GeoPoint,
    bounds: Bounds,
    width: u16,
    height: u16,
    modes: &RenderModeState,
    layer: LayerId,
    color: Rgb8,
    label: Option<&str>,
) {
    let text = modes.has(RenderMode::Text, layer);
    let background = modes.has(RenderMode::Color, layer);
    if !text && !background {
        return;
    }
    let world = point.to_world();
    if world.x < bounds.min_x
        || world.x > bounds.max_x
        || world.y < bounds.min_y
        || world.y > bounds.max_y
    {
        return;
    }
    let x = ((world.x - bounds.min_x) / bounds.width().max(f64::EPSILON) * f64::from(width))
        .floor()
        .clamp(0.0, f64::from(width.saturating_sub(1))) as u16;
    let y = ((world.y - bounds.min_y) / bounds.height().max(f64::EPSILON) * f64::from(height))
        .floor()
        .clamp(0.0, f64::from(height.saturating_sub(1))) as u16;

    // The background marks the true cell: it only tints, so it can never
    // erase a city name or a reading and never needs to move.
    if background {
        let idx = usize::from(y) * usize::from(width) + usize::from(x);
        if let Some(cell) = cells.get_mut(idx) {
            cell.bg = Some(color);
        }
    }

    if text {
        // The glyph *does* overwrite, so nudge it to the nearest free cell
        // rather than blanking a city name or a temperature reading.
        let (gx, gy) = nearest_free_cell(cells, width, height, x, y).unwrap_or((x, y));
        let idx = usize::from(gy) * usize::from(width) + usize::from(gx);
        if let Some(cell) = cells.get_mut(idx) {
            cell.glyph = Some('x');
            cell.color = Some(color);
        }

        // Name the place the pin marks.  If that name is already on the map —
        // standing in Ljubljana, whose capital label is right there — recolour
        // the existing text instead of writing a second copy beside it.
        if let Some(name) = label.map(str::trim).filter(|s| !s.is_empty()) {
            if !recolor_existing_label(cells, width, height, name, color) {
                write_pin_label(cells, width, height, gx, gy, name, color);
            }
        }
    }
}

/// Recolour an existing on-screen copy of `name`, if there is one.
///
/// Returns `true` when a match was found and recoloured, meaning the caller
/// should not draw its own label.
///
/// The whole grid is searched, not a window around the pin.  The two labels
/// are anchored to different things — the pin sits at your measured position,
/// the capital label at the city's own hardcoded centre — so they can be many
/// cells apart and still name the same place; a fix on the edge of Ljubljana
/// put them five rows apart and printed "Ljubljana" twice.  A same-named place
/// elsewhere on screen is possible in principle but far rarer than that.
fn recolor_existing_label(
    cells: &mut [RasterCell],
    width: u16,
    height: u16,
    name: &str,
    color: Rgb8,
) -> bool {
    let w = usize::from(width);
    let target: Vec<char> = name.chars().collect();
    if target.is_empty() || target.len() > w {
        return false;
    }

    let first = target[0];
    for row in 0..height {
        let base = usize::from(row) * w;
        // Compare case-insensitively: station and capital labels do not agree
        // on casing, and "LJUBLJANA" is still the place the pin is marking.
        for start in 0..=(w - target.len()) {
            // Prefilter on the first glyph before paying for the full
            // comparison: almost every start offset in an unrelated row is
            // ruled out by its first character alone. Uses the same
            // comparison as the full path below, or a fast "LJUBLJANA" pin
            // would stop matching "Ljubljana".
            let first_matches = cells.get(base + start).is_some_and(|c| {
                c.glyph
                    .is_some_and(|g| g.eq_ignore_ascii_case(&first) || g == first)
            });
            if !first_matches {
                continue;
            }
            #[cfg(test)]
            tests::LABEL_FULL_COMPARE_CALLS.with(|c| c.set(c.get() + 1));
            let matches = target.iter().enumerate().all(|(i, want)| {
                cells.get(base + start + i).is_some_and(|c| {
                    c.glyph
                        .is_some_and(|g| g.eq_ignore_ascii_case(want) || g == *want)
                })
            });
            if matches {
                for i in 0..target.len() {
                    if let Some(cell) = cells.get_mut(base + start + i) {
                        cell.color = Some(color);
                    }
                }
                return true;
            }
        }
    }
    false
}

/// Write the pin's label one row below the marker, if there is room.
///
/// The label is skipped rather than overwritten onto occupied cells: a partly
/// clobbered city name or temperature reading is worse than an unlabelled pin,
/// which still shows position via the `x` itself.
fn write_pin_label(
    cells: &mut [RasterCell],
    width: u16,
    height: u16,
    x: u16,
    y: u16,
    name: &str,
    color: Rgb8,
) {
    let row = y + 1;
    if row >= height {
        return;
    }
    let w = usize::from(width);
    let base = usize::from(row) * w;
    // Centre the label under the marker, clamped into the viewport.
    let len = name.chars().count();
    let start = usize::from(x)
        .saturating_sub(len / 2)
        .min(w.saturating_sub(len));
    if start + len > w {
        return;
    }
    if !(start..start + len).all(|cx| cells.get(base + cx).is_some_and(|c| c.glyph.is_none())) {
        return;
    }
    for (i, ch) in name.chars().enumerate() {
        if let Some(cell) = cells.get_mut(base + start + i) {
            cell.glyph = Some(ch);
            cell.color = Some(color);
        }
    }
}

/// Find the closest cell to (`x`, `y`) that holds no glyph, searching outward
/// to `PIN_NUDGE_RADIUS`.
///
/// Returns `(x, y)` itself when already free, and `None` when everything
/// within the radius is occupied — the caller then overwrites, since a marker
/// that silently vanishes is worse than one that covers a label.
fn nearest_free_cell(
    cells: &[RasterCell],
    width: u16,
    height: u16,
    x: u16,
    y: u16,
) -> Option<(u16, u16)> {
    let free = |cx: u16, cy: u16| -> bool {
        cells
            .get(usize::from(cy) * usize::from(width) + usize::from(cx))
            .is_some_and(|c| c.glyph.is_none())
    };
    if free(x, y) {
        return Some((x, y));
    }

    // Consider one candidate cell against the current best, by squared
    // (y-weighted) distance. `best` is threaded through explicitly rather
    // than captured, so the ring loops below can each call this without
    // fighting the borrow checker.
    let consider = |cx: i32, cy: i32, best: &mut Option<(u16, u16, u32)>| {
        if cx < 0 || cy < 0 || cx >= i32::from(width) || cy >= i32::from(height) {
            return;
        }
        let (cx, cy) = (cx as u16, cy as u16);
        if !free(cx, cy) {
            return;
        }
        // Squared cell distance; columns are ~half as wide as rows are
        // tall, so weight y to keep the nudge visually shortest.
        let dx = u32::from(x.abs_diff(cx));
        let dy = u32::from(y.abs_diff(cy)) * 2;
        let d2 = dx * dx + dy * dy;
        if best.is_none_or(|(_, _, bd)| d2 < bd) {
            *best = Some((cx, cy, d2));
        }
    };

    let (xi, yi) = (i32::from(x), i32::from(y));
    for r in 1..=PIN_NUDGE_RADIUS {
        let ri = i32::from(r);
        let mut best: Option<(u16, u16, u32)> = None;

        // Walk only the ring at distance r, in the same row-major order the
        // old full-square sweep visited its ring cells in, so a tie between
        // two equally-close free cells still resolves to the same winner:
        // top row, then the ring's side columns (top to bottom), then the
        // bottom row.
        for cx in (xi - ri)..=(xi + ri) {
            consider(cx, yi - ri, &mut best);
        }
        for cy in (yi - ri + 1)..=(yi + ri - 1) {
            consider(xi - ri, cy, &mut best);
            consider(xi + ri, cy, &mut best);
        }
        for cx in (xi - ri)..=(xi + ri) {
            consider(cx, yi + ri, &mut best);
        }

        if let Some((bx, by, _)) = best {
            return Some((bx, by));
        }
    }
    None
}

fn desired_border_resolution(app: &App) -> BorderResolution {
    BorderResolution::for_zoom(app.viewport.zoom)
}

fn show_regions(app: &App) -> bool {
    // Regions are too noisy at continent zoom — gated by
    // includes_regions() which kicks in at High10m (zoom ≥ 5.5).
    desired_border_resolution(app).includes_regions() && app.layers.enabled(LayerId::RegionBorders)
}

fn show_roads(app: &App) -> bool {
    // Roads are too noisy below zoom 3.5 (continent view).  No
    // resolution gate — once cached they appear at any reasonable zoom.
    app.viewport.zoom >= 3.5 && app.layers.enabled(LayerId::MajorRoads)
}

fn raster_radar(
    cells: &mut [RasterCell],
    radar: &RadarFrame,
    bounds: Bounds,
    width: u16,
    height: u16,
    modes: &RenderModeState,
) {
    let id = LayerId::Radar;
    let in_braille = modes.has(RenderMode::Braille, id);
    let in_color = modes.has(RenderMode::Color, id);
    let in_text = modes.has(RenderMode::Text, id);
    if !in_braille && !in_color && !in_text {
        return;
    }

    let sub_width = u32::from(width) * 2;
    let sub_height = u32::from(height) * 4;
    let cells_len = cells.len();
    let w_usize = usize::from(width);

    let sx_scale = sub_width as f64 / bounds.width().max(f64::EPSILON);
    let sy_scale = sub_height as f64 / bounds.height().max(f64::EPSILON);
    let min_x = bounds.min_x;
    let min_y = bounds.min_y;

    // Color-only fast path: one bg write per terminal cell.  Skips
    // braille bit computation and coalesces radar rows that map to the
    // same terminal cell row (4 subcell rows per cell).
    if in_color && !in_braille && !in_text {
        for tile in &radar.tiles {
            let tb = tile_bounds(tile.coord);
            if !bounds.intersects(tb) {
                continue;
            }
            let tile_world_width = tb.max_x - tb.min_x;
            let tile_world_height = tb.max_y - tb.min_y;
            let inv_size = 1.0 / f64::from(tile.size);
            let tile_rows = &tile.rows;

            let mut row_idx = 0usize;
            while row_idx < tile_rows.len() {
                let world_y_start = tb.min_y + row_idx as f64 * inv_size * tile_world_height;
                let start_sy = ((world_y_start - min_y) * sy_scale).floor() as i32;
                let start_cell_y = (start_sy.clamp(0, sub_height as i32) as u32 / 4) as usize;

                // Find the last radar row that maps to the same cell row.
                let mut end_idx = row_idx;
                while end_idx + 1 < tile_rows.len() {
                    let ny = tb.min_y + (end_idx + 2) as f64 * inv_size * tile_world_height;
                    let next_sy = ((ny - min_y) * sy_scale).floor() as i32;
                    let next_cell_y = (next_sy.clamp(0, sub_height as i32) as u32 / 4) as usize;
                    if next_cell_y != start_cell_y {
                        break;
                    }
                    end_idx += 1;
                }

                let runs = &tile_rows[end_idx];
                for run in runs {
                    let world_x_start =
                        tb.min_x + f64::from(run.start_x) * inv_size * tile_world_width;
                    let world_x_end = tb.min_x + f64::from(run.end_x) * inv_size * tile_world_width;
                    let start_sx = ((world_x_start - min_x) * sx_scale).floor() as i32;
                    let end_sx = ((world_x_end - min_x) * sx_scale).ceil() as i32;
                    let start_sx = start_sx.clamp(0, sub_width as i32) as u32;
                    let end_sx = end_sx.clamp(0, sub_width as i32) as u32;
                    if start_sx >= end_sx {
                        continue;
                    }

                    let color = run.color;
                    let start_cell_x = (start_sx / 2) as usize;
                    let end_cell_x = ((end_sx - 1) / 2) as usize + 1;
                    let row_base = start_cell_y * w_usize;
                    for cx in start_cell_x..end_cell_x {
                        let idx = row_base + cx;
                        if idx < cells_len {
                            cells[idx].bg = Some(color);
                        }
                    }
                }

                row_idx = end_idx + 1;
            }
        }
        return;
    }

    // Full path: braille + optional color/text overlays.
    //
    // Split into horizontal bands of terminal rows.  Each band owns a
    // disjoint slice of `cells`, so bands can run in parallel without
    // synchronisation, and within a band tiles/rows/runs are still visited
    // in the original order — so the output is identical either way.  Rows
    // outside a band are rejected before touching that row's runs, which is
    // where nearly all the work is.
    let draw_band = |band: &mut [RasterCell], cy_lo: usize| {
        let cy_hi = cy_lo + band.len() / w_usize;
        for tile in &radar.tiles {
            let tb = tile_bounds(tile.coord);
            if !bounds.intersects(tb) {
                continue;
            }

            let tile_world_width = tb.max_x - tb.min_x;
            let tile_world_height = tb.max_y - tb.min_y;
            let inv_size = 1.0 / f64::from(tile.size);

            // Radar rows map linearly onto subcell-Y, so derive the window of
            // rows that can touch this band rather than scanning them all —
            // otherwise every band pays for every row of every tile.  The
            // window is deliberately loose (±1 row); the per-row band check
            // below stays the authority on what actually gets drawn.
            let rows_len = tile.rows.len();
            let sy_per_row = inv_size * tile_world_height * sy_scale;
            let sy_row0 = (tb.min_y - min_y) * sy_scale;
            let (i_lo, i_hi) = if sy_per_row > 0.0 {
                let lo = (((cy_lo * 4) as f64 - sy_row0) / sy_per_row).floor() as i64 - 1;
                let hi = (((cy_hi * 4) as f64 - sy_row0) / sy_per_row).ceil() as i64 + 1;
                let max = rows_len as i64;
                (lo.clamp(0, max) as usize, hi.clamp(0, max) as usize)
            } else {
                (0, rows_len)
            };

            for (row_index, runs) in tile.rows.iter().enumerate().take(i_hi).skip(i_lo) {
                let world_y_start = tb.min_y + row_index as f64 * inv_size * tile_world_height;
                let world_y_end = tb.min_y + (row_index + 1) as f64 * inv_size * tile_world_height;

                let start_sy = ((world_y_start - min_y) * sy_scale).floor() as i32;
                let end_sy = ((world_y_end - min_y) * sy_scale).ceil() as i32;
                let start_sy = start_sy.clamp(0, sub_height as i32) as u32;
                let end_sy = end_sy.clamp(0, sub_height as i32) as u32;
                if start_sy >= end_sy {
                    continue;
                }

                let cy_start = (start_sy / 4) as usize;
                let cy_end = ((end_sy - 1) / 4) as usize;
                // This row lands entirely outside the band — skip its runs.
                if cy_end < cy_lo || cy_start >= cy_hi {
                    continue;
                }

                for run in runs {
                    let world_x_start =
                        tb.min_x + f64::from(run.start_x) * inv_size * tile_world_width;
                    let world_x_end = tb.min_x + f64::from(run.end_x) * inv_size * tile_world_width;

                    let start_sx = ((world_x_start - min_x) * sx_scale).floor() as i32;
                    let end_sx = ((world_x_end - min_x) * sx_scale).ceil() as i32;
                    let start_sx = start_sx.clamp(0, sub_width as i32) as u32;
                    let end_sx = end_sx.clamp(0, sub_width as i32) as u32;
                    if start_sx >= end_sx {
                        continue;
                    }

                    let color = run.color;
                    let intensity = run.intensity;

                    // Iterate terminal cells, not subcells: a run spans up to
                    // 8 subcells per cell, so accumulating the braille bits per
                    // cell (rather than one write per dot) cuts the inner loop
                    // ~8× while producing byte-identical output.
                    let glyph = in_text.then(|| radar_glyph(intensity));
                    let cx_start = (start_sx / 2) as usize;
                    let cx_end = ((end_sx - 1) / 2) as usize;

                    for cy in cy_start.max(cy_lo)..=cy_end.min(cy_hi.saturating_sub(1)) {
                        // Braille bit masks for the left/right columns, ORed
                        // over this cell's covered sub-rows.
                        let (mut col0, mut col1) = (0u8, 0u8);
                        if in_braille {
                            let cell_sy0 = cy as u32 * 4;
                            let sy_lo = start_sy.max(cell_sy0);
                            let sy_hi = end_sy.min(cell_sy0 + 4);
                            for sy in sy_lo..sy_hi {
                                let r = sy & 3;
                                col0 |= braille_bit(0, r);
                                col1 |= braille_bit(1, r);
                            }
                        }
                        let row_base = (cy - cy_lo) * w_usize;
                        for cx in cx_start..=cx_end {
                            let idx = row_base + cx;
                            if cx >= w_usize || idx >= band.len() {
                                continue;
                            }
                            let cell = &mut band[idx];
                            if in_braille {
                                let cell_sx0 = cx as u32 * 2;
                                let mut bits = 0u8;
                                if cell_sx0 >= start_sx && cell_sx0 < end_sx {
                                    bits |= col0;
                                }
                                if cell_sx0 + 1 >= start_sx && cell_sx0 + 1 < end_sx {
                                    bits |= col1;
                                }
                                cell.bits |= bits;
                                if intensity >= cell.intensity {
                                    cell.color = Some(color);
                                    cell.intensity = intensity;
                                }
                            }
                            if in_color {
                                cell.bg = Some(color);
                            }
                            if in_text {
                                cell.glyph = glyph;
                                cell.color = Some(color);
                            }
                        }
                    }
                }
            }
        }
    };

    // One band per worker, but never thinner than a few rows — below that the
    // per-band tile/row rescan costs more than the parallelism returns.
    const MIN_BAND_ROWS: usize = 4;
    let threads = rayon::current_num_threads();
    let rows_per_band = (usize::from(height).div_ceil(threads)).max(MIN_BAND_ROWS);
    if threads == 1 || usize::from(height) <= MIN_BAND_ROWS {
        draw_band(cells, 0);
    } else {
        cells
            .par_chunks_mut(w_usize * rows_per_band)
            .enumerate()
            .for_each(|(i, band)| draw_band(band, i * rows_per_band));
    }
}

fn radar_glyph(intensity: u8) -> char {
    match intensity {
        0 => '·',
        1 => '∘',
        2 => '○',
        3 => '●',
        _ => '◆',
    }
}

fn raster_warnings(cells: &mut [RasterCell], app: &App, bounds: Bounds, width: u16, height: u16) {
    let warning_layer = match &app.warning_layer {
        Some(w) => w,
        None => return,
    };

    let modes = app.layers.mode_state();
    let id = LayerId::MeteoAlarm;
    let in_braille = modes.has(RenderMode::Braille, id);
    let in_color = modes.has(RenderMode::Color, id);
    let in_text = modes.has(RenderMode::Text, id);
    if !in_braille && !in_color && !in_text {
        return;
    }

    let sub_width = u32::from(width) * 2;
    let sub_height = u32::from(height) * 4;

    for feature in &warning_layer.features {
        let color = feature.color();
        let poly = &feature.polygon;
        if poly.len() < 3 {
            continue;
        }

        let mut min_sy = i32::MAX;
        let mut max_sy = i32::MIN;

        let sub_poly: Vec<(i32, i32)> = poly
            .iter()
            .filter_map(|p| {
                if p.x < bounds.min_x
                    || p.x > bounds.max_x
                    || p.y < bounds.min_y
                    || p.y > bounds.max_y
                {
                    None
                } else {
                    let sx = world_to_subcell_axis(p.x, bounds.min_x, bounds.width(), sub_width);
                    let sy = world_to_subcell_axis(p.y, bounds.min_y, bounds.height(), sub_height);
                    min_sy = min_sy.min(sy);
                    max_sy = max_sy.max(sy);
                    Some((sx, sy))
                }
            })
            .collect();

        if sub_poly.len() < 3 {
            continue;
        }

        min_sy = min_sy.max(0);
        max_sy = max_sy.min(sub_height as i32 - 1);

        for sy in min_sy..=max_sy {
            let mut intersections: Vec<i32> = Vec::new();
            let num_verts = sub_poly.len();
            for i in 0..num_verts {
                let (x1, y1) = sub_poly[i];
                let (x2, y2) = sub_poly[(i + 1) % num_verts];
                let (min_y, max_y) = if y1 < y2 { (y1, y2) } else { (y2, y1) };
                if sy < min_y || sy >= max_y {
                    continue;
                }
                if y2 == y1 {
                    continue;
                }
                let x = x1 + (x2 - x1) * (sy - y1) / (y2 - y1);
                intersections.push(x);
            }
            intersections.sort();
            for pair in intersections.chunks(2) {
                if pair.len() < 2 {
                    continue;
                }
                let sx_start = pair[0].max(0).min(sub_width as i32 - 1);
                let sx_end = pair[1].max(0).min(sub_width as i32 - 1);
                for sx in sx_start..=sx_end {
                    if in_braille {
                        set_subcell(cells, width, sx as u32, sy as u32, color, 2);
                    }
                    if in_color {
                        set_subcell_bg(cells, width, sx as u32, sy as u32, color);
                    }
                    if in_text {
                        set_subcell_glyph(cells, width, sx as u32, sy as u32, '⚠', color);
                    }
                }
            }
        }
    }
}

// Braille bolt layout — top-right → bottom-left zigzag:
//   row 0  . ●  = 0x08
//   row 1  ● .  = 0x02   ← kink
//   row 2  . ●  = 0x20
//   row 3  ● .  = 0x40
//
// Negative strikes (common) grow top-right → bottom-left.
// Positive strikes (rare)   grow bottom-left → top-right (reversed tip/upper).
const BOLT_TIP_NEG: u8 = 0x08; // top-right spark
const BOLT_UPPER_NEG: u8 = 0x08 | 0x02 | 0x20; // top three nodes
const BOLT_TIP_POS: u8 = 0x40; // bottom-left spark
const BOLT_UPPER_POS: u8 = 0x40 | 0x20 | 0x02; // bottom three nodes (grows up)
const BOLT_FULL: u8 = 0x08 | 0x02 | 0x20 | 0x40; // complete zigzag (same for both)

/// Per-frame style shared across all three render modes.
/// Returns `(fg_color, bg_color, braille_bits)`.
///
/// `positive` selects the rare positive-polarity palette (cyan) and reversed
/// bolt animation (bottom-up).  Negative uses the standard yellow palette.
fn impact_frame_style(frame: u32, positive: bool) -> (Rgb8, Rgb8, u8) {
    let (tip, upper) = if positive {
        (BOLT_TIP_POS, BOLT_UPPER_POS)
    } else {
        (BOLT_TIP_NEG, BOLT_UPPER_NEG)
    };
    let bits = match frame {
        0 => tip,
        1 => upper,
        _ => BOLT_FULL,
    };

    let (fg, bg) = if positive {
        // Cyan / blue-white: visually distinct for the rare positive strikes.
        match frame {
            0 => (Rgb8::new(255, 255, 255), Rgb8::new(18, 55, 70)),
            1 => (Rgb8::new(180, 240, 255), Rgb8::new(90, 215, 240)),
            2 => (Rgb8::new(60, 220, 255), Rgb8::new(80, 230, 255)),
            3 => (Rgb8::new(55, 200, 230), Rgb8::new(55, 165, 185)),
            4 => (Rgb8::new(48, 170, 200), Rgb8::new(38, 120, 140)),
            5 => (Rgb8::new(40, 140, 165), Rgb8::new(25, 88, 100)),
            6 => (Rgb8::new(32, 110, 130), Rgb8::new(16, 62, 72)),
            _ => (Rgb8::new(24, 82, 97), Rgb8::new(10, 40, 48)),
        }
    } else {
        // Amber / yellow: standard negative-strike palette.
        match frame {
            0 => (Rgb8::new(255, 255, 255), Rgb8::new(70, 70, 18)),
            1 => (Rgb8::new(255, 255, 160), Rgb8::new(220, 220, 120)),
            2 => (Rgb8::new(255, 240, 60), Rgb8::new(255, 245, 100)),
            3 => (Rgb8::new(230, 210, 55), Rgb8::new(180, 165, 55)),
            4 => (Rgb8::new(200, 180, 48), Rgb8::new(130, 118, 38)),
            5 => (Rgb8::new(165, 145, 40), Rgb8::new(90, 80, 25)),
            6 => (Rgb8::new(130, 112, 32), Rgb8::new(60, 54, 16)),
            _ => (Rgb8::new(95, 82, 24), Rgb8::new(38, 34, 10)),
        }
    };
    (fg, bg, bits)
}

/// Interpolate between two u8 channel values linearly.
fn lerp_u8(a: u8, b: u8, t: f64) -> u8 {
    (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8
}

/// Trail style faded by `progress` (0 = just entered trail, 1 = about to expire).
/// Uses t² so the bolt stays visible for most of the trail window and fades near the end.
/// `positive` selects the cyan palette to match positive-strike impact colors.
fn trail_style(progress: f64, positive: bool) -> (Rgb8, Rgb8) {
    let t = progress.clamp(0.0, 1.0).powi(2);
    if positive {
        let fg = Rgb8::new(lerp_u8(80, 4, t), lerp_u8(175, 10, t), lerp_u8(195, 10, t));
        let bg = Rgb8::new(lerp_u8(10, 1, t), lerp_u8(55, 3, t), lerp_u8(65, 3, t));
        (fg, bg)
    } else {
        let fg = Rgb8::new(lerp_u8(150, 10, t), lerp_u8(130, 8, t), lerp_u8(38, 2, t));
        let bg = Rgb8::new(lerp_u8(55, 4, t), lerp_u8(50, 3, t), lerp_u8(14, 1, t));
        (fg, bg)
    }
}

fn raster_lightning(cells: &mut [RasterCell], app: &App, bounds: Bounds, width: u16, height: u16) {
    let modes = app.layers.mode_state();
    if !modes.has_any(LayerId::Lightning) || app.lightning_strikes.is_empty() {
        return;
    }

    let in_braille = modes.has(RenderMode::Braille, LayerId::Lightning);
    let in_color = modes.has(RenderMode::Color, LayerId::Lightning);
    let in_text = modes.has(RenderMode::Text, LayerId::Lightning);
    if !in_braille && !in_color && !in_text {
        return;
    }

    // On non-live frames only show strikes that arrived near the displayed
    // radar frame's capture time (within ±5 min).  Live mode shows all strikes.
    let frame_ts_filter: Option<i64> = if app.playback_mode != PlaybackMode::Live {
        app.timestamps.get(app.frame_index).copied()
    } else {
        None
    };
    let now = std::time::Instant::now();
    let now_sys = std::time::SystemTime::now();
    let strike_matches_frame = |arrived: &std::time::Instant| -> bool {
        let Some(frame_ts) = frame_ts_filter else {
            return true;
        };
        let age = now.duration_since(*arrived);
        let strike_sys = now_sys.checked_sub(age).unwrap_or(now_sys);
        let strike_unix = strike_sys
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        (strike_unix - frame_ts).abs() < 300
    };
    if frame_ts_filter.is_some()
        && !app
            .lightning_strikes
            .iter()
            .any(|(_, a, _)| strike_matches_frame(a))
    {
        return;
    }

    let trail_dur_ms = u64::from(app.layers.lightning_trail_minutes) * 60_000;
    let trail_dur = std::time::Duration::from_millis(trail_dur_ms);

    let sub_width = u32::from(width) * 2;
    let sub_height = u32::from(height) * 4;
    let num_cells = usize::from(width) * usize::from(height);

    // Helper: map a WorldPoint to (sx, sy, cell_idx), or None if out of view.
    let to_cell = |point: &WorldPoint| -> Option<(u32, u32, usize)> {
        if point.x < bounds.min_x
            || point.x > bounds.max_x
            || point.y < bounds.min_y
            || point.y > bounds.max_y
        {
            return None;
        }
        let sx = ((point.x - bounds.min_x) / bounds.width().max(f64::EPSILON)
            * f64::from(sub_width))
        .floor()
        .clamp(0.0, f64::from(sub_width.saturating_sub(1))) as u32;
        let sy = ((point.y - bounds.min_y) / bounds.height().max(f64::EPSILON)
            * f64::from(sub_height))
        .floor()
        .clamp(0.0, f64::from(sub_height.saturating_sub(1))) as u32;
        let idx = (sy / 4) as usize * usize::from(width) + (sx / 2) as usize;
        (idx < num_cells).then_some((sx, sy, idx))
    };

    // ── Pass 1: Impact ────────────────────────────────────────────────────
    // Newest-first, one animation per cell.  Impact clears the cell entirely
    // so the bolt shape is unambiguous.
    let mut impact_cells = vec![false; num_cells];

    for (point, arrived, pol) in app
        .lightning_strikes
        .iter()
        .rev()
        .filter(|(_, a, _)| strike_matches_frame(a))
    {
        let elapsed = now.duration_since(*arrived);
        if elapsed >= trail_dur {
            continue;
        }
        let elapsed_ms = elapsed.as_millis() as u32;
        if elapsed_ms >= LIGHTNING_IMPACT_MS {
            continue;
        }
        let Some((sx, sy, cell_idx)) = to_cell(point) else {
            continue;
        };
        if impact_cells[cell_idx] {
            continue;
        }
        impact_cells[cell_idx] = true;

        let positive = *pol > 0;
        let text_glyph = if positive { '*' } else { '+' };
        let frame = elapsed_ms / 100;
        let (fg, bg, bits) = impact_frame_style(frame, positive);

        if in_braille {
            if let Some(cell) = cells.get_mut(cell_idx) {
                // Replace clears radar/border dots — bolt shape is fully visible.
                cell.bits = bits;
                cell.color = Some(fg);
                cell.intensity = 200;
            }
        }
        if in_color {
            set_subcell_bg(cells, width, sx, sy, bg);
        }
        if in_text {
            if let Some(cell) = cells.get_mut(cell_idx) {
                cell.glyph = Some(text_glyph);
                cell.color = Some(fg);
            }
        }
    }

    // ── Pass 2: Trail — float heatmap, auto-range normalised ─────────────
    // Accumulate per-strike colours into a f32 buffer so u8 saturation never
    // clips densely overlapping cells before we know the global maximum.
    // After accumulation one pass finds the peak and scales the entire visible
    // range so the densest cell lands at TRAIL_TARGET.  Sparse scenes show
    // strikes at their natural dim colour; busy scenes use the full range up
    // to the cap, with relative density preserved throughout.
    //
    // Impact cells are excluded from the heatmap — the animated bolt owns
    // those cells.  Text glyph is newest-wins (impact cells skipped too).

    const TRAIL_TARGET: f32 = 85.0; // max brightness any trail cell reaches

    let mut heatmap = vec![0.0_f32; num_cells * 3]; // r/g/b interleaved
    let mut trail_touched = vec![false; num_cells];
    let mut trail_cell_list: Vec<usize> = Vec::new();
    let mut text_cells = vec![false; num_cells];

    for (point, arrived, pol) in app
        .lightning_strikes
        .iter()
        .rev()
        .filter(|(_, a, _)| strike_matches_frame(a))
    {
        let elapsed = now.duration_since(*arrived);
        if elapsed >= trail_dur {
            continue;
        }
        let elapsed_ms = elapsed.as_millis() as u32;
        if elapsed_ms < LIGHTNING_IMPACT_MS {
            continue;
        }
        let Some((_sx, _sy, cell_idx)) = to_cell(point) else {
            continue;
        };
        // Impact cells are fully owned by pass 1.
        if impact_cells[cell_idx] {
            continue;
        }

        let positive = *pol > 0;
        let text_glyph = if positive { '*' } else { '+' };
        let trail_progress = (elapsed_ms as f64 - f64::from(LIGHTNING_IMPACT_MS))
            / (trail_dur_ms as f64 - f64::from(LIGHTNING_IMPACT_MS)).max(1.0);
        let (fg, bg) = trail_style(trail_progress, positive);

        if (in_braille || in_color) && (bg.r | bg.g | bg.b) > 0 {
            heatmap[cell_idx * 3] += bg.r as f32;
            heatmap[cell_idx * 3 + 1] += bg.g as f32;
            heatmap[cell_idx * 3 + 2] += bg.b as f32;
            if !trail_touched[cell_idx] {
                trail_touched[cell_idx] = true;
                trail_cell_list.push(cell_idx);
            }
        }

        if in_text && !text_cells[cell_idx] {
            text_cells[cell_idx] = true;
            if let Some(cell) = cells.get_mut(cell_idx) {
                if cell.glyph.is_none() {
                    cell.glyph = Some(text_glyph);
                    cell.color = Some(fg);
                }
            }
        }
    }

    // Normalise and write heatmap to cells.
    if !trail_cell_list.is_empty() {
        let peak = trail_cell_list
            .iter()
            .map(|&idx| {
                heatmap[idx * 3]
                    .max(heatmap[idx * 3 + 1])
                    .max(heatmap[idx * 3 + 2])
            })
            .fold(0.0_f32, f32::max);

        if peak > 0.0 {
            // Global scale: bring the hottest cell down to TRAIL_TARGET.
            // Never scale up — sparse scenes keep their natural dim colour.
            let scale = (TRAIL_TARGET / peak).min(1.0);

            // Minimum floor: even a single nearly-expired lonely strike must
            // remain faintly visible.  Applied per-cell after the global
            // scale, and only when the scaled peak would drop below the floor.
            // The boost is proportional (same factor on all channels) so the
            // hue is preserved.
            const TRAIL_FLOOR: f32 = 5.0; // minimum peak-channel brightness

            for &idx in &trail_cell_list {
                let r = heatmap[idx * 3];
                let g = heatmap[idx * 3 + 1];
                let b = heatmap[idx * 3 + 2];
                let raw_peak = r.max(g).max(b);
                if raw_peak == 0.0 {
                    continue;
                }

                // Choose the effective scale: global normalisation or the
                // minimum floor boost, whichever produces a brighter result.
                let scaled_peak = raw_peak * scale;
                let eff = if scaled_peak < TRAIL_FLOOR {
                    TRAIL_FLOOR / raw_peak
                } else {
                    scale
                };

                let fr = (r * eff).round() as u8;
                let fg_c = (g * eff).round() as u8;
                let fb = (b * eff).round() as u8;

                if let Some(cell) = cells.get_mut(idx) {
                    cell.bg = Some(Rgb8::new(fr, fg_c, fb));
                }
            }
        }
    }
}

fn raster_obs_placeholders(
    cells: &mut [RasterCell],
    app: &App,
    bounds: Bounds,
    width: u16,
    height: u16,
) {
    const DIM: Rgb8 = Rgb8::new(60, 60, 60);

    let sub_width = u32::from(width) * 2;
    let sub_height = u32::from(height) * 4;
    let obs_mode = obs_display_mode(app.viewport.zoom);

    for &(lat, lon) in CAPITALS.iter().chain(MAJOR_CITIES.iter()) {
        if !obs_point_visible(lat, lon, obs_mode) {
            continue;
        }
        let world = lat_lon_to_world(lat, lon);
        if world.x < bounds.min_x
            || world.x > bounds.max_x
            || world.y < bounds.min_y
            || world.y > bounds.max_y
        {
            continue;
        }
        let sx = ((world.x - bounds.min_x) / bounds.width().max(f64::EPSILON)
            * f64::from(sub_width))
        .floor()
        .clamp(0.0, f64::from(sub_width.saturating_sub(1))) as u32;
        let sy = ((world.y - bounds.min_y) / bounds.height().max(f64::EPSILON)
            * f64::from(sub_height))
        .floor()
        .clamp(0.0, f64::from(sub_height.saturating_sub(1))) as u32;
        write_obs_str(cells, width, sx, sy, "·", DIM, false);
    }
}

fn raster_observations(
    cells: &mut [RasterCell],
    app: &App,
    bounds: Bounds,
    width: u16,
    height: u16,
) {
    let modes = app.layers.mode_state();

    let obs_layer_active = crate::layers::LayerRegistry::ORDER
        .iter()
        .any(|id| (id.is_observation()) && modes.has(RenderMode::Text, *id));

    if app.obs_cache.is_none() {
        if obs_layer_active && app.has_obs_task() {
            raster_obs_placeholders(cells, app, bounds, width, height);
        }
        return;
    }
    let Some(obs) = app.obs_cache.as_ref() else {
        return;
    };

    let sub_width = u32::from(width) * 2;
    let sub_height = u32::from(height) * 4;
    let show_names = show_station_names(app.viewport.zoom);
    let obs_mode = obs_display_mode(app.viewport.zoom);

    for id in crate::layers::LayerRegistry::ORDER {
        if !id.is_observation() {
            continue;
        }
        let Some(property) = id.observation_property() else {
            continue;
        };

        if !modes.has(RenderMode::Text, id) {
            continue;
        }

        // Two passes so capital-adjacent stations are placed first and claim
        // their cells — they're then the last to be dropped by the declutter as
        // the view gets denser.
        for capitals_first in [true, false] {
            for point in &obs.points {
                let (lat, lon) = (point.point.lat, point.point.lon);
                let is_capital = is_capital_station(lat, lon);
                if is_capital != capitals_first {
                    continue;
                }
                if !obs_point_visible(lat, lon, obs_mode) {
                    continue;
                }

                let world = point.world;
                if world.x < bounds.min_x
                    || world.x > bounds.max_x
                    || world.y < bounds.min_y
                    || world.y > bounds.max_y
                {
                    continue;
                }

                let sx = ((world.x - bounds.min_x) / bounds.width().max(f64::EPSILON)
                    * f64::from(sub_width))
                .floor()
                .clamp(0.0, f64::from(sub_width.saturating_sub(1))) as u32;
                let sy = ((world.y - bounds.min_y) / bounds.height().max(f64::EPSILON)
                    * f64::from(sub_height))
                .floor()
                .clamp(0.0, f64::from(sub_height.saturating_sub(1)))
                    as u32;

                // 2-D proximity guard: skip this label if any existing glyph
                // occupies the rectangle ±radius cols × ±1 row around it.
                // Capital-adjacent stations (first pass) get a tiny radius (2)
                // so they are never crowded out by other stations.  Non-capital
                // stations (second pass) use the zoom-adaptive radius.
                let cell_x = (sx / 2) as usize;
                let cell_y = (sy / 4) as usize;
                let r = if capitals_first {
                    2
                } else {
                    declutter_radius(app.viewport.zoom)
                };
                let col_start = cell_x.saturating_sub(r);
                let col_end = (cell_x + r + 1).min(usize::from(width));
                let row_start = cell_y.saturating_sub(1);
                let row_end = (cell_y + 2).min(usize::from(height));
                if (row_start..row_end).any(|ry| {
                    let rb = ry * usize::from(width);
                    (col_start..col_end)
                        .any(|cx| cells.get(rb + cx).is_some_and(|c| c.glyph.is_some()))
                }) {
                    continue;
                }

                let (text, color) = obs_display_text(property, point);
                write_obs_str(cells, width, sx, sy, &text, color, false);
            }
        }
    }

    if show_names {
        raster_capital_names(cells, bounds, width, height);
    }

    // Placeholder dots for European capitals whose bbox query returned no
    // reporting station.  Drawn only when the capital is in viewport AND
    // nothing has already been placed at that screen position.
    const CAPITAL_NO_DATA: Rgb8 = Rgb8::new(75, 75, 75);
    for &(clat, clon) in CAPITALS {
        let world = lat_lon_to_world(clat, clon);
        if world.x < bounds.min_x
            || world.x > bounds.max_x
            || world.y < bounds.min_y
            || world.y > bounds.max_y
        {
            continue;
        }
        let sx = ((world.x - bounds.min_x) / bounds.width().max(f64::EPSILON)
            * f64::from(sub_width))
        .floor()
        .clamp(0.0, f64::from(sub_width.saturating_sub(1))) as u32;
        let sy = ((world.y - bounds.min_y) / bounds.height().max(f64::EPSILON)
            * f64::from(sub_height))
        .floor()
        .clamp(0.0, f64::from(sub_height.saturating_sub(1))) as u32;
        let idx = (sy / 4) as usize * usize::from(width) + (sx / 2) as usize;
        if cells.get(idx).is_some_and(|c| c.glyph.is_none()) {
            write_obs_str(cells, width, sx, sy, "·", CAPITAL_NO_DATA, false);
        }
    }
}

/// One band of an observation colour ramp: `max` is the band's exclusive
/// upper bound (the last band is open-ended). Shared with the legend
/// (CP-2/CP-3), which enumerates this table instead of restating thresholds.
struct ObsBand {
    max: f64,
    color: Rgb8,
}

/// A full colour ramp for one `ObservationProperty`, plus the unit label the
/// legend displays alongside it so CP-3 need not hardcode a unit list.
struct ObsScale {
    unit: &'static str,
    bands: &'static [ObsBand],
}

// Temperature: cold blue → teal → yellow-green → amber → hot orange. Follows
// standard synoptic weather-map convention.
const TEMPERATURE_BANDS: &[ObsBand] = &[
    ObsBand {
        max: -20.0,
        color: Rgb8::new(80, 110, 210),
    },
    ObsBand {
        max: -10.0,
        color: Rgb8::new(110, 155, 235),
    },
    ObsBand {
        max: 0.0,
        color: Rgb8::new(140, 195, 240),
    },
    ObsBand {
        max: 10.0,
        color: Rgb8::new(100, 205, 185),
    },
    ObsBand {
        max: 20.0,
        color: Rgb8::new(165, 215, 120),
    },
    ObsBand {
        max: 30.0,
        color: Rgb8::new(235, 185, 65),
    },
    ObsBand {
        max: f64::INFINITY,
        color: Rgb8::new(230, 100, 60),
    },
];

// Wind: Beaufort-inspired calm-gray → light-blue → green → yellow → orange → red.
const WIND_SPEED_BANDS: &[ObsBand] = &[
    ObsBand {
        max: 1.0,
        color: Rgb8::new(165, 185, 195),
    },
    ObsBand {
        max: 3.0,
        color: Rgb8::new(130, 190, 235),
    },
    ObsBand {
        max: 8.0,
        color: Rgb8::new(110, 210, 150),
    },
    ObsBand {
        max: 14.0,
        color: Rgb8::new(220, 205, 80),
    },
    ObsBand {
        max: 20.0,
        color: Rgb8::new(230, 140, 60),
    },
    ObsBand {
        max: f64::INFINITY,
        color: Rgb8::new(215, 80, 80),
    },
];

// Humidity: dry amber → comfortable green → moist blue.
const HUMIDITY_BANDS: &[ObsBand] = &[
    ObsBand {
        max: 30.0,
        color: Rgb8::new(205, 170, 75),
    },
    ObsBand {
        max: 50.0,
        color: Rgb8::new(195, 210, 120),
    },
    ObsBand {
        max: 70.0,
        color: Rgb8::new(130, 200, 155),
    },
    ObsBand {
        max: 85.0,
        color: Rgb8::new(100, 175, 220),
    },
    ObsBand {
        max: f64::INFINITY,
        color: Rgb8::new(70, 130, 215),
    },
];

// Pressure: low = stormy red, normal = neutral, high = fair-weather blue.
const PRESSURE_BANDS: &[ObsBand] = &[
    ObsBand {
        max: 980.0,
        color: Rgb8::new(215, 85, 85),
    },
    ObsBand {
        max: 1000.0,
        color: Rgb8::new(200, 145, 130),
    },
    ObsBand {
        max: 1015.0,
        color: Rgb8::new(165, 185, 205),
    },
    ObsBand {
        max: 1030.0,
        color: Rgb8::new(110, 170, 220),
    },
    ObsBand {
        max: f64::INFINITY,
        color: Rgb8::new(70, 135, 215),
    },
];

/// Look up the colour ramp and unit label for an `ObservationProperty`.
/// Enumerable by the future legend so it never needs a second hardcoded copy.
fn obs_scale(property: ObservationProperty) -> ObsScale {
    match property {
        ObservationProperty::Temperature => ObsScale {
            unit: "°C",
            bands: TEMPERATURE_BANDS,
        },
        ObservationProperty::WindSpeed => ObsScale {
            unit: "m/s",
            bands: WIND_SPEED_BANDS,
        },
        ObservationProperty::Humidity => ObsScale {
            unit: "%",
            bands: HUMIDITY_BANDS,
        },
        ObservationProperty::Pressure => ObsScale {
            unit: "hPa",
            bands: PRESSURE_BANDS,
        },
    }
}

fn obs_color(property: ObservationProperty, value: Option<f64>) -> Rgb8 {
    let Some(v) = value else {
        return Rgb8::GRAY;
    };
    let scale = obs_scale(property);
    scale
        .bands
        .iter()
        .find(|band| v < band.max)
        .map(|band| band.color)
        .unwrap_or_else(|| scale.bands[scale.bands.len() - 1].color)
}

fn set_subcell(cells: &mut [RasterCell], width: u16, sx: u32, sy: u32, color: Rgb8, intensity: u8) {
    let cell_x = sx / 2;
    let cell_y = sy / 4;
    let index = cell_y as usize * usize::from(width) + cell_x as usize;
    let Some(cell) = cells.get_mut(index) else {
        return;
    };
    cell.bits |= braille_bit(sx % 2, sy % 4);
    if intensity >= cell.intensity {
        cell.color = Some(color);
        cell.intensity = intensity;
    }
}

fn set_subcell_bg(cells: &mut [RasterCell], width: u16, sx: u32, sy: u32, color: Rgb8) {
    let cell_x = sx / 2;
    let cell_y = sy / 4;
    let index = cell_y as usize * usize::from(width) + cell_x as usize;
    let Some(cell) = cells.get_mut(index) else {
        return;
    };
    cell.bg = Some(color);
}

fn set_subcell_glyph(
    cells: &mut [RasterCell],
    width: u16,
    sx: u32,
    sy: u32,
    glyph: char,
    color: Rgb8,
) {
    let cell_x = sx / 2;
    let cell_y = sy / 4;
    let index = cell_y as usize * usize::from(width) + cell_x as usize;
    let Some(cell) = cells.get_mut(index) else {
        return;
    };
    cell.glyph = Some(glyph);
    cell.color = Some(color);
}

fn wind_arrow(direction_deg: f64) -> char {
    const ARROWS: [char; 8] = ['↑', '↗', '→', '↘', '↓', '↙', '←', '↖'];
    let idx = ((direction_deg + 22.5) % 360.0 / 45.0) as usize;
    ARROWS[idx.min(7)]
}

fn write_obs_str(
    cells: &mut [RasterCell],
    width: u16,
    sx: u32,
    sy: u32,
    text: &str,
    color: Rgb8,
    italic: bool,
) {
    let cell_x = (sx / 2) as usize;
    let cell_y = (sy / 4) as usize;
    let base = cell_y * usize::from(width) + cell_x;
    for (i, ch) in text.chars().enumerate() {
        let idx = base + i;
        let Some(cell) = cells.get_mut(idx) else {
            break;
        };
        cell.glyph = Some(ch);
        cell.color = Some(color);
        if italic {
            cell.modifier = Modifier::ITALIC;
        }
    }
}

fn obs_display_text(property: ObservationProperty, point: &ObservationPoint) -> (String, Rgb8) {
    // Dim but visible placeholder used when a value hasn't arrived yet.
    const PLACEHOLDER: Rgb8 = Rgb8::new(72, 72, 72);
    // Neutral gray for calm wind (no meaningful color ramp at ~0 m/s).
    const CALM: Rgb8 = Rgb8::new(155, 175, 185);

    match property {
        ObservationProperty::WindSpeed => {
            match (point.wind_direction, point.wind_speed) {
                // Complete placeholder — data not yet loaded.
                (None, None) => ("·".to_string(), PLACEHOLDER),
                // Calm — direction irrelevant below ~0.5 m/s.
                (_, Some(s)) if s < 0.5 => ("○".to_string(), CALM),
                // Direction known, speed known — primary display: arrow + m/s.
                (Some(d), Some(s)) => {
                    let arrow = wind_arrow(d);
                    let spd = s.round() as u32;
                    (format!("{arrow}{spd}"), obs_color(property, Some(s)))
                }
                // Direction known, speed missing.
                (Some(d), None) => (format!("{}·", wind_arrow(d)), PLACEHOLDER),
                // Speed known, direction missing (variable/unrecorded).
                (None, Some(s)) => {
                    let spd = s.round() as u32;
                    (format!("~{spd}"), obs_color(property, Some(s)))
                }
            }
        }
        ObservationProperty::Temperature => match point.temperature {
            Some(t) => (format!("{:.0}°", t), obs_color(property, Some(t))),
            None => ("·".to_string(), PLACEHOLDER),
        },
        ObservationProperty::Humidity => match point.humidity {
            Some(h) => (format!("{:.0}%", h), obs_color(property, Some(h))),
            None => ("·".to_string(), PLACEHOLDER),
        },
        ObservationProperty::Pressure => match point.pressure {
            Some(p) => (format!("{:.0}", p), obs_color(property, Some(p))),
            None => ("·".to_string(), PLACEHOLDER),
        },
    }
}

fn braille_bit(x: u32, y: u32) -> u8 {
    match (x, y) {
        (0, 0) => 0x01,
        (0, 1) => 0x02,
        (0, 2) => 0x04,
        (0, 3) => 0x40,
        (1, 0) => 0x08,
        (1, 1) => 0x10,
        (1, 2) => 0x20,
        (1, 3) => 0x80,
        _ => 0,
    }
}

/// Blit the rasterised cell grid straight into the terminal buffer.
///
/// The grid already holds exactly one entry per terminal cell, so going via
/// `Paragraph` would mean allocating a `String` and a `Vec<Span>` per row,
/// every frame, only for ratatui to copy the glyphs back out into this same
/// buffer.  Writing the cells directly makes the map path allocation-free.
fn blit_cells(cells: &[RasterCell], area: Rect, width: u16, height: u16, buf: &mut Buffer) {
    let w = usize::from(width);
    for y in 0..height {
        let row = &cells[usize::from(y) * w..usize::from(y) * w + w];
        for (x, cell) in row.iter().enumerate() {
            let Ok(x) = u16::try_from(x) else { continue };
            let Some(buf_cell) = buf.cell_mut((area.x + x, area.y + y)) else {
                continue;
            };
            let packed = cell.packed();
            let mut style = Style::default();
            if let Some(fg) = packed.fg {
                style = style.fg(to_terminal_color(fg));
            }
            if let Some(bg) = packed.bg {
                style = style.bg(to_terminal_color(bg));
            }
            buf_cell.set_char(raster_glyph(packed));
            buf_cell.set_style(style.add_modifier(packed.modifier));
        }
    }
}

fn raster_glyph(cell: PackedBrailleCell) -> char {
    if let Some(glyph) = cell.glyph {
        return glyph;
    }
    if cell.bits == 0 {
        ' '
    } else {
        char::from_u32(0x2800 + u32::from(cell.bits)).unwrap_or(' ')
    }
}

fn to_terminal_color(color: Rgb8) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

fn border_line_color(kind: BorderLineKind) -> Rgb8 {
    match kind {
        BorderLineKind::Region => Rgb8::DARK_GRAY,
        BorderLineKind::Road => Rgb8::AMBER,
        BorderLineKind::Country => Rgb8::GRAY,
    }
}

#[cfg(test)]
fn border_mask_for_view(
    borders: &crate::layers::BorderLayer,
    bounds: Bounds,
    width: u16,
    height: u16,
    stamp: BorderMaskStamp,
) -> BorderMask {
    let sub_width = u32::from(width.max(1)) * 2;
    let sub_height = u32::from(height.max(1)) * 4;
    let cells = compute_mask_cells(borders, bounds, sub_width, sub_height, stamp);
    let marks = cells
        .iter()
        .enumerate()
        .filter_map(|(index, kind)| {
            kind.map(|kind| BorderMaskPoint {
                sx: index as u32 % sub_width,
                sy: index as u32 / sub_width,
                kind,
            })
        })
        .collect();
    BorderMask {
        cells,
        marks,
        // Tests don't compare the center field, but it must be
        // populated for the struct to compile.
        center: crate::geo::WorldPoint { x: 0.5, y: 0.5 },
    }
}

fn should_draw_border_line(kind: BorderLineKind, stamp: BorderMaskStamp) -> bool {
    match kind {
        BorderLineKind::Country => true,
        BorderLineKind::Region => stamp.show_regions,
        BorderLineKind::Road => stamp.show_roads,
    }
}

/// Bresenham-like subcell coordinates along the segment (x1,y1)→(x2,y2).
/// Produces the same sequence of subcell addresses that
/// `mark_border_segment` would write to, without writing anywhere.
fn bresenham_cells(x1: i32, y1: i32, x2: i32, y2: i32) -> impl Iterator<Item = (i32, i32)> {
    let steps = (x2 - x1).abs().max((y2 - y1).abs()).max(1);
    (0..=steps).map(move |step| {
        let t = step as f64 / steps as f64;
        let x = (x1 as f64 + (x2 - x1) as f64 * t).round() as i32;
        let y = (y1 as f64 + (y2 - y1) as f64 * t).round() as i32;
        (x, y)
    })
}

#[allow(clippy::too_many_arguments)] // 8 coordinates needed for line drawing
fn mark_border_segment(
    mask: &mut [Option<BorderLineKind>],
    bounds: Bounds,
    sub_width: u32,
    sub_height: u32,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    kind: BorderLineKind,
) {
    let sx1 = world_to_subcell_axis(x1, bounds.min_x, bounds.width(), sub_width);
    let sy1 = world_to_subcell_axis(y1, bounds.min_y, bounds.height(), sub_height);
    let sx2 = world_to_subcell_axis(x2, bounds.min_x, bounds.width(), sub_width);
    let sy2 = world_to_subcell_axis(y2, bounds.min_y, bounds.height(), sub_height);
    for (sx, sy) in bresenham_cells(sx1, sy1, sx2, sy2) {
        if sx < 0 || sy < 0 || sx >= sub_width as i32 || sy >= sub_height as i32 {
            continue;
        }
        let index = sy as usize * sub_width as usize + sx as usize;
        mask[index] = Some(stronger_border_kind(mask[index], kind));
    }
}

fn stronger_border_kind(
    existing: Option<BorderLineKind>,
    candidate: BorderLineKind,
) -> BorderLineKind {
    // Draw order (bottom → top): Region, Road, Country.  The
    // last-drawn layer keeps its colour, so the mask must remember
    // the topmost kind for each subcell.
    match (existing, candidate) {
        (_, BorderLineKind::Country) | (Some(BorderLineKind::Country), _) => {
            BorderLineKind::Country
        }
        (_, BorderLineKind::Road) | (Some(BorderLineKind::Road), _) => BorderLineKind::Road,
        _ => BorderLineKind::Region,
    }
}

/// Subcell-space offset between the mask's stored center and the
/// current viewport center.  Used to shift existing mask marks rather
/// than recomputing the full mask on every pan.
fn subcell_offset(
    center: WorldPoint,
    mask_center: WorldPoint,
    bounds: &Bounds,
    sub_width: u32,
    sub_height: u32,
) -> (i32, i32) {
    let dx_world = center.x - mask_center.x;
    let dy_world = center.y - mask_center.y;
    let dx_sub = (-dx_world / bounds.width().max(f64::EPSILON) * sub_width as f64).round() as i32;
    let dy_sub = (-dy_world / bounds.height().max(f64::EPSILON) * sub_height as f64).round() as i32;
    (dx_sub, dy_sub)
}

fn world_to_subcell_axis(world: f64, min: f64, span: f64, size: u32) -> i32 {
    ((world - min) / span.max(f64::EPSILON) * f64::from(size)).floor() as i32
}

fn clipped_segment(
    bounds: Bounds,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
) -> Option<(f64, f64, f64, f64)> {
    if (x1 - x2).abs() > 0.5 {
        return None;
    }

    let dx = x2 - x1;
    let dy = y2 - y1;
    let mut entering = 0.0;
    let mut leaving = 1.0;

    for (p, q) in [
        (-dx, x1 - bounds.min_x),
        (dx, bounds.max_x - x1),
        (-dy, y1 - bounds.min_y),
        (dy, bounds.max_y - y1),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
            continue;
        }
        let t = q / p;
        if p < 0.0 {
            if t > leaving {
                return None;
            }
            entering = f64::max(entering, t);
        } else {
            if t < entering {
                return None;
            }
            leaving = f64::min(leaving, t);
        }
    }

    Some((
        x1 + entering * dx,
        y1 + entering * dy,
        x1 + leaving * dx,
        y1 + leaving * dy,
    ))
}

/// Fixed area for the left (main) layer panel, bottom-aligned with
/// 2-character left margin and 1-character bottom margin.
fn layer_area(area: Rect) -> Rect {
    let height = LayerRegistry::MAIN_ORDER.len() as u16; // all items (headers are rows too)
    let height = height.min(area.height.saturating_sub(1));
    let width = 30u16.min(area.width.saturating_sub(3));
    let y = area.y + area.height.saturating_sub(1 + height);
    Rect {
        x: area.x + 2,
        y,
        width,
        height,
    }
}

/// Area for the bottom-right legend panel, mirroring `layer_area` reflected
/// horizontally: same baseline (one row of bottom padding), two-column inset
/// from the right edge instead of the left. Unlike `layer_area`, height is
/// driven by the caller (CP-3 knows how many scale blocks fit) rather than a
/// fixed item count, so both dimensions are parameters here.
fn legend_area(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(3));
    let height = height.min(area.height.saturating_sub(1));
    let x = area.x + area.width.saturating_sub(width + 2);
    let y = area.y + area.height.saturating_sub(1 + height);
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// One colour-carrying scale that can appear on the legend. Maps 1:1 to
/// either the dBZ ramp or an `ObservationProperty` ramp, so CP-3 can pull the
/// band table + unit via `DBZ_BANDS`/`DBZ_UNIT` or `obs_scale()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegendScale {
    Dbz,
    Temperature,
    WindSpeed,
    Humidity,
    Pressure,
}

impl LegendScale {
    /// The `ObservationProperty` this scale reads bands from, if it isn't dBZ.
    fn observation_property(self) -> Option<ObservationProperty> {
        match self {
            Self::Dbz => None,
            Self::Temperature => Some(ObservationProperty::Temperature),
            Self::WindSpeed => Some(ObservationProperty::WindSpeed),
            Self::Humidity => Some(ObservationProperty::Humidity),
            Self::Pressure => Some(ObservationProperty::Pressure),
        }
    }
}

/// Which scales are currently active, in a fixed declaration order (dBZ,
/// Temperature, Wind, Humidity, Pressure). A scale is active exactly when its
/// layer owns a colour-painting render mode — dBZ requires `Radar` to own
/// `Braille` or `Color` specifically (its palette only appears there); the
/// observation scales just require their layer to own any mode at all.
/// Ordering here is declaration order only — `fitting_scales` drops from the
/// tail for degradation, keeping dBZ (declared first) and dropping
/// observation scales first.
fn active_scales(rms: &RenderModeState) -> Vec<LegendScale> {
    let mut scales = Vec::new();
    if rms.has(RenderMode::Braille, LayerId::Radar) || rms.has(RenderMode::Color, LayerId::Radar) {
        scales.push(LegendScale::Dbz);
    }
    if rms.has_any(LayerId::SurfTemp) {
        scales.push(LegendScale::Temperature);
    }
    if rms.has_any(LayerId::SurfWind) {
        scales.push(LegendScale::WindSpeed);
    }
    if rms.has_any(LayerId::SurfHumidity) {
        scales.push(LegendScale::Humidity);
    }
    if rms.has_any(LayerId::SurfPressure) {
        scales.push(LegendScale::Pressure);
    }
    scales
}

/// Abbreviated quantity name for a legend scale's header row — the unit is
/// composed alongside it (`legend_scale_data`) from the same band table CP-1
/// already reads (`DBZ_UNIT` / `ObsScale::unit`), so it is never duplicated
/// here. Header reads `"<name> / <unit>"` (a slash, not parentheses), the
/// scientific "quantity / unit" axis convention.
fn legend_scale_name(scale: LegendScale) -> &'static str {
    match scale {
        LegendScale::Dbz => "Reflect",
        LegendScale::Temperature => "Temp",
        LegendScale::WindSpeed => "Wind",
        LegendScale::Humidity => "Humid",
        LegendScale::Pressure => "Press",
    }
}

/// One band's colour and exclusive upper bound, unified to `f64` so layout
/// code doesn't care whether it came from the dBZ (`f32`) or observation
/// (`f64`) band tables.
struct LegendBand {
    max: f64,
    color: Rgb8,
}

/// Everything the legend needs to draw one scale's two-row block: its
/// composed `"<name> / <unit>"` header (row 1) and band table (row 2's bar),
/// pulled from the CP-1 tables rather than restated here.
struct LegendScaleData {
    header: String,
    bands: Vec<LegendBand>,
}

fn legend_scale_data(scale: LegendScale) -> LegendScaleData {
    match scale.observation_property() {
        None => LegendScaleData {
            header: format!(
                "{} / {}",
                legend_scale_name(scale),
                crate::providers::meteogate::DBZ_UNIT
            ),
            bands: crate::providers::meteogate::DBZ_BANDS
                .iter()
                .map(|b| LegendBand {
                    max: f64::from(b.max),
                    color: b.color,
                })
                .collect(),
        },
        Some(property) => {
            let scale_data = obs_scale(property);
            LegendScaleData {
                header: format!("{} / {}", legend_scale_name(scale), scale_data.unit),
                bands: scale_data
                    .bands
                    .iter()
                    .map(|b| LegendBand {
                        max: b.max,
                        color: b.color,
                    })
                    .collect(),
            }
        }
    }
}

/// Every band's colour repeated `LEGEND_HALF_CELLS_PER_BAND` times, giving a
/// flat sequence of half-cell colour samples for the whole bar — the input
/// `legend_bar_cells` packs two-at-a-time into terminal cells.
fn legend_bar_half_colors(bands: &[LegendBand]) -> Vec<Color> {
    bands
        .iter()
        .flat_map(|b| {
            std::iter::repeat_n(
                to_terminal_color(b.color),
                LEGEND_HALF_CELLS_PER_BAND as usize,
            )
        })
        .collect()
}

/// Pack half-cell colour samples two at a time into terminal cells using the
/// same half-block idiom as the radar timeline (`timeline_bar_spans`): a
/// cell whose two halves agree is drawn as a solid background colour; a cell
/// whose halves disagree — a band boundary — is drawn as `▌` with `fg` = the
/// left half's colour and `bg` = the right half's, instead of blending them.
/// A trailing odd half-cell (odd bands × an odd `LEGEND_HALF_CELLS_PER_BAND`)
/// pairs with itself, so it still renders as one solid cell, never a gap.
fn legend_bar_cells(half_colors: &[Color]) -> Vec<(char, Style)> {
    let mut cells = Vec::with_capacity(half_colors.len().div_ceil(2));
    let mut i = 0;
    while i < half_colors.len() {
        let left = half_colors[i];
        let right = half_colors.get(i + 1).copied().unwrap_or(left);
        if left == right {
            cells.push((' ', Style::default().bg(left)));
        } else {
            cells.push(('▌', Style::default().fg(left).bg(right)));
        }
        i += 2;
    }
    cells
}

/// Terminal-cell width of a bar with `n_bands` bands at the fixed half-cells-
/// per-band resolution — used to size the shared bar-width budget before any
/// scale's actual colours are resolved.
fn legend_bar_width_cells(n_bands: usize) -> u16 {
    (n_bands as u16 * LEGEND_HALF_CELLS_PER_BAND).div_ceil(2)
}

/// Which whole bars fit in `height` given `rows_per_bar`, dropping from the
/// TAIL of `scales` until the remainder fits. Since `active_scales` returns
/// dBZ first, dropping from the tail keeps dBZ and drops observation scales
/// first, per the fixed-priority degradation rule. If not even one bar fits,
/// the slice is empty.
fn fitting_scales(scales: &[LegendScale], rows_per_bar: u16, height: u16) -> &[LegendScale] {
    if rows_per_bar == 0 {
        return &[];
    }
    let max_bars = (height / rows_per_bar) as usize;
    let n = scales.len().min(max_bars);
    &scales[..n]
}

/// Low-end and high-end marker text for a scale's boundary-number row.
/// Derived from the band table's boundaries plus two physical-domain facts
/// no band table encodes: wind speed and humidity never go below 0, and
/// humidity is a percentage so it never exceeds 100 even though its top
/// colour band (`>= 85`) is open-ended like every other scale's. The low
/// marker anchors the left edge of the bar region (fraction 0) and the high
/// marker the right edge (fraction 1); `legend_labels` fills in the interior
/// boundary numbers between them.
fn legend_endpoints(scale: LegendScale, bands: &[LegendBand]) -> (String, String) {
    let start = match scale {
        LegendScale::Dbz => format!("<{:.0}", bands[0].max),
        LegendScale::WindSpeed | LegendScale::Humidity => "0".to_string(),
        LegendScale::Temperature | LegendScale::Pressure => format!("{:.0}", bands[0].max),
    };
    let end = if scale == LegendScale::Humidity {
        "100".to_string()
    } else {
        match bands.last() {
            Some(last) if last.max.is_finite() => format!("{:.0}", last.max),
            Some(_) if bands.len() >= 2 => {
                format!("{:.0}+", bands[bands.len() - 2].max)
            }
            _ => String::new(),
        }
    };
    (start, end)
}

/// Minimum cell gap between two boundary numbers on the top row — used to
/// derive the uniform stride in `legend_labels` so no two numbers (and no
/// number and an endpoint) ever touch or merge.
const LEGEND_LABEL_MIN_GAP: u16 = 4;

/// Evenly-spaced (uniform-stride) boundary numbers for a scale's top row.
/// Boundaries are indices `0..=n` (`n = bands.len()`), boundary `i` sitting at
/// cell `round(i / n * bar_width)` — uniform because bands are equal width.
/// The smallest stride `s >= 1` with `round(s / n * bar_width) >=
/// LEGEND_LABEL_MIN_GAP` is chosen (`s = ceil(MIN_GAP * n / bar_width)`), and
/// boundaries `0, s, 2s, …` are shown. The high end (`n`) is always shown; if
/// the last stride-multiple already lands within `LEGEND_LABEL_MIN_GAP` cells
/// of it, that multiple is dropped so the final pair isn't cramped. Index 0
/// is labelled with `legend_endpoints`' low marker, index `n` with its high
/// marker, and interior index `i` with `bands[i - 1].max` (the value at that
/// boundary).
fn legend_labels(
    scale: LegendScale,
    bands: &[LegendBand],
    bar_width: u16,
) -> Vec<(u16, String, Rgb8)> {
    let (low, high) = legend_endpoints(scale, bands);
    let n = bands.len();
    let label_at = |i: usize| -> String {
        if i == 0 {
            low.clone()
        } else if i == n {
            high.clone()
        } else {
            format!("{:.0}", bands[i - 1].max)
        }
    };
    // Tick colour: index 0 -> first band, interior/high index i -> bands[i - 1]
    // (the band whose upper bound the number marks) — same mapping as `label_at`.
    let color_at = |i: usize| -> Rgb8 {
        if i == 0 {
            bands[0].color
        } else {
            bands[i - 1].color
        }
    };
    let pos_at = |i: usize| -> u16 { (i as f64 / n as f64 * f64::from(bar_width)).round() as u16 };

    if n == 0 || bar_width == 0 {
        let fallback = Rgb8 { r: 0, g: 0, b: 0 };
        return vec![(0, low, fallback), (bar_width, high, fallback)];
    }

    let stride = (u32::from(LEGEND_LABEL_MIN_GAP) * n as u32)
        .div_ceil(u32::from(bar_width).max(1))
        .max(1) as usize;

    let mut indices: Vec<usize> = (0..=n).step_by(stride).collect();
    if let Some(&last) = indices.last() {
        if last != n {
            let last_pos = pos_at(last);
            if bar_width.saturating_sub(last_pos) < LEGEND_LABEL_MIN_GAP {
                indices.pop();
            }
            indices.push(n);
        }
    }

    indices
        .into_iter()
        .map(|i| (pos_at(i), label_at(i), color_at(i)))
        .collect()
}

/// Area for the right (options) panel, placed to the right of the main
/// panel and bottom-aligned with it.  Height is computed from the number
/// of lines the caller needs to render.
fn options_panel_area(total_area: Rect, main_area: Rect, n_lines: u16) -> Rect {
    let height = n_lines.min(total_area.height.saturating_sub(1));
    let width = 22u16.min(
        total_area
            .width
            .saturating_sub(main_area.x + main_area.width + 2),
    );
    let x = main_area.x + main_area.width + 1;
    let y = total_area.y + total_area.height.saturating_sub(1 + height);
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Order visible overlay rows deterministically before truncating to
/// `max_visible` (CP-5): running tasks before terminal ones, then
/// oldest-first by `started_at` so a row doesn't reshuffle every time its
/// fraction updates — the longest-waiting task stays put. Ties (equal on
/// both keys) break on `id` so the order is total and never flaky.
///
/// NOT in this checkpoint: a "user-initiated before background" tier — it
/// discriminates on the `Geocode`/`Location` kinds CP-6 introduces (spec
/// change-log 2026-07-21).
fn sort_visible_tasks(mut tasks: Vec<&ActiveTask>) -> Vec<&ActiveTask> {
    tasks.sort_by(|a, b| {
        let a_running = a.state == TaskState::Running;
        let b_running = b.state == TaskState::Running;
        b_running
            .cmp(&a_running) // running (true) sorts before terminal (false)
            .then(a.started_at.cmp(&b.started_at)) // oldest first
            .then(a.id.cmp(&b.id)) // total, stable tie-break
    });
    tasks
}

/// Cap on simultaneously visible rows in the task-progress overlay, shared
/// with `task_queue_reserved_rows` so the legend's reserved-space budget can
/// never drift from what `render_task_queue` actually draws.
const MAX_VISIBLE_TASKS: usize = 8;

/// Render task progress overlay in the top-right corner.
fn render_task_queue(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App, now: Instant) {
    if app.active_tasks.is_empty() {
        return;
    }

    let bar_chars = 12usize;

    // CP-4's visibility threshold filters first, then CP-5's deterministic
    // sort, then truncate — a young task must not win a slot over an older
    // visible one purely by vec position, and an all-invisible panel must
    // render nothing (early return below covers that case).
    let visible: Vec<&ActiveTask> = app
        .active_tasks
        .iter()
        .filter(|t| t.is_visible(now))
        .collect();
    let ordered = sort_visible_tasks(visible);
    let rows: Vec<&ActiveTask> = ordered.into_iter().take(MAX_VISIBLE_TASKS).collect();
    if rows.is_empty() {
        return;
    }
    let n = rows.len();

    let kind_w: usize = 8;
    let label_w: usize = 18;
    let pct_w: usize = 4;
    let status_w: usize = 3;
    let panel_w: u16 = (kind_w + 1 + label_w + 1 + bar_chars + 1 + pct_w + 1 + status_w) as u16;

    let x = area.x + area.width.saturating_sub(panel_w + 1);
    let y = area.y;
    let q_area = Rect {
        x,
        y,
        width: panel_w,
        height: n as u16,
    };

    // One shared phase for every marquee this frame — they need to move,
    // not stay in lockstep forever, but per-task offsets aren't worth the
    // complexity for a handful of rows.
    let marquee_phase = marquee_phase(MARQUEE_PERIOD);

    let mut lines: Vec<TextLine<'static>> = Vec::with_capacity(n);
    for task in &rows {
        let color = task.kind.color();

        let kind = format!("{:>kw$}", task.kind.label(), kw = kind_w);
        let kind_sp = Span::styled(
            kind,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        );

        let label_raw = if task.label.len() > label_w {
            let mut s = String::with_capacity(label_w);
            s.push_str(&task.label[..label_w.saturating_sub(1)]);
            s.push('…');
            s
        } else {
            let mut s = task.label.clone();
            while s.len() < label_w {
                s.push(' ');
            }
            s
        };
        let label_sp = Span::raw(format!(" {label_raw}"));

        let (bar_str, pct_str) = match task.fraction {
            Some(_) => (
                braille_bar(task.display_fraction, bar_chars),
                format!("{:>3.0}%", task.display_fraction * 100.0),
            ),
            // No measurable progress: a marquee, and no faked percentage.
            None => (
                braille_marquee(marquee_phase, bar_chars),
                format!("{:>4}", "···"),
            ),
        };
        let bar_sp = Span::styled(format!(" {bar_str}"), Style::default().fg(color));
        let pct_sp = Span::styled(pct_str, Style::default().fg(color));

        let status_sp = match task.state {
            TaskState::Completed => Span::styled(" ✓", Color::Green),
            TaskState::Error => Span::styled(" ✗", Color::Red),
            _ => Span::raw("  "),
        };

        lines.push(TextLine::from(vec![
            kind_sp, label_sp, bar_sp, pct_sp, status_sp,
        ]));
    }

    frame.render_widget(Clear, q_area);
    frame.render_widget(Paragraph::new(lines), q_area);
}

/// Saturated colour for each render mode.
fn mode_color(mode: RenderMode) -> Color {
    match mode {
        RenderMode::Braille => Color::Rgb(255, 210, 0),
        RenderMode::Color => Color::Rgb(210, 40, 255),
        RenderMode::Text => Color::Rgb(40, 220, 80),
    }
}

/// Map colour for a geographic layer — mirrors the border colour on the map.
fn geo_layer_color(id: LayerId) -> Color {
    match id {
        LayerId::RegionBorders => Color::Rgb(80, 80, 80),
        LayerId::MajorRoads => Color::Rgb(255, 191, 0),
        _ => Color::Rgb(128, 128, 128), // Countries and fallback
    }
}

/// Indicator style for a rendered single layer in the main list.
/// `selected` = BOLD — BOLD marks the cursor, not the active mode.
fn primary_mode_style(modes: &RenderModeState, id: LayerId, selected: bool) -> Style {
    let color = if modes.has(RenderMode::Braille, id) {
        mode_color(RenderMode::Braille)
    } else if modes.has(RenderMode::Color, id) {
        mode_color(RenderMode::Color)
    } else if modes.has(RenderMode::Text, id) {
        mode_color(RenderMode::Text)
    } else {
        Color::DarkGray
    };
    let s = Style::default().fg(color);
    if selected {
        s.add_modifier(Modifier::BOLD)
    } else {
        s
    }
}

/// Same as `primary_mode_style` but aggregates across a group's children.
fn group_mode_style(modes: &RenderModeState, children: &[LayerId], selected: bool) -> Style {
    let color = if children
        .iter()
        .any(|id| modes.has(RenderMode::Braille, *id))
    {
        mode_color(RenderMode::Braille)
    } else if children.iter().any(|id| modes.has(RenderMode::Color, *id)) {
        mode_color(RenderMode::Color)
    } else if children.iter().any(|id| modes.has(RenderMode::Text, *id)) {
        mode_color(RenderMode::Text)
    } else {
        Color::DarkGray
    };
    let s = Style::default().fg(color);
    if selected {
        s.add_modifier(Modifier::BOLD)
    } else {
        s
    }
}

/// Style for an active option in the right panel (bold = active, unlike the
/// main list where bold = selected).
fn option_mode_style(key: &str) -> Style {
    let mode = match key {
        "braille" => RenderMode::Braille,
        "color" => RenderMode::Color,
        _ => RenderMode::Text,
    };
    Style::default()
        .fg(mode_color(mode))
        .add_modifier(Modifier::BOLD)
}

/// Apply `Modifier::DIM` to every span in a list of lines.
/// Used to passively show the layer panel when it loses focus.
/// DIM is defined by the terminal (~50 % brightness) — no colours hardcoded.
fn apply_dim(lines: Vec<TextLine<'static>>) -> Vec<TextLine<'static>> {
    lines
        .into_iter()
        .map(|line| {
            TextLine::from(
                line.spans
                    .into_iter()
                    .map(|s| Span::styled(s.content, s.style.add_modifier(Modifier::DIM)))
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

/// Half-cells every colour band occupies in the bar. Fixed and ODD so a
/// band's boundary lands mid-cell rather than on a cell edge — that mid-cell
/// landing is what produces a two-colour `▌` split instead of a hard,
/// full-cell colour change, and reads as a finer gradient at 2× the cell
/// resolution. Narrowed back from 7 to 3 — the wider bar dominated the map;
/// evenly-spaced `legend_labels` no longer need the extra width to avoid
/// merging.
const LEGEND_HALF_CELLS_PER_BAND: u16 = 3;
/// Two rows per scale: row 1 is the `name / unit` header plus the
/// fraction-positioned boundary numbers, row 2 is the gradient bar alone.
const LEGEND_ROWS_PER_BAR: u16 = 2;

/// Rows the task-progress overlay (`render_task_queue`) currently occupies
/// at the top of the map area: visible tasks, capped at the same
/// `max_visible` the overlay itself enforces. Kept as a standalone pure
/// function (not inside `render_task_queue`) so `render_legend` can shrink
/// its own vertical budget by this amount — the two panels share the
/// right-hand column on a short terminal and must never overlap — without
/// changing `render_task_queue` itself.
fn task_queue_reserved_rows(active_tasks: &[ActiveTask], now: Instant) -> u16 {
    active_tasks
        .iter()
        .filter(|t| t.is_visible(now))
        .count()
        .min(MAX_VISIBLE_TASKS) as u16
}

/// Render the bottom-right legend panel: a two-row block per active scale
/// (`active_scales`) — row 1 a `name / unit` header followed inline by a
/// sub-character half-block gradient bar, row 2 fraction-positioned boundary
/// numbers (`legend_labels`) under the bar, each drawn in its band's tick
/// colour, read from the shared CP-1 band tables. Mirrors
/// `render_layer_list`'s placement on the opposite corner. Draws nothing,
/// and reserves no area, when no colour-carrying scale is active or none
/// fits the available height.
///
/// `reserved_top_rows` is how many rows the task-progress overlay currently
/// occupies at the top of `area` (see `task_queue_reserved_rows`); the
/// legend's own available area is shrunk from the top by that amount so a
/// full task queue on a short terminal makes the legend degrade or draw
/// nothing rather than draw over the overlay. The task queue takes
/// precedence and is never moved.
fn render_legend(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    rms: &RenderModeState,
    reserved_top_rows: u16,
) {
    let scales = active_scales(rms);
    if scales.is_empty() {
        return;
    }

    // Shrink from the top by the task queue's occupied rows, keeping the
    // bottom edge (and thus the baseline `legend_area` anchors to) fixed.
    let top_shrink = reserved_top_rows.min(area.height);
    let area = Rect {
        x: area.x,
        y: area.y + top_shrink,
        width: area.width,
        height: area.height - top_shrink,
    };

    // legend_area reserves one row of bottom padding, matching layer_area.
    let available_height = area.height.saturating_sub(1);
    let fitted = fitting_scales(&scales, LEGEND_ROWS_PER_BAR, available_height);
    if fitted.is_empty() {
        return;
    }

    let data: Vec<LegendScaleData> = fitted.iter().map(|&s| legend_scale_data(s)).collect();

    // The widest bar among the fitted scales (dBZ has the most bands) sets
    // the shared width budget; rows still left-align at the same x — a
    // scale with fewer bands just has a shorter bar. The title column is
    // fixed to the widest header so every bar (and its numbers) starts at
    // the same x.
    let max_band_count = data.iter().map(|d| d.bands.len()).max().unwrap_or(0);
    let desired_bar_width = legend_bar_width_cells(max_band_count);
    let title_col_width = data
        .iter()
        .map(|d| d.header.chars().count() as u16)
        .max()
        .unwrap_or(0);
    let bar_width = desired_bar_width.min(area.width.saturating_sub(title_col_width + 1));
    if bar_width == 0 {
        return;
    }

    let panel_width = title_col_width + 1 + bar_width;
    let legend = legend_area(area, panel_width, fitted.len() as u16 * LEGEND_ROWS_PER_BAR);
    if legend.width == 0 || legend.height == 0 {
        return;
    }

    frame.render_widget(Clear, legend);
    let bar_x = (legend.x + title_col_width + 1).min(legend.x + legend.width);
    let buf = frame.buffer_mut();

    for (i, (&scale, d)) in fitted.iter().zip(data.iter()).enumerate() {
        let y0 = legend.y + i as u16 * LEGEND_ROWS_PER_BAR;
        let y1 = y0 + 1;
        if y1 >= legend.y + legend.height {
            break;
        }
        buf.set_string(legend.x, y0, &d.header, Style::default());

        let half_colors = legend_bar_half_colors(&d.bands);
        let cells = legend_bar_cells(&half_colors);
        let available_bar_cells =
            (cells.len() as u16).min(legend.width.saturating_sub(bar_x.saturating_sub(legend.x)));

        // Row 0: the gradient bar, inline with the title.
        for (cx, &(glyph, style)) in (bar_x..).zip(cells.iter().take(available_bar_cells as usize))
        {
            if let Some(cell) = buf.cell_mut((cx, y0)) {
                cell.set_style(style);
                cell.set_char(glyph);
            }
        }

        // Row 1: boundary numbers, each centred on its fraction position
        // under the bar above, drawn in its band's tick colour.
        for (pos, label, color) in legend_labels(scale, &d.bands, available_bar_cells) {
            let label_width = label.chars().count() as u16;
            let offset = pos
                .saturating_sub(label_width / 2)
                .min(available_bar_cells.saturating_sub(label_width));
            let x = bar_x + offset;
            if x + label_width <= legend.x + legend.width {
                buf.set_string(x, y1, &label, Style::default().fg(to_terminal_color(color)));
            }
        }
    }
}

fn render_layer_list(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let focused = app.layer_panel_focused;
    let modes = app.layers.mode_state();
    let dim = Style::default().fg(Color::DarkGray);

    // ── Main (left) panel ─────────────────────────────────────────────
    // Always rendered. When not focused: no selection indicators, whole panel dimmed.
    let mut left_lines: Vec<TextLine<'static>> = Vec::new();
    for (i, item) in LayerRegistry::MAIN_ORDER.iter().enumerate() {
        // BOLD + UNDERLINED = cursor. Only when panel is focused.
        let selected =
            focused && i == app.layers.selected_main_index() && !app.layers.is_in_options();
        let label_style = if selected {
            Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default()
        };

        let line: TextLine<'static> = match item {
            MainItem::Header(label) => TextLine::from(Span::styled(
                label.to_string(),
                Style::default().fg(Color::DarkGray),
            )),

            MainItem::Single(id) => {
                let status = app.layers.get_state(*id).map(|s| &s.status);
                let err_ch = match status {
                    Some(LayerStatus::Error(_)) => " !",
                    _ => "",
                };
                if id.is_simple_toggle() {
                    let enabled = app.layers.enabled(*id);
                    let locked = app.layers.get_state(*id).is_some_and(|s| s.locked);
                    if locked {
                        let style = Style::default().fg(Color::Rgb(100, 100, 100));
                        let mark = if enabled { "●" } else { "○" };
                        TextLine::from(vec![
                            Span::styled(mark, style),
                            Span::raw(" "),
                            Span::styled(format!("{}{err_ch}", id.label()), style),
                        ])
                    } else {
                        let color = geo_layer_color(*id);
                        let mark_style = {
                            let s =
                                Style::default().fg(if enabled { color } else { Color::DarkGray });
                            if selected {
                                s.add_modifier(Modifier::BOLD)
                            } else {
                                s
                            }
                        };
                        let mark = if enabled { "●" } else { "○" };
                        TextLine::from(vec![
                            Span::styled(mark, mark_style),
                            Span::raw(" "),
                            Span::styled(format!("{}{err_ch}", id.label()), label_style),
                        ])
                    }
                } else {
                    let mark_style = primary_mode_style(modes, *id, selected);
                    let mark = if modes.has_any(*id) { "●" } else { "○" };
                    TextLine::from(vec![
                        Span::styled(mark, mark_style),
                        Span::raw(" "),
                        Span::styled(format!("{}{err_ch}", id.label()), label_style),
                    ])
                }
            }

            MainItem::Group(g) => {
                let children = g.children();
                let mark_style = group_mode_style(modes, &children, selected);
                let any_active = children.iter().any(|id| modes.has_any(*id));
                let mark = if any_active { "▶" } else { "▷" };
                TextLine::from(vec![
                    Span::styled(mark, mark_style),
                    Span::raw(" "),
                    Span::styled(item.label().to_string(), label_style),
                ])
            }
        };
        left_lines.push(line);
    }

    // Dim main list when the panel is defocused OR when focus is in the options panel.
    let main_dimmed = !focused || app.layers.is_in_options();
    let left_lines = if main_dimmed {
        apply_dim(left_lines)
    } else {
        left_lines
    };

    let main_area = layer_area(area);
    // Compute max content width for sub-panel positioning, then clear and
    // render each line at its own width so the map shows through on trailing
    // cells that have no text.
    let content_w = left_lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0) as u16;
    let tight_main = Rect {
        width: content_w.min(main_area.width),
        ..main_area
    };
    for (i, line) in left_lines.into_iter().enumerate() {
        let y = main_area.y + i as u16;
        if y >= main_area.y + main_area.height {
            break;
        }
        let line_w = line
            .spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum::<usize>() as u16;
        let line_rect = Rect {
            x: main_area.x,
            y,
            width: line_w.min(main_area.width),
            height: 1,
        };
        if line_rect.width > 0 {
            frame.render_widget(Clear, line_rect);
            frame.render_widget(Paragraph::new(vec![line]), line_rect);
        }
    }

    // ── Options (right) panel ──────────────────────────────────────────
    // Only shown when the panel is focused (defocused = submenus hidden).
    if !focused {
        return;
    }
    let selected_item = LayerRegistry::MAIN_ORDER[app.layers.selected_main_index()];
    let options = app.layers.options_for_item(selected_item);
    if options.is_empty() {
        return;
    }

    // options_cursor() returns Some(i) when the options panel has focus.
    let opt_cursor = app.layers.options_cursor();
    let n_lines = 1 + options.len() as u16; // header + options

    let sub_area = options_panel_area(area, tight_main, n_lines);
    if sub_area.width == 0 || sub_area.height == 0 {
        return;
    }

    let mut panel_lines: Vec<TextLine<'static>> = vec![TextLine::from(Span::styled(
        selected_item.label().to_string(),
        Style::default().add_modifier(Modifier::BOLD),
    ))];

    for (i, (key, opt)) in options.iter().enumerate() {
        let cursor_here = opt_cursor.is_some_and(|sc| i == sc);
        // Bold = cursor in the options panel (mirrors main list convention).
        let opt_label_style = if cursor_here {
            Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default()
        };
        let mode_active_style = option_mode_style(key);

        let line = match opt {
            LayerOption::Toggle {
                label,
                value,
                has_error,
            } => {
                let mark_style = if *value { mode_active_style } else { dim };
                let mark = if *value { "●" } else { "○" };
                let err_ch = if *has_error { " !" } else { "" };
                TextLine::from(vec![
                    Span::styled(mark, mark_style),
                    Span::raw(" "),
                    Span::styled(format!("{label}{err_ch}"), opt_label_style),
                ])
            }
            LayerOption::Choice {
                label,
                value,
                options: choices,
            } => {
                let choice_label = choices.get(*value).copied().unwrap_or("");
                TextLine::from(Span::styled(
                    format!("{label}: {choice_label}"),
                    opt_label_style,
                ))
            }
            LayerOption::Range {
                label, value, unit, ..
            } => TextLine::from(vec![
                Span::styled("◦ ", dim),
                Span::styled(format!("{label}: {value} {unit}"), opt_label_style),
            ]),
        };
        panel_lines.push(line);
    }

    // Dim options panel when focus is in the main list.
    let panel_lines = if app.layers.is_in_options() {
        panel_lines
    } else {
        apply_dim(panel_lines)
    };

    // Clear and render each line at its own width — same per-line approach as
    // the main panel so the map shows through on empty trailing cells.
    for (i, line) in panel_lines.into_iter().enumerate() {
        let y = sub_area.y + i as u16;
        if y >= sub_area.y + sub_area.height {
            break;
        }
        let line_w = line
            .spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum::<usize>() as u16;
        let line_rect = Rect {
            x: sub_area.x,
            y,
            width: line_w.min(sub_area.width),
            height: 1,
        };
        if line_rect.width > 0 {
            frame.render_widget(Clear, line_rect);
            frame.render_widget(Paragraph::new(vec![line]), line_rect);
        }
    }
}

/// The footer hint strip, fitted to `width`.
///
/// Hints come from the registry ranked by how much a new user needs them, and
/// whole hints are dropped from the right until the strip fits — a narrow
/// terminal loses `history` before it loses `quit`, and never shows a hint
/// sliced down the middle.
fn footer_hint_spans(width: u16) -> Vec<Span<'static>> {
    const GAP: &str = "   ";
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for h in keys::footer_hints() {
        let text = format!(" {}{GAP}", h.label);
        let cost = h.keys.chars().count() + text.chars().count();
        if used + cost > width as usize {
            continue;
        }
        used += cost;
        spans.push(key_span(h.keys));
        spans.push(desc_span(&text));
    }
    spans
}

/// The `/` prompt: the query being typed, or the last search's outcome.
///
/// Takes the footer's row rather than floating over the map — a place search
/// is a one-line interaction and a modal box would hide the very map the
/// result lands on.
fn render_search_prompt(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let mut spans = Vec::new();
    match &app.search_input {
        Some(query) => {
            spans.push(key_span("/"));
            spans.push(Span::raw(query.clone()));
            // A block shows the caret without needing real cursor placement.
            spans.push(Span::styled(
                "▏",
                Style::default().add_modifier(Modifier::SLOW_BLINK),
            ));
            spans.push(desc_span("   enter"));
            spans.push(desc_span(" pin   esc"));
            spans.push(desc_span(" cancel"));
        }
        None => {
            let failed = matches!(
                app.layers.get_state(LayerId::SearchPin).map(|s| &s.status),
                Some(LayerStatus::Error(_))
            ) || app
                .search_status
                .as_deref()
                .is_some_and(|s| s.starts_with("No match"));
            let style = if failed {
                Style::default().fg(Color::Rgb(Rgb8::AMBER.r, Rgb8::AMBER.g, Rgb8::AMBER.b))
            } else {
                Style::default().fg(Color::Rgb(Rgb8::BLUE.r, Rgb8::BLUE.g, Rgb8::BLUE.b))
            };
            spans.push(Span::styled("pin ", style));
            spans.push(Span::raw(app.search_status.clone().unwrap_or_default()));
        }
    }
    frame.render_widget(Paragraph::new(TextLine::from(spans)), area);
}

fn render_footer(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    // Built from the registry, so the footer can never drift from the help.

    // "zoom" (4) + " X.X" value (5) + trailing space = 10 chars, always fixed
    let zoom_line = TextLine::from(vec![
        key_span("zoom"),
        Span::raw(format!(" {:4.1} ", app.viewport.zoom)),
    ]);

    let scale = render_scale_bar(app);
    let scale_w = scale.chars().count() as u16;

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(10),
            Constraint::Length(scale_w),
        ])
        .split(area);

    frame.render_widget(
        Paragraph::new(TextLine::from(footer_hint_spans(chunks[0].width))),
        chunks[0],
    );
    frame.render_widget(Paragraph::new(zoom_line), chunks[1]);
    frame.render_widget(
        Paragraph::new(TextLine::from(scale)).alignment(ratatui::layout::Alignment::Right),
        chunks[2],
    );
}

fn km_per_char(app: &App) -> f64 {
    let bounds = app.viewport.bounds(app.map_width, app.map_height);
    let wu_per_char = bounds.width() / app.map_width.max(1) as f64;
    let deg_per_char = wu_per_char * 360.0;
    let center = world_to_lat_lon(app.viewport.center);
    let km_per_deg = 111.32 * center.lat.to_radians().cos();
    deg_per_char * km_per_deg
}

/// Nice round interval: 1, 2, 5, 10, 20, 50, 100, …
const NICE: [f64; 16] = [
    1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0, 20000.0,
    50000.0, 100000.0,
];

/// Snap `ideal` chars-per-segment to a divisor of BAR_CHARS (20) that keeps
/// segments wide enough to read at a glance — minimum 4 chars per stripe.
/// Divisors: 4 (5 segs), 5 (4 segs), 10 (2 segs), 20 (1 seg).
fn scale_bar_seg_chars(ideal: usize) -> usize {
    const DIVISORS: [usize; 4] = [4, 5, 10, 20];
    DIVISORS
        .iter()
        .min_by_key(|&&d| d.abs_diff(ideal.max(4)))
        .copied()
        .unwrap_or(5)
}

fn render_scale_bar(app: &App) -> String {
    const BAR_CHARS: usize = 20;
    const TOTAL_WIDTH: usize = 30;

    let kmpc = km_per_char(app).max(f64::EPSILON);
    let total_km = BAR_CHARS as f64 * kmpc;

    let mut segment_km = 1.0;
    for &n in NICE.iter().rev() {
        let segments = total_km / n;
        if segments >= 1.0 && (segments - segments.round()).abs() < 0.15 {
            segment_km = n;
            break;
        }
    }
    if segment_km == 1.0 && total_km > 1.0 {
        for &n in &NICE {
            if n >= total_km / 3.0 {
                segment_km = n;
                break;
            }
        }
    }

    let ideal_seg = (segment_km / kmpc).round() as usize;
    let seg_chars = scale_bar_seg_chars(ideal_seg);

    // Re-derive the label from the *actual* stripe width so the number
    // always matches what's drawn, even when seg_chars was snapped.
    let actual_seg_km = seg_chars as f64 * kmpc;
    let label_km = NICE
        .iter()
        .copied()
        .min_by(|&a, &b| {
            (a - actual_seg_km)
                .abs()
                .partial_cmp(&(b - actual_seg_km).abs())
                .unwrap()
        })
        .unwrap_or(actual_seg_km);
    let label = if label_km >= 1000.0 {
        format!("{:.0}k km", label_km / 1000.0)
    } else {
        format!("{:.0} km", label_km)
    };

    let mut bar = String::with_capacity(BAR_CHARS);
    let mut flip = false;
    let mut i = 0;
    while i < BAR_CHARS {
        let ch = if flip { '░' } else { '█' };
        for _ in 0..seg_chars {
            bar.push(ch);
        }
        i += seg_chars;
        flip = !flip;
    }

    let full = format!("{label} {bar}");
    format!("{:>TOTAL_WIDTH$}", full, TOTAL_WIDTH = TOTAL_WIDTH)
}

/// Render a progress bar using dithered braille characters.
///
/// Each terminal column maps to one braille character which encodes 8 levels
/// (0–8) via progressive dot fill:
///
///   0 ⠀  empty
///   1 ⠂  left middle dot                 (dither ≈ 12 %)
///   2 ⠅  left top + bottom dots          (dither ≈ 25 %)
///   3 ⠇  left 3-dot column
///   4 ⡇  left 4-dot column (full left)
///   5 ⡗  left full + right middle dot
///   6 ⡯  left full + right top + bottom
///   7 ⡿  left full + right 3-dot column
///   8 ⣿  both columns full
///
/// A `width`-char bar therefore has `width × 8` distinct levels.
fn braille_bar(fraction: f64, width: usize) -> String {
    const LEVELS: [char; 9] = [
        '\u{2800}', // ⠀  0/8
        '\u{2802}', // ⠂  1/8  left middle dot
        '\u{2805}', // ⠅  2/8  left top+bottom (dithered)
        '\u{2807}', // ⠇  3/8  left 3 dots
        '\u{2847}', // ⡇  4/8  left col full
        '\u{2857}', // ⡗  5/8  left full + right middle
        '\u{286F}', // ⡯  6/8  left full + right top+bottom
        '\u{287F}', // ⡿  7/8  left full + right 3 dots
        '\u{28FF}', // ⣿  8/8  full
    ];
    let total = width * 8;
    let filled = (fraction.clamp(0.0, 1.0) * total as f64).round() as usize;
    (0..width)
        .map(|i| LEVELS[filled.saturating_sub(i * 8).min(8)])
        .collect()
}

/// Current position, in `[0, 1)`, of a marquee sweeping back and forth over
/// `period` — driven off wall-clock time rather than any per-task state, so
/// an indeterminate task needs no animation bookkeeping of its own.
fn marquee_phase(period: Duration) -> f64 {
    let period_ms = period.as_millis().max(1);
    let elapsed_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    (elapsed_ms % period_ms) as f64 / period_ms as f64
}

/// Render an indeterminate progress marquee: a small lit block bouncing
/// across a `width`-char bar, at the position `phase` (`[0, 1)`, one lap per
/// call to [`marquee_phase`]) maps to via a triangle wave — so it sweeps to
/// one end and back rather than snapping from the far edge to the start.
fn braille_marquee(phase: f64, width: usize) -> String {
    const BLOCK: usize = 3; // lit-cell width of the sweeping block
    if width == 0 {
        return String::new();
    }
    let block = BLOCK.min(width);
    let travel = width - block; // range the block's left edge can occupy
    let phase = phase.rem_euclid(1.0);
    let triangle = if phase < 0.5 {
        phase * 2.0
    } else {
        2.0 - phase * 2.0
    };
    let pos = (triangle * travel as f64).round() as usize;
    (0..width)
        .map(|i| {
            if (pos..pos + block).contains(&i) {
                '\u{28FF}' // ⣿ full
            } else {
                '\u{2800}' // ⠀ empty
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TaskKind;
    use crate::layers::{RadarRun, RadarTile};
    use std::cell::Cell;

    /// `ActiveTask` has pub fields; build fixtures directly rather than
    /// relying on any `App` constructor (none cheap exists — see the
    /// `app::tests` module comment).
    fn task(id: u64, state: TaskState, started_at: Instant) -> ActiveTask {
        ActiveTask {
            id,
            label: "test".into(),
            action: String::new(),
            fraction: Some(0.0),
            display_fraction: 0.0,
            anim_from: 0.0,
            anim_t: 1.0,
            kind: TaskKind::RadarFrame,
            state,
            started_at,
            completed_at: None,
            last_anim: started_at,
        }
    }

    // ── map-legend CP-2: legend_area placement + active_scales mapping ──

    /// `legend_area` must sit on the same baseline as `layer_area` (same y,
    /// same height for a given input) and mirror its inset: two columns from
    /// the right edge instead of the left.
    #[test]
    fn legend_area_mirrors_layer_area_placement() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };
        let main = layer_area(area);
        let legend = legend_area(area, 20, main.height);

        assert_eq!(legend.y, main.y, "same baseline as the layer panel");
        assert_eq!(legend.height, main.height);
        assert_eq!(
            legend.x + legend.width,
            area.x + area.width - 2,
            "right edge sits two columns inset from the right edge, mirroring \
             the layer panel's left inset of 2"
        );
    }

    /// Width and height are clamped the same way `layer_area` clamps its own
    /// dimensions, so an oversized request never draws outside the map or
    /// over the footer row.
    #[test]
    fn legend_area_clamps_to_available_space() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 5,
        };
        let legend = legend_area(area, 200, 200);

        assert_eq!(legend.width, area.width.saturating_sub(3));
        assert_eq!(legend.height, area.height.saturating_sub(1));
        assert!(legend.x >= area.x);
        assert!(legend.x + legend.width <= area.x + area.width);
        assert!(legend.y + legend.height <= area.y + area.height);
    }

    /// No colour-carrying layer active → nothing to show.
    #[test]
    fn active_scales_empty_when_nothing_active() {
        let rms = RenderModeState::new();
        assert_eq!(active_scales(&rms), vec![]);
    }

    /// Radar owning `Braille` is enough to activate the dBZ scale.
    #[test]
    fn active_scales_radar_braille_yields_dbz_only() {
        let mut rms = RenderModeState::new();
        rms.braille = Some(LayerId::Radar);
        assert_eq!(active_scales(&rms), vec![LegendScale::Dbz]);
    }

    /// Both radar (via `Color`) and an observation property active: both
    /// appear, in fixed declaration order (dBZ before Temperature), not
    /// activation order.
    #[test]
    fn active_scales_radar_and_obs_layer_both_appear_in_declaration_order() {
        let mut rms = RenderModeState::new();
        rms.text = Some(LayerId::SurfTemp);
        rms.color = Some(LayerId::Radar);
        assert_eq!(
            active_scales(&rms),
            vec![LegendScale::Dbz, LegendScale::Temperature]
        );
    }

    /// A geographic layer (no colour ramp) owning a mode contributes nothing.
    #[test]
    fn active_scales_geographic_layer_contributes_nothing() {
        let mut rms = RenderModeState::new();
        rms.text = Some(LayerId::MapBorders);
        assert_eq!(active_scales(&rms), vec![]);
    }

    // ── Observation colour scales: map-legend CP-1 band-table extraction ──

    /// Each scale's band table must be retrievable with its correct unit
    /// label, so the future legend never needs a second hardcoded unit list.
    #[test]
    fn obs_scale_reports_correct_unit_per_property() {
        assert_eq!(obs_scale(ObservationProperty::Temperature).unit, "°C");
        assert_eq!(obs_scale(ObservationProperty::WindSpeed).unit, "m/s");
        assert_eq!(obs_scale(ObservationProperty::Humidity).unit, "%");
        assert_eq!(obs_scale(ObservationProperty::Pressure).unit, "hPa");
    }

    #[test]
    fn obs_scale_bands_are_open_ended_at_the_top() {
        for property in [
            ObservationProperty::Temperature,
            ObservationProperty::WindSpeed,
            ObservationProperty::Humidity,
            ObservationProperty::Pressure,
        ] {
            let scale = obs_scale(property);
            assert!(
                scale.bands.last().unwrap().max.is_infinite(),
                "{property:?} top band must be open-ended"
            );
        }
    }

    // ── map-legend CP-3 rework: sub-character gradient bar ──────────────

    /// Every band contributes exactly `LEGEND_HALF_CELLS_PER_BAND` half-cell
    /// colour samples — no remainder distribution, so segments stay uniform
    /// regardless of how many bands a scale has.
    #[test]
    fn legend_bar_half_colors_are_uniform_per_band() {
        for bands in [
            crate::providers::meteogate::DBZ_BANDS
                .iter()
                .map(|b| LegendBand {
                    max: f64::from(b.max),
                    color: b.color,
                })
                .collect::<Vec<_>>(),
            TEMPERATURE_BANDS
                .iter()
                .map(|b| LegendBand {
                    max: b.max,
                    color: b.color,
                })
                .collect::<Vec<_>>(),
        ] {
            let half_colors = legend_bar_half_colors(&bands);
            assert_eq!(
                half_colors.len(),
                bands.len() * LEGEND_HALF_CELLS_PER_BAND as usize,
                "every band must contribute the same fixed half-cell count"
            );
            for (i, band) in bands.iter().enumerate() {
                let start = i * LEGEND_HALF_CELLS_PER_BAND as usize;
                let run = &half_colors[start..start + LEGEND_HALF_CELLS_PER_BAND as usize];
                assert!(
                    run.iter().all(|&c| c == to_terminal_color(band.color)),
                    "band {i}'s half-cells must all carry its own colour"
                );
            }
        }
    }

    /// `LEGEND_HALF_CELLS_PER_BAND` must be odd — that's what makes a band
    /// boundary land mid-cell (producing the two-colour `▌` split) rather
    /// than exactly on a cell edge.
    #[test]
    fn legend_half_cells_per_band_is_odd() {
        assert_eq!(LEGEND_HALF_CELLS_PER_BAND % 2, 1);
    }

    /// A cell straddling a band boundary is drawn as `▌` carrying the two
    /// distinct band colours (fg != bg) instead of a blended colour — the
    /// same half-block idiom `timeline_bar_spans` uses (mirrors the test at
    /// `a_download_boundary_splits_a_cell_instead_of_blending_it`).
    #[test]
    fn legend_bar_boundary_cell_splits_instead_of_blending() {
        let bands = vec![
            LegendBand {
                max: 10.0,
                color: Rgb8 { r: 255, g: 0, b: 0 },
            },
            LegendBand {
                max: 20.0,
                color: Rgb8 { r: 0, g: 0, b: 255 },
            },
        ];
        let half_colors = legend_bar_half_colors(&bands);
        let cells = legend_bar_cells(&half_colors);
        let split: Vec<_> = cells.iter().filter(|(g, _)| *g == '▌').collect();
        assert!(
            !split.is_empty(),
            "a band boundary must be drawn with ▌, not a blended colour"
        );
        for (_, style) in split {
            assert_ne!(
                style.fg.unwrap(),
                style.bg.unwrap(),
                "a split cell must paint two distinct band colours"
            );
        }
    }

    /// A cell fully inside one band (both half-cells the same colour) is a
    /// solid colour, not a spurious `▌` split.
    #[test]
    fn legend_bar_within_band_cell_is_solid() {
        let bands = vec![LegendBand {
            max: 10.0,
            color: Rgb8 {
                r: 10,
                g: 20,
                b: 30,
            },
        }];
        let half_colors = legend_bar_half_colors(&bands);
        let cells = legend_bar_cells(&half_colors);
        assert!(!cells.is_empty());
        for (glyph, style) in cells {
            assert_eq!(glyph, ' ', "a solid cell needs no split glyph");
            assert_eq!(style.bg.unwrap(), to_terminal_color(bands[0].color));
        }
    }

    /// A trailing odd half-cell (odd bands × the odd per-band count) must
    /// still render solid — no half-empty or gap cell.
    #[test]
    fn legend_bar_trailing_odd_half_cell_has_no_gap() {
        let bands = vec![
            LegendBand {
                max: 10.0,
                color: Rgb8 { r: 1, g: 2, b: 3 },
            },
            LegendBand {
                max: 20.0,
                color: Rgb8 { r: 4, g: 5, b: 6 },
            },
            LegendBand {
                max: 30.0,
                color: Rgb8 { r: 7, g: 8, b: 9 },
            },
        ];
        let half_colors = legend_bar_half_colors(&bands);
        assert_eq!(half_colors.len() % 2, 1, "test assumes an odd total");
        let cells = legend_bar_cells(&half_colors);
        assert_eq!(cells.len(), half_colors.len().div_ceil(2));
        let (glyph, style) = cells.last().unwrap();
        assert_eq!(*glyph, ' ', "trailing odd half-cell must render solid");
        assert_eq!(style.bg.unwrap(), to_terminal_color(bands[2].color));
    }

    /// No max_bars room at all (rows_per_bar 0, or height 0) drops everything.
    #[test]
    fn fitting_scales_empty_when_nothing_fits() {
        let scales = vec![LegendScale::Dbz, LegendScale::Temperature];
        assert_eq!(fitting_scales(&scales, 2, 0), &[] as &[LegendScale]);
        assert_eq!(fitting_scales(&scales, 0, 100), &[] as &[LegendScale]);
    }

    /// Enough height for every bar keeps them all.
    #[test]
    fn fitting_scales_keeps_everything_when_it_fits() {
        let scales = vec![LegendScale::Dbz, LegendScale::Temperature];
        assert_eq!(fitting_scales(&scales, 2, 4), scales.as_slice());
    }

    /// Short terminal: enough height for one bar only. Since `active_scales`
    /// is dBZ-first, dropping from the tail keeps dBZ and drops the
    /// observation scale — the fixed-priority degradation rule.
    #[test]
    fn fitting_scales_drops_observation_bars_first_keeping_dbz() {
        let scales = vec![
            LegendScale::Dbz,
            LegendScale::Temperature,
            LegendScale::WindSpeed,
        ];
        assert_eq!(fitting_scales(&scales, 2, 2), &[LegendScale::Dbz]);
    }

    /// dBZ's start is marked open-low (`<5`) since reflectivity has no
    /// natural floor, and its end carries a `+` since the top band is
    /// open-ended.
    #[test]
    fn legend_endpoints_dbz_open_low_and_open_high() {
        let bands: Vec<LegendBand> = crate::providers::meteogate::DBZ_BANDS
            .iter()
            .map(|b| LegendBand {
                max: f64::from(b.max),
                color: b.color,
            })
            .collect();
        let (start, end) = legend_endpoints(LegendScale::Dbz, &bands);
        assert_eq!(start, "<5");
        assert_eq!(end, "60+");
    }

    /// Temperature and pressure have no natural domain floor either, but are
    /// shown as the plain first boundary rather than an open-low marker.
    #[test]
    fn legend_endpoints_temperature_and_pressure_use_first_boundary() {
        let temp_bands: Vec<LegendBand> = TEMPERATURE_BANDS
            .iter()
            .map(|b| LegendBand {
                max: b.max,
                color: b.color,
            })
            .collect();
        let (start, end) = legend_endpoints(LegendScale::Temperature, &temp_bands);
        assert_eq!(start, "-20");
        assert_eq!(end, "30+");

        let pressure_bands: Vec<LegendBand> = PRESSURE_BANDS
            .iter()
            .map(|b| LegendBand {
                max: b.max,
                color: b.color,
            })
            .collect();
        let (start, end) = legend_endpoints(LegendScale::Pressure, &pressure_bands);
        assert_eq!(start, "980");
        assert_eq!(end, "1030+");
    }

    /// Wind speed and humidity are physically non-negative, so their start
    /// value is the real domain floor (0), not the first colour boundary.
    #[test]
    fn legend_endpoints_wind_and_humidity_floor_at_zero() {
        let wind_bands: Vec<LegendBand> = WIND_SPEED_BANDS
            .iter()
            .map(|b| LegendBand {
                max: b.max,
                color: b.color,
            })
            .collect();
        let (start, end) = legend_endpoints(LegendScale::WindSpeed, &wind_bands);
        assert_eq!(start, "0");
        assert_eq!(end, "20+");

        let humidity_bands: Vec<LegendBand> = HUMIDITY_BANDS
            .iter()
            .map(|b| LegendBand {
                max: b.max,
                color: b.color,
            })
            .collect();
        let (start, _) = legend_endpoints(LegendScale::Humidity, &humidity_bands);
        assert_eq!(start, "0");
    }

    /// Humidity's top colour band is open-ended (`>= 85`) like every other
    /// scale's, but it is a percentage and can never exceed 100 — so its end
    /// value is the finite `100`, not `85+`.
    #[test]
    fn legend_endpoints_humidity_ends_at_finite_100() {
        assert!(
            HUMIDITY_BANDS.last().unwrap().max.is_infinite(),
            "test assumes the band table itself is open-ended"
        );
        let bands: Vec<LegendBand> = HUMIDITY_BANDS
            .iter()
            .map(|b| LegendBand {
                max: b.max,
                color: b.color,
            })
            .collect();
        let (_, end) = legend_endpoints(LegendScale::Humidity, &bands);
        assert_eq!(end, "100", "humidity must never show an open-ended +");
    }

    /// `legend_labels` positions are strictly monotonic increasing, the low
    /// and high markers are always present at the bar's edges, and the kept
    /// positions are an EVENLY-SPACED (uniform-stride) subset — every gap
    /// between consecutive labels is equal within +/-1 cell, except possibly
    /// the final interval into the high-end marker. A merely min-gap-
    /// satisfying but irregular subset (the old greedy behaviour) would fail
    /// this: it could keep positions with gaps like 5, 15, 40 (uneven) as
    /// long as each pair cleared the minimum.
    #[test]
    fn legend_labels_evenly_spaced_low_high_always_present() {
        let bands: Vec<LegendBand> = crate::providers::meteogate::DBZ_BANDS
            .iter()
            .map(|b| LegendBand {
                max: f64::from(b.max),
                color: b.color,
            })
            .collect();
        let bar_width = legend_bar_width_cells(bands.len());
        let labels = legend_labels(LegendScale::Dbz, &bands, bar_width);

        assert_eq!(labels.first().unwrap().1, "<5", "low-end marker missing");
        assert!(
            labels.last().unwrap().1.ends_with('+'),
            "high-end marker missing its open-ended +: {:?}",
            labels.last()
        );
        assert_eq!(labels.last().unwrap().0, bar_width);
        assert_eq!(labels.first().unwrap().0, 0);

        // Tick colours: low marker -> first band, high marker -> last band, an
        // interior label -> the band whose upper bound it names.
        assert_eq!(
            labels.first().unwrap().2,
            bands[0].color,
            "low marker must carry the first band's colour"
        );
        assert_eq!(
            labels.last().unwrap().2,
            bands.last().unwrap().color,
            "high marker must carry the last band's colour"
        );
        let interior = labels
            .iter()
            .find(|(_, label, _)| label.parse::<f64>().is_ok())
            .expect("must have at least one interior boundary number");
        let value: f64 = interior.1.parse().unwrap();
        let expected = bands
            .iter()
            .find(|b| (b.max - value).abs() < f64::EPSILON)
            .unwrap()
            .color;
        assert_eq!(
            interior.2, expected,
            "interior label {:?} must carry its own band's colour",
            interior.1
        );

        let positions: Vec<u16> = labels.iter().map(|(pos, _, _)| *pos).collect();
        for pair in positions.windows(2) {
            assert!(
                pair[0] < pair[1],
                "positions must be strictly increasing: {labels:?}"
            );
        }

        let gaps: Vec<u16> = positions.windows(2).map(|w| w[1] - w[0]).collect();
        // Every gap except the final one (into the high-end marker, which may
        // be a shorter or longer leftover interval) must be equal within one
        // cell of every other non-final gap.
        if gaps.len() > 2 {
            let regular_gaps = &gaps[..gaps.len() - 1];
            let min_gap = *regular_gaps.iter().min().unwrap();
            let max_gap = *regular_gaps.iter().max().unwrap();
            assert!(
                max_gap - min_gap <= 1,
                "non-final gaps must be evenly spaced within 1 cell: {gaps:?}"
            );
        }
    }

    /// A narrow bar drops every interior candidate (there's no room for any
    /// of them without violating the min-gap rule) but low and high survive
    /// regardless — degradation never removes the two mandatory markers.
    #[test]
    fn legend_labels_narrow_bar_drops_interior_keeps_low_and_high() {
        let bands: Vec<LegendBand> = crate::providers::meteogate::DBZ_BANDS
            .iter()
            .map(|b| LegendBand {
                max: f64::from(b.max),
                color: b.color,
            })
            .collect();
        let labels = legend_labels(LegendScale::Dbz, &bands, LEGEND_LABEL_MIN_GAP);
        assert_eq!(
            labels.len(),
            2,
            "a bar too narrow for any interior gap should keep only low+high: {labels:?}"
        );
        assert_eq!(labels[0].1, "<5");
        assert!(labels[1].1.ends_with('+'));
    }

    /// No active scale → nothing renders and no legend area is reserved
    /// (checked indirectly: the buffer is untouched where the legend would
    /// have drawn).
    #[test]
    fn render_legend_draws_nothing_when_no_scale_active() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let rms = RenderModeState::new();
        let mut t = Terminal::new(TestBackend::new(60, 20)).unwrap();
        t.draw(|f| render_legend(f, f.area(), &rms, 0)).unwrap();
        let b = t.backend().buffer();
        for y in 0..b.area.height {
            for x in 0..b.area.width {
                assert_eq!(b[(x, y)].symbol(), " ", "cell ({x},{y}) was drawn on");
            }
        }
    }

    /// Radar active renders a dBZ bar: colours from `DBZ_BANDS` appear
    /// low→high left→right on the gradient row (row 1 of the scale's
    /// two-row block, inline with the title), a `▌` two-colour boundary
    /// split appears on that same UPPER row, the boundary numbers appear on
    /// the LOWER row under it (row 2) each in its band's tick colour, and
    /// the footer row (last row) is never touched.
    #[test]
    fn render_legend_radar_active_draws_low_to_high_dbz_bar() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut rms = RenderModeState::new();
        rms.braille = Some(LayerId::Radar);
        let mut t = Terminal::new(TestBackend::new(60, 20)).unwrap();
        let footer_y = t.backend().buffer().area.height - 1;
        t.draw(|f| render_legend(f, f.area(), &rms, 0)).unwrap();
        let b = t.backend().buffer();

        let bands = crate::providers::meteogate::DBZ_BANDS;
        let first_bg = to_terminal_color(bands.first().unwrap().color);
        let last_bg = to_terminal_color(bands.last().unwrap().color);
        // Not testing an exact y (legend is bottom-aligned): find whichever
        // row carries the lowest band's colour, then assert the highest
        // band's colour is on that SAME row — proving the gradient is one
        // row, not spread across a bar row plus a separate label row.
        let row_with =
            |c: Color| (0..b.area.height).find(|&y| (0..b.area.width).any(|x| b[(x, y)].bg == c));
        let first_row = row_with(first_bg).expect("lowest dBZ band colour missing from the bar");
        let last_row = row_with(last_bg).expect("highest dBZ band colour missing from the bar");
        assert_eq!(
            first_row, last_row,
            "low and high band colours must be on the same single (upper) row"
        );

        // The upper row (y0, inline with the title) must carry a `▌`
        // two-colour split at a boundary.
        let split_x = (0..b.area.width).find(|&x| b[(x, first_row)].symbol() == "▌");
        let split_x = split_x.expect("gradient row must contain a ▌ boundary split cell");
        let cell = &b[(split_x, first_row)];
        assert_ne!(
            cell.fg, cell.bg,
            "a ▌ split cell must carry two distinct band colours"
        );

        // The row below the gradient (y1) must carry a boundary number.
        let numbers_row = first_row + 1;
        let mut numbers_row_text = String::new();
        for x in 0..b.area.width {
            numbers_row_text.push_str(b[(x, numbers_row)].symbol());
        }
        assert!(
            numbers_row_text.chars().any(|c| c.is_ascii_digit()),
            "row below the bar must show boundary numbers: {numbers_row_text:?}"
        );

        // The high marker (`60+`) must be drawn in the last dBZ band's colour.
        let high_marker_x = (0..b.area.width).find(|&x| {
            let text: String = (x..b.area.width)
                .take(3)
                .map(|cx| b[(cx, numbers_row)].symbol().chars().next().unwrap_or(' '))
                .collect();
            text.starts_with("60+")
        });
        let high_marker_x = high_marker_x.expect("high marker `60+` missing from the numbers row");
        assert_eq!(
            b[(high_marker_x, numbers_row)].fg,
            last_bg,
            "the high marker must be drawn in the last band's tick colour"
        );

        for x in 0..b.area.width {
            assert_eq!(
                b[(x, footer_y)].bg,
                Color::Reset,
                "legend must never draw over the footer row"
            );
        }
    }

    /// An observation property active renders its bar with the correct unit
    /// in the name column.
    #[test]
    fn render_legend_obs_property_shows_correct_unit() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut rms = RenderModeState::new();
        rms.text = Some(LayerId::SurfWind);
        let mut t = Terminal::new(TestBackend::new(60, 20)).unwrap();
        t.draw(|f| render_legend(f, f.area(), &rms, 0)).unwrap();
        let b = t.backend().buffer();

        let mut text = String::new();
        for y in 0..b.area.height {
            for x in 0..b.area.width {
                text.push_str(b[(x, y)].symbol());
            }
        }
        assert!(
            text.contains("m/s"),
            "wind legend must show its unit: {text:?}"
        );
    }

    /// Both radar and an observation property active: two two-row blocks
    /// stack (header text for both scales is present).
    #[test]
    fn render_legend_both_active_stack_as_horizontal_strips() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut rms = RenderModeState::new();
        rms.braille = Some(LayerId::Radar);
        rms.color = Some(LayerId::SurfTemp);
        let mut t = Terminal::new(TestBackend::new(60, 20)).unwrap();
        t.draw(|f| render_legend(f, f.area(), &rms, 0)).unwrap();
        let b = t.backend().buffer();

        let mut text = String::new();
        for y in 0..b.area.height {
            for x in 0..b.area.width {
                text.push_str(b[(x, y)].symbol());
            }
        }
        assert!(
            text.contains("dBZ"),
            "dBZ bar's name column missing: {text:?}"
        );
        assert!(
            text.contains("Temp"),
            "Temp bar's name column missing: {text:?}"
        );
    }

    /// A short terminal drops whole bars, keeping dBZ and dropping the
    /// observation bar first.
    #[test]
    fn render_legend_short_terminal_keeps_dbz_drops_obs_first() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut rms = RenderModeState::new();
        rms.braille = Some(LayerId::Radar);
        rms.color = Some(LayerId::SurfTemp);
        // Just enough height for one two-row block (LEGEND_ROWS_PER_BAR = 2)
        // plus the one row of bottom padding `legend_area`/`layer_area`
        // reserve — not enough for a second block.
        let mut t = Terminal::new(TestBackend::new(60, 3)).unwrap();
        t.draw(|f| render_legend(f, f.area(), &rms, 0)).unwrap();
        let b = t.backend().buffer();

        let mut text = String::new();
        for y in 0..b.area.height {
            for x in 0..b.area.width {
                text.push_str(b[(x, y)].symbol());
            }
        }
        assert!(text.contains("dBZ"), "dBZ bar should survive: {text:?}");
        assert!(
            !text.contains("Temp"),
            "obs bar should be dropped first: {text:?}"
        );
    }

    /// A full task queue (8 visible tasks, `render_task_queue`'s cap) reserves
    /// its rows across the whole width; on a terminal short enough that naive
    /// top-height + bottom-height would collide, `render_legend` must never
    /// write into those reserved rows. Sentinel-fills the reserved region
    /// before drawing (rather than invoking `render_task_queue`, which needs
    /// a full `App`) so the assertion is a direct buffer check against the
    /// real `render_legend` under the exact budget `render_task_queue`
    /// exposes via `task_queue_reserved_rows`.
    #[test]
    fn render_legend_never_overlaps_a_full_task_queue_on_a_short_terminal() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut rms = RenderModeState::new();
        rms.braille = Some(LayerId::Radar);
        rms.color = Some(LayerId::SurfTemp);

        // Height 10: naive top(8) + bottom(2 scales * 2 rows + 1 padding = 5)
        // = 13 > 10, so without the guard the two regions would collide.
        let mut t = Terminal::new(TestBackend::new(60, 10)).unwrap();
        let reserved = 8u16;

        // Both writes must happen in the same `draw` call: `Terminal` double-
        // buffers, so a sentinel written in one `draw` is gone (reset to
        // blank) by the time the next `draw` call hands out its buffer.
        t.draw(|f| {
            let area = f.area();
            {
                let buf = f.buffer_mut();
                for y in 0..reserved {
                    for x in 0..area.width {
                        if let Some(cell) = buf.cell_mut((x, y)) {
                            cell.set_char('Q');
                        }
                    }
                }
            }
            render_legend(f, area, &rms, reserved);
        })
        .unwrap();
        let b = t.backend().buffer();

        for y in 0..reserved {
            for x in 0..b.area.width {
                assert_eq!(
                    b[(x, y)].symbol(),
                    "Q",
                    "legend wrote into the task queue's reserved row ({x},{y})"
                );
            }
        }
    }

    /// A long-running, already-visible task — `started_at` set well past
    /// `TASK_VISIBLE_AFTER` so `is_visible` returns true at `now`.
    fn visible_task(id: u64) -> ActiveTask {
        ActiveTask {
            id,
            label: "test".into(),
            action: String::new(),
            fraction: Some(0.5),
            display_fraction: 0.5,
            anim_from: 0.5,
            anim_t: 1.0,
            kind: TaskKind::RadarFrame,
            state: TaskState::Running,
            started_at: Instant::now() - Duration::from_secs(1),
            completed_at: None,
            last_anim: Instant::now(),
        }
    }

    #[test]
    fn task_queue_reserved_rows_caps_at_eight_visible_tasks() {
        let now = Instant::now();
        let tasks: Vec<ActiveTask> = (0..12).map(visible_task).collect();
        assert_eq!(task_queue_reserved_rows(&tasks, now), 8);
    }

    #[test]
    fn task_queue_reserved_rows_counts_only_visible_tasks() {
        let now = Instant::now();
        assert_eq!(task_queue_reserved_rows(&[], now), 0);
    }

    // ── Task overlay: CP-5 deterministic overflow sort ─────────────────

    #[test]
    fn running_sorts_before_a_completed_task_positioned_earlier() {
        let now = Instant::now();
        let completed = task(1, TaskState::Completed, now);
        let running = task(2, TaskState::Running, now);
        // Completed is positioned before running in the input vec.
        let sorted = sort_visible_tasks(vec![&completed, &running]);
        assert_eq!(sorted[0].id, 2, "running must precede terminal");
        assert_eq!(sorted[1].id, 1);
    }

    #[test]
    fn two_running_tasks_sort_oldest_first() {
        let now = Instant::now();
        let older = task(1, TaskState::Running, now);
        let newer = task(2, TaskState::Running, now + Duration::from_millis(50));
        let sorted = sort_visible_tasks(vec![&newer, &older]);
        assert_eq!(sorted[0].id, 1, "oldest-started task must sort first");
        assert_eq!(sorted[1].id, 2);
    }

    #[test]
    fn ties_break_stably_by_id() {
        let now = Instant::now();
        let a = task(5, TaskState::Running, now);
        let b = task(3, TaskState::Running, now);
        let sorted = sort_visible_tasks(vec![&a, &b]);
        assert_eq!(
            sorted[0].id, 3,
            "equal state and start time: lower id first"
        );
        assert_eq!(sorted[1].id, 5);
    }

    #[test]
    fn overflow_take_keeps_all_running_and_drops_completed() {
        let now = Instant::now();
        // 6 completed (older start times so they'd sort first under a
        // naive oldest-only order) + 4 running, max_visible = 8.
        let mut tasks: Vec<ActiveTask> = (0..6)
            .map(|i| task(i, TaskState::Completed, now - Duration::from_secs(60 - i)))
            .collect();
        tasks.extend(
            (6..10).map(|i| task(i, TaskState::Running, now - Duration::from_secs(10 - i))),
        );
        let refs: Vec<&ActiveTask> = tasks.iter().collect();
        let sorted = sort_visible_tasks(refs);
        let survivors: Vec<u64> = sorted.into_iter().take(8).map(|t| t.id).collect();
        for running_id in 6..10 {
            assert!(
                survivors.contains(&running_id),
                "running task {running_id} must survive truncation, got {survivors:?}"
            );
        }
        assert_eq!(survivors.len(), 8);
        let dropped_completed = (0..6).filter(|id| !survivors.contains(id)).count();
        assert_eq!(dropped_completed, 2, "2 of the 6 completed rows must drop");
    }

    thread_local! {
        /// Counts how many times `recolor_existing_label` pays for the full
        /// per-character comparison, so the prefilter's savings are directly
        /// observable rather than inferred from wall-clock time.
        pub(super) static LABEL_FULL_COMPARE_CALLS: Cell<usize> = const { Cell::new(0) };
    }

    // ── Task overlay: determinate bar / indeterminate marquee ──────────

    /// The determinate path must render byte-identically to before the
    /// `Option<f64>` change — `braille_bar` itself is untouched, but this
    /// pins the exact output so a future edit to the dispatch in
    /// `render_task_queue` can't silently start feeding it a different
    /// value (e.g. the raw `fraction` instead of `display_fraction`).
    /// Falsify: change the `.round()` to `.floor()` in `braille_bar` and
    /// confirm this assertion breaks.
    #[test]
    fn braille_bar_determinate_output_is_unchanged() {
        assert_eq!(braille_bar(0.5, 12), "⣿⣿⣿⣿⣿⣿⠀⠀⠀⠀⠀⠀");
        assert_eq!(braille_bar(0.0, 12), "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀");
        assert_eq!(braille_bar(1.0, 12), "⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿");
    }

    #[test]
    fn braille_marquee_sweeps_from_left_at_phase_zero() {
        let bar = braille_marquee(0.0, 12);
        assert_eq!(bar, "⣿⣿⣿⠀⠀⠀⠀⠀⠀⠀⠀⠀");
    }

    #[test]
    fn braille_marquee_reaches_the_right_edge_at_phase_half() {
        // Triangle wave peaks at phase 0.5: the block's left edge sits at
        // `width - block`, i.e. flush with the right end of the bar.
        let bar = braille_marquee(0.5, 12);
        assert_eq!(bar, "⠀⠀⠀⠀⠀⠀⠀⠀⠀⣿⣿⣿");
    }

    #[test]
    fn braille_marquee_bounces_back_toward_the_left() {
        // Falsify: a naive sawtooth (no bounce) would keep pos climbing past
        // phase 0.5 instead of reversing; this would fail such an implementation.
        let at_far_end = braille_marquee(0.5, 12);
        let past_the_peak = braille_marquee(0.75, 12);
        assert_ne!(at_far_end, past_the_peak);
        assert_eq!(braille_marquee(0.75, 12), braille_marquee(0.25, 12));
    }

    #[test]
    fn braille_marquee_never_exceeds_the_requested_width() {
        for i in 0..20 {
            let phase = i as f64 / 20.0;
            let bar = braille_marquee(phase, 12);
            assert_eq!(
                bar.chars().count(),
                12,
                "phase {phase} produced the wrong bar length"
            );
        }
    }

    // ── Capital name labels ────────────────────────────────────────────

    /// Read back the text a `raster_capital_names` pass wrote, as
    /// (row, col, string) per contiguous run — enough to assert placement.
    fn rendered_names(bounds: Bounds, width: u16, height: u16) -> Vec<(usize, usize, String)> {
        let mut cells = vec![RasterCell::default(); usize::from(width) * usize::from(height)];
        raster_capital_names(&mut cells, bounds, width, height);
        let mut out = Vec::new();
        for row in 0..usize::from(height) {
            let mut col = 0usize;
            while col < usize::from(width) {
                if cells[row * usize::from(width) + col].glyph.is_some() {
                    let start = col;
                    let mut s = String::new();
                    while col < usize::from(width) {
                        match cells[row * usize::from(width) + col].glyph {
                            Some(c) => {
                                s.push(c);
                                col += 1;
                            }
                            None => break,
                        }
                    }
                    out.push((row, start, s));
                } else {
                    col += 1;
                }
            }
        }
        out
    }

    /// Bounds tight around Ljubljana (46.05, 14.51).
    fn ljubljana_bounds() -> Bounds {
        let c = lat_lon_to_world(46.05, 14.51);
        Bounds {
            min_x: c.x - 0.01,
            max_x: c.x + 0.01,
            min_y: c.y - 0.01,
            max_y: c.y + 0.01,
        }
    }

    /// The name marks the city, so it must be anchored to the city's own
    /// coordinates — not to whatever weather station happens to be nearby.
    #[test]
    fn capital_name_is_drawn_at_the_citys_own_position() {
        let bounds = ljubljana_bounds();
        let (w, h) = (80u16, 40u16);
        let names = rendered_names(bounds, w, h);
        let (row, col, text) = names
            .iter()
            .find(|(_, _, s)| s.contains("Ljubljana"))
            .expect("Ljubljana must be drawn");
        // City is centred in these bounds, so the name sits at mid-width and
        // one row below mid-height.
        assert_eq!(*col, usize::from(w) / 2, "anchored at the city's column");
        assert_eq!(*row, usize::from(h) / 2 + 1, "one row below the city");
        assert_eq!(text.trim(), "Ljubljana");
    }

    /// Names must not depend on observation data at all: this pass runs with
    /// no stations whatsoever and still labels the city.
    #[test]
    fn capital_names_render_without_any_observation_data() {
        let names = rendered_names(ljubljana_bounds(), 80, 40);
        assert!(
            names.iter().any(|(_, _, s)| s.contains("Ljubljana")),
            "city names must not vanish when no station reports"
        );
    }

    #[test]
    fn capitals_outside_the_viewport_are_not_drawn() {
        let names = rendered_names(ljubljana_bounds(), 80, 40);
        assert!(
            !names.iter().any(|(_, _, s)| s.contains("Reykjavik")),
            "only capitals inside the viewport are drawn, got {names:?}"
        );
    }

    // ── Location marker ────────────────────────────────────────────────

    /// Viewport covering the whole world, so world coords map linearly onto
    /// the 10×10 cell grid used by the marker tests.
    const WORLD_BOUNDS: Bounds = Bounds {
        min_x: 0.0,
        max_x: 1.0,
        min_y: 0.0,
        max_y: 1.0,
    };

    fn location_modes(text: bool, background: bool) -> RenderModeState {
        let mut modes = RenderModeState::new();
        if text {
            modes.set_overlay(RenderMode::Text, LayerId::Location);
        }
        if background {
            modes.set_overlay(RenderMode::Color, LayerId::Location);
        }
        modes
    }

    fn search_pin_modes() -> RenderModeState {
        let mut modes = RenderModeState::new();
        modes.set_overlay(RenderMode::Text, LayerId::SearchPin);
        modes
    }

    fn marker_grid(point: GeoPoint, modes: &RenderModeState) -> Vec<RasterCell> {
        let mut cells = vec![RasterCell::default(); 100];
        raster_pin(
            &mut cells,
            point,
            WORLD_BOUNDS,
            10,
            10,
            modes,
            LayerId::Location,
            Rgb8::RED,
            None,
        );
        cells
    }

    /// Text mode marks the spot with a red `x` and must leave the cell's
    /// background alone, so whatever is underneath still shows.
    #[test]
    fn location_text_mode_draws_a_red_x_without_touching_the_background() {
        // lat/lon 0,0 is the centre of the world in Mercator space.
        let cells = marker_grid(GeoPoint::new(0.0, 0.0), &location_modes(true, false));
        let marked: Vec<&RasterCell> = cells.iter().filter(|c| c.glyph.is_some()).collect();
        assert_eq!(marked.len(), 1, "exactly one cell is marked");
        assert_eq!(marked[0].glyph, Some('x'));
        assert_eq!(marked[0].color, Some(Rgb8::RED));
        assert_eq!(marked[0].bg, None, "text mode must not set a background");
    }

    /// Background mode paints the cell red but must not touch the foreground —
    /// that is what lets radar braille read through the marker.
    #[test]
    fn location_background_mode_sets_red_bg_and_leaves_the_foreground() {
        let mut cells = vec![RasterCell::default(); 100];
        let centre = 5 * 10 + 5;
        cells[centre].glyph = Some('@');
        cells[centre].color = Some(Rgb8::GREEN);
        raster_pin(
            &mut cells,
            GeoPoint::new(0.0, 0.0),
            WORLD_BOUNDS,
            10,
            10,
            &location_modes(false, true),
            LayerId::Location,
            Rgb8::RED,
            None,
        );
        assert_eq!(cells[centre].bg, Some(Rgb8::RED));
        assert_eq!(cells[centre].glyph, Some('@'), "foreground untouched");
        assert_eq!(cells[centre].color, Some(Rgb8::GREEN), "colour untouched");
    }

    #[test]
    fn location_marker_is_not_drawn_when_it_owns_no_mode() {
        let cells = marker_grid(GeoPoint::new(0.0, 0.0), &location_modes(false, false));
        assert!(cells.iter().all(|c| c.glyph.is_none() && c.bg.is_none()));
    }

    // ── Accuracy gate at the render site ───────────────────────────────

    use crate::providers::location::LocationSource;

    fn location_fix(accuracy_m: Option<f64>) -> LocationFix {
        LocationFix::new(
            GeoPoint::new(0.0, 0.0),
            accuracy_m,
            LocationSource::Platform,
        )
    }

    fn draw_location_marker(fix: Option<LocationFix>, label: Option<&str>) -> Vec<RasterCell> {
        let mut cells = vec![RasterCell::default(); 100];
        raster_location_marker(
            &mut cells,
            fix,
            label,
            WORLD_BOUNDS,
            10,
            10,
            &location_modes(true, false),
        );
        cells
    }

    /// A 10 km fix is the motivating case: GeoIP delivers exactly this on
    /// boot, and it must not draw a dot claiming better precision than it has.
    #[test]
    fn a_coarse_10km_fix_draws_no_marker() {
        let cells = draw_location_marker(Some(location_fix(Some(10_000.0))), Some("Ljubljana"));
        assert!(
            cells.iter().all(|c| c.glyph.is_none()),
            "coarse fix must not draw the marker"
        );
        assert!(
            !cells.iter().any(|c| c.glyph == Some('L')),
            "coarse fix must not draw the label either"
        );
    }

    /// A precise 25 m fix (the WiFi-refined GeoClue stage) draws normally.
    #[test]
    fn a_precise_25m_fix_draws_the_marker() {
        let cells = draw_location_marker(Some(location_fix(Some(25.0))), None);
        assert!(
            cells.iter().any(|c| c.glyph == Some('x')),
            "precise fix must draw the marker"
        );
    }

    #[test]
    fn unknown_accuracy_draws_no_marker() {
        let cells = draw_location_marker(Some(location_fix(None)), None);
        assert!(cells.iter().all(|c| c.glyph.is_none()));
    }

    #[test]
    fn manual_fix_always_draws_regardless_of_accuracy() {
        let fix = LocationFix::new(GeoPoint::new(0.0, 0.0), None, LocationSource::Manual);
        let cells = draw_location_marker(Some(fix), None);
        assert!(
            cells.iter().any(|c| c.glyph == Some('x')),
            "Manual (--lat/--lon) must always render"
        );
    }

    #[test]
    fn no_fix_draws_no_marker() {
        let cells = draw_location_marker(None, None);
        assert!(cells.iter().all(|c| c.glyph.is_none()));
    }

    // ── Pin collision nudging ──────────────────────────────────────────

    /// The `x` must not blank a city name or a reading it lands on.
    #[test]
    fn pin_glyph_moves_to_a_free_cell_instead_of_erasing_text() {
        let mut cells = vec![RasterCell::default(); 100];
        let centre = 5 * 10 + 5;
        cells[centre].glyph = Some('L'); // e.g. the "Ljubljana" label
        cells[centre].color = Some(Rgb8::GRAY);
        raster_pin(
            &mut cells,
            GeoPoint::new(0.0, 0.0),
            WORLD_BOUNDS,
            10,
            10,
            &location_modes(true, false),
            LayerId::Location,
            Rgb8::RED,
            None,
        );
        assert_eq!(cells[centre].glyph, Some('L'), "existing label survives");
        let pin = cells.iter().position(|c| c.glyph == Some('x'));
        assert!(pin.is_some(), "the marker is still drawn somewhere");
        let idx = pin.unwrap();
        let (px, py) = ((idx % 10) as i32, (idx / 10) as i32);
        assert!(
            (px - 5).abs() <= 1 && (py - 5).abs() <= 1,
            "nudged to an adjacent cell, got ({px},{py})"
        );
    }

    /// The background only tints, so it never needs to move — it must stay on
    /// the true cell even when a label sits there.
    #[test]
    fn pin_background_stays_put_when_the_cell_holds_text() {
        let mut cells = vec![RasterCell::default(); 100];
        let centre = 5 * 10 + 5;
        cells[centre].glyph = Some('L');
        raster_pin(
            &mut cells,
            GeoPoint::new(0.0, 0.0),
            WORLD_BOUNDS,
            10,
            10,
            &location_modes(false, true),
            LayerId::Location,
            Rgb8::RED,
            None,
        );
        assert_eq!(cells[centre].bg, Some(Rgb8::RED), "tint on the true cell");
        assert_eq!(cells[centre].glyph, Some('L'));
    }

    /// With everything nearby taken, drawing over a label beats vanishing.
    #[test]
    fn pin_glyph_overwrites_when_no_free_cell_is_within_reach() {
        let mut cells = vec![RasterCell::default(); 100];
        for c in cells.iter_mut() {
            c.glyph = Some('#');
        }
        raster_pin(
            &mut cells,
            GeoPoint::new(0.0, 0.0),
            WORLD_BOUNDS,
            10,
            10,
            &location_modes(true, false),
            LayerId::Location,
            Rgb8::RED,
            None,
        );
        assert_eq!(cells[5 * 10 + 5].glyph, Some('x'), "drawn at the true cell");
    }

    #[test]
    fn nearest_free_cell_returns_the_cell_itself_when_free() {
        let cells = vec![RasterCell::default(); 100];
        assert_eq!(nearest_free_cell(&cells, 10, 10, 4, 6), Some((4, 6)));
    }

    /// Columns are about half as wide as rows are tall, so a horizontal nudge
    /// is visually shorter than a vertical one of the same cell count.
    #[test]
    fn nearest_free_cell_prefers_a_horizontal_nudge() {
        let mut cells = vec![RasterCell::default(); 100];
        cells[5 * 10 + 5].glyph = Some('#');
        // Both (4,5)/(6,5) and (5,4)/(5,6) are one cell away.
        let (x, y) = nearest_free_cell(&cells, 10, 10, 5, 5).unwrap();
        assert_eq!(y, 5, "stays on the same row");
        assert!(x == 4 || x == 6, "moved sideways, got x={x}");
    }

    #[test]
    fn nearest_free_cell_gives_up_when_everything_is_occupied() {
        let mut cells = vec![RasterCell::default(); 100];
        for c in cells.iter_mut() {
            c.glyph = Some('#');
        }
        assert_eq!(nearest_free_cell(&cells, 10, 10, 5, 5), None);
    }

    /// The search pin renders through the same path, in blue.
    #[test]
    fn search_pin_draws_a_blue_x() {
        let mut cells = vec![RasterCell::default(); 100];
        raster_pin(
            &mut cells,
            GeoPoint::new(0.0, 0.0),
            WORLD_BOUNDS,
            10,
            10,
            &search_pin_modes(),
            LayerId::SearchPin,
            Rgb8::BLUE,
            None,
        );
        let marked: Vec<&RasterCell> = cells.iter().filter(|c| c.glyph.is_some()).collect();
        assert_eq!(marked.len(), 1);
        assert_eq!(marked[0].glyph, Some('x'));
        assert_eq!(marked[0].color, Some(Rgb8::BLUE));
    }

    /// End to end: both pins on one map, each in its own colour. Searching
    /// must not blank the "you are here" marker.
    #[test]
    fn both_pins_render_together_in_their_own_colours() {
        let mut modes = RenderModeState::new();
        modes.set_overlay(RenderMode::Text, LayerId::Location);
        modes.set_overlay(RenderMode::Text, LayerId::SearchPin);
        let mut cells = vec![RasterCell::default(); 100];
        // Two clearly separate points inside the world bounds.
        raster_pin(
            &mut cells,
            GeoPoint::new(-60.0, 0.0),
            WORLD_BOUNDS,
            10,
            10,
            &modes,
            LayerId::Location,
            Rgb8::RED,
            None,
        );
        raster_pin(
            &mut cells,
            GeoPoint::new(60.0, 0.0),
            WORLD_BOUNDS,
            10,
            10,
            &modes,
            LayerId::SearchPin,
            Rgb8::BLUE,
            None,
        );
        let drawn: Vec<Rgb8> = cells
            .iter()
            .filter(|c| c.glyph == Some('x'))
            .filter_map(|c| c.color)
            .collect();
        assert_eq!(drawn.len(), 2, "both pins drawn");
        assert!(drawn.contains(&Rgb8::RED), "location marker still there");
        assert!(drawn.contains(&Rgb8::BLUE), "search pin there too");
    }

    // ── pin labels ─────────────────────────────────────────────────────

    /// Read a row of the raster back as a string, for label assertions.
    fn row_text(cells: &[RasterCell], width: usize, row: usize) -> String {
        (0..width)
            .map(|c| cells[row * width + c].glyph.unwrap_or(' '))
            .collect()
    }

    #[test]
    fn pin_label_is_drawn_below_the_marker() {
        let mut modes = RenderModeState::new();
        modes.set_overlay(RenderMode::Text, LayerId::Location);
        let mut cells = vec![RasterCell::default(); 400];
        raster_pin(
            &mut cells,
            GeoPoint::new(0.0, 0.0),
            WORLD_BOUNDS,
            20,
            20,
            &modes,
            LayerId::Location,
            Rgb8::RED,
            Some("Kranj"),
        );
        let marker_row = cells.iter().position(|c| c.glyph == Some('x')).unwrap() / 20;
        assert!(
            row_text(&cells, 20, marker_row + 1).contains("Kranj"),
            "label sits on the row under the marker"
        );
    }

    /// The whole point of the recolour path: standing in a city whose name is
    /// already on the map must tint that name, not print a second copy.
    #[test]
    fn pin_label_recolors_an_existing_name_instead_of_duplicating_it() {
        let mut modes = RenderModeState::new();
        modes.set_overlay(RenderMode::Text, LayerId::Location);
        let width = 20usize;
        let mut cells = vec![RasterCell::default(); width * 20];

        // Pre-draw "Ljubljana" where the capital label would already be.
        let (lrow, lcol) = (11usize, 8usize);
        for (i, ch) in "Ljubljana".chars().enumerate() {
            cells[lrow * width + lcol + i].glyph = Some(ch);
            cells[lrow * width + lcol + i].color = Some(Rgb8::new(105, 105, 105));
        }

        raster_pin(
            &mut cells,
            GeoPoint::new(0.0, 0.0),
            WORLD_BOUNDS,
            width as u16,
            20,
            &modes,
            LayerId::Location,
            Rgb8::RED,
            Some("Ljubljana"),
        );

        let occurrences = (0..20)
            .filter(|&r| row_text(&cells, width, r).contains("Ljubljana"))
            .count();
        assert_eq!(occurrences, 1, "name is not duplicated");
        assert_eq!(
            cells[lrow * width + lcol].color,
            Some(Rgb8::RED),
            "the existing name is recoloured to the pin colour"
        );
    }

    /// Casing differs between capital labels and station names; a pin in
    /// "LJUBLJANA" is still marking Ljubljana.
    #[test]
    fn pin_label_match_ignores_case() {
        let width = 20usize;
        let mut cells = vec![RasterCell::default(); width * 20];
        for (i, ch) in "LJUBLJANA".chars().enumerate() {
            cells[10 * width + 5 + i].glyph = Some(ch);
        }
        assert!(recolor_existing_label(
            &mut cells,
            width as u16,
            20,
            "Ljubljana",
            Rgb8::RED
        ));
        assert_eq!(cells[10 * width + 5].color, Some(Rgb8::RED));
    }

    /// The pin and the capital label are anchored to different points — your
    /// measured position vs the city's hardcoded centre — so a match many
    /// cells away is still the same place and must not be printed twice.
    /// This is the case that shipped broken: five rows apart, drawn twice.
    #[test]
    fn pin_label_recolors_a_match_far_from_the_pin() {
        let width = 40usize;
        let mut cells = vec![RasterCell::default(); width * 20];
        for (i, ch) in "Ljubljana".chars().enumerate() {
            cells[15 * width + 30 + i].glyph = Some(ch);
        }
        assert!(recolor_existing_label(
            &mut cells,
            width as u16,
            20,
            "Ljubljana",
            Rgb8::RED
        ));
        assert_eq!(cells[15 * width + 30].color, Some(Rgb8::RED));
    }

    /// A grid where no cell's first character matches must be ruled out by
    /// the prefilter alone; the full per-character comparison should never
    /// run. Before the prefilter existed this counter incremented once per
    /// start offset in every row, so this test failed on the pre-fix code.
    #[test]
    fn pin_label_no_match_grid_skips_the_full_compare() {
        let width = 40usize;
        let mut cells = vec![RasterCell::default(); width * 20];
        for c in cells.iter_mut() {
            c.glyph = Some('#');
        }
        LABEL_FULL_COMPARE_CALLS.with(|c| c.set(0));
        assert!(!recolor_existing_label(
            &mut cells,
            width as u16,
            20,
            "Ljubljana",
            Rgb8::RED
        ));
        assert_eq!(
            LABEL_FULL_COMPARE_CALLS.with(|c| c.get()),
            0,
            "first-character prefilter must rule out a no-match grid without a full compare"
        );
    }

    /// A different name is never touched, however close it sits.
    #[test]
    fn pin_label_does_not_recolor_a_different_name() {
        let width = 40usize;
        let mut cells = vec![RasterCell::default(); width * 20];
        for (i, ch) in "Kranj".chars().enumerate() {
            cells[2 * width + 3 + i].glyph = Some(ch);
        }
        assert!(!recolor_existing_label(
            &mut cells,
            width as u16,
            20,
            "Ljubljana",
            Rgb8::RED
        ));
        assert_eq!(cells[2 * width + 3].color, None);
    }

    /// An unlabelled pin must still render — the marker is the important part.
    #[test]
    fn pin_without_a_label_still_draws_its_marker() {
        let mut modes = RenderModeState::new();
        modes.set_overlay(RenderMode::Text, LayerId::Location);
        let mut cells = vec![RasterCell::default(); 100];
        raster_pin(
            &mut cells,
            GeoPoint::new(0.0, 0.0),
            WORLD_BOUNDS,
            10,
            10,
            &modes,
            LayerId::Location,
            Rgb8::RED,
            None,
        );
        assert_eq!(cells.iter().filter(|c| c.glyph == Some('x')).count(), 1);
    }

    /// The label must never partially clobber a reading already on that row.
    #[test]
    fn pin_label_is_skipped_when_the_row_below_is_occupied() {
        let width = 20usize;
        let mut cells = vec![RasterCell::default(); width * 20];
        for c in cells.iter_mut().skip(11 * width).take(width) {
            c.glyph = Some('7');
        }
        write_pin_label(&mut cells, width as u16, 20, 10, 10, "Kranj", Rgb8::RED);
        assert!(
            !row_text(&cells, width, 11).contains("Kranj"),
            "label yields to existing text"
        );
    }

    /// When both pins land on the same cell the nudge keeps them both
    /// visible instead of one silently replacing the other.
    #[test]
    fn overlapping_pins_do_not_erase_each_other() {
        let mut modes = RenderModeState::new();
        modes.set_overlay(RenderMode::Text, LayerId::Location);
        modes.set_overlay(RenderMode::Text, LayerId::SearchPin);
        let mut cells = vec![RasterCell::default(); 100];
        let same = GeoPoint::new(0.0, 0.0);
        raster_pin(
            &mut cells,
            same,
            WORLD_BOUNDS,
            10,
            10,
            &modes,
            LayerId::Location,
            Rgb8::RED,
            None,
        );
        raster_pin(
            &mut cells,
            same,
            WORLD_BOUNDS,
            10,
            10,
            &modes,
            LayerId::SearchPin,
            Rgb8::BLUE,
            None,
        );
        let drawn: Vec<Rgb8> = cells
            .iter()
            .filter(|c| c.glyph == Some('x'))
            .filter_map(|c| c.color)
            .collect();
        assert_eq!(drawn.len(), 2, "both pins survive an exact overlap");
        assert!(drawn.contains(&Rgb8::RED) && drawn.contains(&Rgb8::BLUE));
    }

    /// The two pins are independent: the location marker must not appear just
    /// because the search pin owns a mode.
    #[test]
    fn a_pin_only_draws_when_its_own_layer_owns_a_mode() {
        let mut cells = vec![RasterCell::default(); 100];
        raster_pin(
            &mut cells,
            GeoPoint::new(0.0, 0.0),
            WORLD_BOUNDS,
            10,
            10,
            &search_pin_modes(),
            LayerId::Location,
            Rgb8::RED,
            None,
        );
        assert!(cells.iter().all(|c| c.glyph.is_none()));
    }

    #[test]
    fn location_marker_lands_in_the_cell_matching_its_coordinates() {
        let cells = marker_grid(GeoPoint::new(0.0, 0.0), &location_modes(true, false));
        let idx = cells.iter().position(|c| c.glyph.is_some()).unwrap();
        // Centre of a 10×10 grid.
        assert_eq!((idx % 10, idx / 10), (5, 5));
    }

    #[test]
    fn location_marker_outside_the_viewport_is_not_drawn() {
        let bounds = Bounds {
            min_x: 0.0,
            max_x: 0.1,
            min_y: 0.0,
            max_y: 0.1,
        };
        let mut cells = vec![RasterCell::default(); 100];
        // Ljubljana — well outside the top-left corner of the world.
        raster_pin(
            &mut cells,
            GeoPoint::new(14.5, 46.05),
            bounds,
            10,
            10,
            &location_modes(true, true),
            LayerId::Location,
            Rgb8::RED,
            None,
        );
        assert!(cells.iter().all(|c| c.glyph.is_none() && c.bg.is_none()));
    }

    /// Flatten bar spans into one (char, style) per rendered cell.
    fn bar_cells(spans: &[Span<'static>]) -> Vec<(char, Style)> {
        spans
            .iter()
            .flat_map(|s| s.content.chars().map(|c| (c, s.style)))
            .collect()
    }

    #[test]
    fn bar_width_is_stable_regardless_of_how_many_frames_loaded() {
        // The collapse this replaced: width tracked the loaded frame count.
        let space = 200;
        // Two slots per cell: 36 slots over 18 cells is 36 half-cells.
        assert_eq!(timeline_bar_width(3, space), Some(18));
        assert_eq!(timeline_bar_width(6, space), Some(36));
        // Deeper windows cap rather than stretch.
        assert_eq!(timeline_bar_width(24, space), Some(TIMELINE_BAR_MAX));
        assert_eq!(timeline_bar_width(12, space), Some(TIMELINE_BAR_MAX));
        // Cramped terminals shrink, then drop the bar entirely.
        assert_eq!(timeline_bar_width(24, 20), Some(20));
        assert_eq!(timeline_bar_width(24, TIMELINE_BAR_MIN - 1), None);
    }

    #[test]
    fn bar_has_no_empty_gaps_at_any_history_depth() {
        // Every half-cell must carry a slot, at every offered depth. Sizing the
        // bar one cell per slot (rather than per half-cell) left every second
        // half-cell on the track colour, dithering the bar instead of filling
        // it.
        for hours in crate::providers::meteogate::HISTORY_OPTIONS {
            let slots = crate::providers::meteogate::frames_for_hours(hours);
            let width = timeline_bar_width(hours, 200).expect("bar fits");
            let states = vec![SlotState::InRam; slots];
            let spans = timeline_bar_spans(slots, 0, &states, width);
            let empty: Vec<Color> = bar_cells(&spans)
                .iter()
                .map(|(_, s)| s.bg.unwrap())
                .filter(|c| *c == BAR_MISSING)
                .collect();
            assert!(
                empty.is_empty(),
                "{hours} h: {} of {} cells left empty",
                empty.len(),
                width
            );
        }
    }

    #[test]
    fn the_bar_only_ever_uses_its_four_colours() {
        // A boundary mid-timeline plus a resident window: the shape most likely
        // to invent an in-between shade.
        let mut states = vec![SlotState::Missing; 288];
        for s in states.iter_mut().take(60) {
            *s = SlotState::OnDisk;
        }
        for s in states.iter_mut().take(25) {
            *s = SlotState::InRam;
        }
        let spans = timeline_bar_spans(288, 12, &states, TIMELINE_BAR_MAX);
        let allowed = [BAR_MISSING, BAR_DOWNLOADED, BAR_RAM_DOTS, BAR_PLAYHEAD];
        for (glyph, style) in bar_cells(&spans) {
            for c in [style.fg.unwrap(), style.bg.unwrap()] {
                assert!(
                    allowed.contains(&c),
                    "glyph {glyph:?} painted {c:?}, which is not one of the four bar colours"
                );
            }
        }
    }

    #[test]
    fn a_download_boundary_splits_a_cell_instead_of_blending_it() {
        // 4 slots over 1 cell: the older half missing, the newer half on disk,
        // so the cell's two halves disagree.
        let states = vec![
            SlotState::OnDisk,
            SlotState::OnDisk,
            SlotState::Missing,
            SlotState::Missing,
        ];
        // Playhead parked off this cell's halves is impossible at width 1, so
        // check the split shape at width 2 where the boundary is interior.
        let states4 = [states.as_slice(), states.as_slice()].concat();
        let cells = bar_cells(&timeline_bar_spans(8, 0, &states4, 2));
        let split: Vec<_> = cells.iter().filter(|(g, _)| *g == '▌').collect();
        assert!(
            !split.is_empty(),
            "a half-cell boundary must be drawn with ▌, not a blended colour"
        );
        for (_, style) in split {
            assert_ne!(
                style.fg.unwrap(),
                style.bg.unwrap(),
                "a split cell paints two different states"
            );
        }
    }

    /// Cells excluding split ones (`▌`), which the playhead and download
    /// boundaries produce.
    fn uniform_cells(spans: &[Span<'static>]) -> Vec<(char, Style)> {
        bar_cells(spans)
            .into_iter()
            .filter(|(g, _)| *g != '▌')
            .collect()
    }

    #[test]
    fn bar_renders_two_half_cells_per_column() {
        let states = vec![SlotState::InRam; 4];
        let spans = timeline_bar_spans(4, 0, &states, 8);
        let cells = bar_cells(&spans);
        assert_eq!(cells.len(), 8, "one glyph per cell");
        assert!(
            uniform_cells(&spans)
                .iter()
                .all(|(c, _)| ('\u{2800}'..='\u{28FF}').contains(c)),
            "every uniform cell is a braille glyph carrying two dot columns"
        );
    }

    #[test]
    fn playhead_is_visible_when_slots_share_a_cell() {
        // 24 h over the widest bar: 288 slots, 96 columns, 3 slots each.
        let states = vec![SlotState::InRam; 288];
        let spans = timeline_bar_spans(288, 150, &states, TIMELINE_BAR_MAX);
        let painted: Vec<Color> = bar_cells(&spans)
            .iter()
            .map(|(_, s)| s.bg.unwrap())
            .collect();
        assert!(
            painted.contains(&BAR_PLAYHEAD),
            "playhead must survive sharing a cell with other slots"
        );
    }

    #[test]
    fn empty_timeline_paints_bare_track() {
        let spans = timeline_bar_spans(0, 0, &[], 6);
        let cells = bar_cells(&spans);
        assert_eq!(cells.len(), 6, "track is drawn even before frames arrive");
        for (glyph, style) in cells {
            // Nothing loaded yet looks the same as nothing available — one grey.
            assert_eq!(style.bg, Some(BAR_MISSING));
            assert_eq!(glyph, '\u{2800}', "no slots means no stipple");
        }
    }

    /// Backgrounds of every cell except the playhead's, which overrides them.
    fn backgrounds(spans: &[Span<'static>]) -> Vec<Color> {
        bar_cells(spans)
            .iter()
            .map(|(_, s)| s.bg.unwrap())
            .filter(|c| *c != BAR_PLAYHEAD)
            .collect()
    }

    #[test]
    fn background_carries_download_state_only() {
        // Downloaded but not resident reads as solid white; in-RAM shares that
        // background, because RAM is a texture over it rather than a colour.
        let disk = timeline_bar_spans(8, 0, &[SlotState::OnDisk; 8], 4);
        let ram = timeline_bar_spans(8, 0, &[SlotState::InRam; 8], 4);
        let missing = timeline_bar_spans(8, 0, &[SlotState::Missing; 8], 4);
        assert!(backgrounds(&disk).iter().all(|c| *c == BAR_DOWNLOADED));
        assert!(
            backgrounds(&ram).iter().all(|c| *c == BAR_DOWNLOADED),
            "RAM must not change the bg — it is carried by the dots"
        );
        assert!(backgrounds(&missing).iter().all(|c| *c == BAR_MISSING));
    }

    #[test]
    fn ram_shows_as_dots_and_absent_ram_shows_none() {
        let ram = uniform_cells(&timeline_bar_spans(4, 0, &[SlotState::InRam; 4], 2));
        assert!(
            !ram.is_empty() && ram.iter().all(|(c, _)| *c != '\u{2800}'),
            "resident frames must raise dots"
        );
        let disk = uniform_cells(&timeline_bar_spans(4, 0, &[SlotState::OnDisk; 4], 2));
        assert!(
            !disk.is_empty() && disk.iter().all(|(c, _)| *c == '\u{2800}'),
            "downloaded-but-not-resident must be bare white, no stipple"
        );
    }

    /// Dots raised in braille column `col` of `glyph`.
    fn col_dots(glyph: char, col: usize) -> usize {
        let bits = u32::from(glyph) - 0x2800;
        BRAILLE_COL_BITS[col]
            .iter()
            .filter(|b| bits & (1 << **b) != 0)
            .count()
    }

    #[test]
    fn ram_dot_height_tracks_how_much_of_a_column_is_resident() {
        // 8 slots over 2 cells = 4 columns of 2 slots. Frame 0 is newest and
        // lands rightmost, so cell 0 holds the four oldest; the playhead parked
        // on frame 0 splits cell 1, leaving cell 0 uniform to inspect.
        let mut states = vec![SlotState::OnDisk; 8];
        states[6] = SlotState::InRam; // cell 0, left column
        states[7] = SlotState::InRam; // cell 0, left column
        states[4] = SlotState::InRam; // cell 0, right column: 1 of its 2
        let cells = bar_cells(&timeline_bar_spans(8, 0, &states, 2));
        let glyph = cells[0].0;
        assert_ne!(
            glyph, '\u{258C}',
            "cell 0 should be uniform, away from playhead"
        );
        // Fully resident column fills; half-resident is half height — the
        // sub-cell resolution the stipple buys.
        assert_eq!(col_dots(glyph, 0), 4, "fully resident column fills");
        assert_eq!(col_dots(glyph, 1), 2, "half-resident column is half height");
    }

    #[test]
    fn a_single_resident_frame_in_a_busy_column_still_shows() {
        // 24 slots over 2 cells: 6 per column. One resident frame in cell 0 must
        // not round away to zero dots.
        let mut states = vec![SlotState::OnDisk; 24];
        states[23] = SlotState::InRam; // oldest, lands in cell 0
        let cells = bar_cells(&timeline_bar_spans(24, 0, &states, 2));
        assert_ne!(cells[0].0, '\u{2800}', "1-of-6 resident must still stipple");
    }

    #[test]
    fn braille_columns_fill_from_the_bottom() {
        assert_eq!(braille_columns([0, 0]), '\u{2800}');
        // Full both columns = all eight dots.
        assert_eq!(braille_columns([4, 4]), '\u{28FF}');
        // One dot in the left column is dot 7 (the bottom), not dot 1.
        assert_eq!(braille_columns([1, 0]), '\u{2840}');
    }

    #[test]
    fn help_lists_every_binding_in_the_registry() {
        let text: String = help_lines()
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        for b in crate::keys::BINDINGS {
            if let Some(k) = b.help_keys {
                assert!(text.contains(k), "help omits the `{k}` row");
                assert!(text.contains(b.name), "help omits `{}`", b.name);
            }
        }
    }

    #[test]
    fn help_renders_without_painting_its_own_background() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut t = Terminal::new(TestBackend::new(100, 45)).unwrap();
        t.draw(|f| render_help(f, f.area())).unwrap();
        let b = t.backend().buffer();
        // Clear resets the covered cells; nothing may set a fill colour, so the
        // modal inherits whatever background the terminal itself uses.
        for y in 0..b.area.height {
            for x in 0..b.area.width {
                assert_eq!(
                    b[(x, y)].bg,
                    Color::Reset,
                    "cell ({x},{y}) has a background"
                );
            }
        }
    }

    #[test]
    fn footer_hints_come_from_the_registry() {
        let keys: Vec<_> = crate::keys::footer_hints()
            .into_iter()
            .map(|h| h.keys)
            .collect();
        assert_eq!(keys.first(), Some(&"q"), "the way out is ranked first");
        assert!(keys.contains(&"space"), "play/pause is a headline hint");
        assert!(keys.contains(&"enter"), "layer toggle moved to enter");
        assert!(!keys.contains(&"p"), "`p` is no longer bound");
    }

    fn footer_text(width: u16) -> String {
        footer_hint_spans(width)
            .iter()
            .map(|s| s.content.to_string())
            .collect()
    }

    #[test]
    fn a_wide_footer_shows_every_hint() {
        let text = footer_text(200);
        for h in crate::keys::footer_hints() {
            assert!(text.contains(h.label), "wide footer dropped `{}`", h.label);
        }
    }

    #[test]
    fn a_narrow_footer_drops_whole_hints_worst_ranked_first() {
        for width in [10u16, 20, 40, 60, 80] {
            let text = footer_text(width);
            assert!(
                text.chars().count() <= width as usize,
                "footer overflows at width {width}"
            );
            // Whatever survives, the escape hatch does.
            assert!(text.contains("quit"), "width {width} dropped `quit`");
            // Spans come in (key, label) pairs: a hint is shown whole or not
            // at all.  Compare the pairs, not substrings — "i" would otherwise
            // "match" inside "quit".
            let spans = footer_hint_spans(width);
            assert_eq!(spans.len() % 2, 0, "width {width} left a dangling span");
            for pair in spans.chunks(2) {
                let (key, label) = (pair[0].content.as_ref(), pair[1].content.trim());
                assert!(
                    crate::keys::footer_hints()
                        .iter()
                        .any(|h| h.keys == key && h.label == label),
                    "width {width} rendered `{key} {label}`, which is not a registry hint"
                );
            }
        }
    }

    #[test]
    fn a_footer_too_narrow_for_anything_renders_empty() {
        assert_eq!(footer_text(3), "");
    }

    #[test]
    fn help_modal_is_centred_and_sized_to_content() {
        let screen = Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 60,
        };
        let r = centered_rect(40, 20, screen);
        assert_eq!((r.width, r.height), (40, 20), "sized to what was asked for");
        assert_eq!(r.x, 80, "centred horizontally");
        assert_eq!(r.y, 20, "centred vertically");
        assert!(r.width < screen.width && r.height < screen.height);
    }

    #[test]
    fn help_modal_shrinks_to_fit_a_small_terminal() {
        let tiny = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 10,
        };
        let r = centered_rect(80, 40, tiny);
        assert_eq!((r.width, r.height), (30, 10), "clamped, never overflowing");
        assert_eq!((r.x, r.y), (0, 0));
    }

    #[test]
    fn clips_segments_that_cross_viewport_edges() {
        let bounds = Bounds {
            min_x: 0.3,
            max_x: 0.6,
            min_y: 0.25,
            max_y: 0.75,
        };

        let clipped = clipped_segment(bounds, 0.2, 0.5, 0.7, 0.5).unwrap();
        assert!((clipped.0 - 0.3).abs() < 0.0001);
        assert!((clipped.1 - 0.5).abs() < 0.0001);
        assert!((clipped.2 - 0.6).abs() < 0.0001);
        assert!((clipped.3 - 0.5).abs() < 0.0001);
    }

    #[test]
    fn rejects_world_wrapping_segments() {
        let bounds = Bounds {
            min_x: 0.0,
            max_x: 1.0,
            min_y: 0.0,
            max_y: 1.0,
        };

        assert!(clipped_segment(bounds, 0.1, 0.5, 0.9, 0.5).is_none());
    }

    #[test]
    fn border_line_color_is_kind_only() {
        assert_eq!(border_line_color(BorderLineKind::Country), Rgb8::GRAY);
        assert_eq!(border_line_color(BorderLineKind::Region), Rgb8::DARK_GRAY);
        assert_eq!(border_line_color(BorderLineKind::Road), Rgb8::AMBER);
    }

    #[test]
    fn braille_subcells_pack_into_expected_glyph() {
        let mut cells = vec![RasterCell::default(); 1];
        set_subcell(&mut cells, 1, 0, 0, Rgb8::GREEN, 3);
        set_subcell(&mut cells, 1, 1, 3, Rgb8::RED, 5);

        assert_eq!(raster_glyph(cells[0].packed()), '⢁');
        assert_eq!(cells[0].color, Some(Rgb8::RED));
    }

    #[test]
    fn country_wins_over_road_wins_over_region_mask() {
        let bounds = Bounds {
            min_x: 0.0,
            max_x: 1.0,
            min_y: 0.0,
            max_y: 1.0,
        };
        let mut mask = vec![None; 8];
        // Region first in the mask...
        mark_border_segment(
            &mut mask,
            bounds,
            4,
            2,
            0.25,
            0.25,
            0.75,
            0.25,
            BorderLineKind::Region,
        );
        // ... then Road on top -> Road wins
        mark_border_segment(
            &mut mask,
            bounds,
            4,
            2,
            0.25,
            0.25,
            0.75,
            0.25,
            BorderLineKind::Road,
        );

        assert!(mask.contains(&Some(BorderLineKind::Road)));
        assert!(!mask.contains(&Some(BorderLineKind::Region)));

        // ... then Country on top -> Country wins
        mark_border_segment(
            &mut mask,
            bounds,
            4,
            2,
            0.25,
            0.25,
            0.75,
            0.25,
            BorderLineKind::Country,
        );

        assert!(mask.contains(&Some(BorderLineKind::Country)));
        assert!(!mask.contains(&Some(BorderLineKind::Road)));
    }

    #[test]
    fn border_mask_dimensions_match_viewport() {
        let bounds = Bounds {
            min_x: 0.0,
            max_x: 1.0,
            min_y: 0.0,
            max_y: 1.0,
        };
        // Mock borders: a simple country polygon
        let mut line = crate::layers::BorderLine {
            kind: crate::layers::BorderLineKind::Country,
            points: vec![
                crate::geo::WorldPoint { x: 0.1, y: 0.1 },
                crate::geo::WorldPoint { x: 0.9, y: 0.1 },
                crate::geo::WorldPoint { x: 0.9, y: 0.9 },
                crate::geo::WorldPoint { x: 0.1, y: 0.9 },
            ],
            bbox: crate::geo::Bounds {
                min_x: 0.0,
                max_x: 0.0,
                min_y: 0.0,
                max_y: 0.0,
            },
        };
        line.compute_bbox();
        let borders = crate::layers::BorderLayer {
            resolution: crate::layers::BorderResolution::Low110m,
            lines: vec![line],
            grid: None,
        };
        let stamp = BorderMaskStamp {
            zoom_bits: 7.0_f64.to_bits(),
            resolution: BorderResolution::Regional10m,
            show_regions: true,
            show_roads: true,
            width: 10,
            height: 5,
            layers_version: 0,
        };
        let mask = border_mask_for_view(&borders, bounds, 10, 5, stamp);
        // mask should have width*2 * height*4 entries (subcell resolution)
        assert_eq!(mask.cells.len(), (10 * 2) * (5 * 4));
        // At least some border cells should be marked
        assert!(!mask.marks.is_empty());
        // Not all cells should be borders
        assert!(mask.cells.iter().any(|c| c.is_none()));
    }

    #[test]
    fn spatial_prefilter_skips_lines_outside_viewport() {
        // Viewport covers the upper-left quadrant only.
        let bounds = Bounds {
            min_x: 0.0,
            max_x: 0.5,
            min_y: 0.0,
            max_y: 0.5,
        };
        // A line wholly in the lower-right quadrant — bbox test should skip it.
        let mut off_screen = crate::layers::BorderLine {
            kind: crate::layers::BorderLineKind::Country,
            points: vec![
                crate::geo::WorldPoint { x: 0.8, y: 0.8 },
                crate::geo::WorldPoint { x: 0.9, y: 0.9 },
            ],
            bbox: crate::geo::Bounds {
                min_x: 0.0,
                max_x: 0.0,
                min_y: 0.0,
                max_y: 0.0,
            },
        };
        off_screen.compute_bbox();
        // A line crossing the viewport — should be drawn.
        let mut on_screen = crate::layers::BorderLine {
            kind: crate::layers::BorderLineKind::Country,
            points: vec![
                crate::geo::WorldPoint { x: 0.0, y: 0.25 },
                crate::geo::WorldPoint { x: 0.5, y: 0.25 },
            ],
            bbox: crate::geo::Bounds {
                min_x: 0.0,
                max_x: 0.0,
                min_y: 0.0,
                max_y: 0.0,
            },
        };
        on_screen.compute_bbox();
        let borders = crate::layers::BorderLayer {
            resolution: crate::layers::BorderResolution::Low110m,
            lines: vec![on_screen],
            grid: None,
        };
        let stamp = BorderMaskStamp {
            zoom_bits: 7.0_f64.to_bits(),
            resolution: BorderResolution::Low110m,
            show_regions: false,
            show_roads: false,
            width: 20,
            height: 20,
            layers_version: 0,
        };
        let mask = border_mask_for_view(&borders, bounds, 20, 20, stamp);
        // The off-screen line should contribute zero marks; the on-screen
        // line should contribute many.
        assert!(!mask.marks.is_empty(), "on-screen line should be drawn");
    }

    #[test]
    fn degenerate_bbox_does_not_hide_line() {
        // Regression: pre-bbox caches deserialize with a zero-area
        // Bounds::default(); the pre-filter must NOT use such a bbox
        // to hide the line from view.
        let bounds = Bounds {
            min_x: 0.4,
            max_x: 0.6,
            min_y: 0.4,
            max_y: 0.6,
        };
        // Mimic deserialised-from-old-cache line: bbox left at default.
        let line = crate::layers::BorderLine {
            kind: crate::layers::BorderLineKind::Country,
            points: vec![
                crate::geo::WorldPoint { x: 0.45, y: 0.45 },
                crate::geo::WorldPoint { x: 0.55, y: 0.55 },
            ],
            bbox: crate::geo::Bounds {
                min_x: 0.0,
                max_x: 0.0,
                min_y: 0.0,
                max_y: 0.0,
            },
        };
        let borders = crate::layers::BorderLayer {
            resolution: crate::layers::BorderResolution::Low110m,
            lines: vec![line],
            grid: None,
        };
        let stamp = BorderMaskStamp {
            zoom_bits: 7.0_f64.to_bits(),
            resolution: BorderResolution::Low110m,
            show_regions: false,
            show_roads: false,
            width: 20,
            height: 20,
            layers_version: 0,
        };
        let mask = border_mask_for_view(&borders, bounds, 20, 20, stamp);
        assert!(
            !mask.marks.is_empty(),
            "degenerate-bbox line must still render"
        );
    }

    #[test]
    fn raster_cell_default_values() {
        let cell = RasterCell::default();
        assert_eq!(cell.bits, 0);
        assert_eq!(cell.color, None);
        assert_eq!(cell.intensity, 0);
        assert_eq!(cell.glyph, None);
        assert_eq!(cell.bg, None);
    }

    #[test]
    fn compute_mask_cells_returns_subcell_grid() {
        // Spot-check that compute_mask_cells produces a non-empty grid
        // with marks at expected positions for a single line.
        let bounds = Bounds {
            min_x: 0.0,
            max_x: 1.0,
            min_y: 0.0,
            max_y: 1.0,
        };
        let mut line = crate::layers::BorderLine {
            kind: crate::layers::BorderLineKind::Country,
            points: vec![
                crate::geo::WorldPoint { x: 0.5, y: 0.0 },
                crate::geo::WorldPoint { x: 0.5, y: 1.0 },
            ],
            bbox: crate::geo::Bounds {
                min_x: 0.0,
                max_x: 0.0,
                min_y: 0.0,
                max_y: 0.0,
            },
        };
        line.compute_bbox();
        let layer = crate::layers::BorderLayer {
            resolution: crate::layers::BorderResolution::Low110m,
            lines: vec![line],
            grid: None,
        };
        let stamp = BorderMaskStamp {
            zoom_bits: 5.0_f64.to_bits(),
            resolution: BorderResolution::Low110m,
            show_regions: false,
            show_roads: false,
            width: 10,
            height: 5,
            layers_version: 0,
        };
        let cells = compute_mask_cells(&layer, bounds, 20, 20, stamp);
        assert_eq!(cells.len(), 400);
        assert!(
            cells.iter().any(|c| c.is_some()),
            "vertical line produces marks"
        );
    }

    #[test]
    fn blit_writes_glyphs_and_colors_into_the_buffer() {
        let mut cells = vec![RasterCell::default(); 6]; // 3 wide x 2 high
                                                        // Fill first cell
        cells[0].bits |= 0x01; // bottom-left dot
        cells[0].color = Some(Rgb8::GREEN);

        // Fill last cell
        cells[5].bits |= 0x80 | 0x40; // top-right and top-left in second column
        cells[5].color = Some(Rgb8::RED);

        let area = Rect::new(0, 0, 3, 2);
        let mut buf = Buffer::empty(area);
        blit_cells(&cells, area, 3, 2, &mut buf);

        // Braille glyph and colour land on the marked cells.
        assert_eq!(buf[(0, 0)].symbol(), "\u{2801}");
        assert_eq!(buf[(0, 0)].fg, to_terminal_color(Rgb8::GREEN));
        assert_eq!(buf[(2, 1)].symbol(), "\u{28c0}");
        assert_eq!(buf[(2, 1)].fg, to_terminal_color(Rgb8::RED));
        // Untouched cells render blank, not stale content.
        assert_eq!(buf[(1, 0)].symbol(), " ");
    }

    #[test]
    fn blit_respects_the_area_offset() {
        let cells = vec![
            RasterCell {
                bits: 0x01,
                color: Some(Rgb8::GREEN),
                ..RasterCell::default()
            };
            2
        ];
        // A 2x1 grid drawn into an area offset from the buffer origin.
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 3));
        blit_cells(&cells, Rect::new(1, 2, 2, 1), 2, 1, &mut buf);

        assert_eq!(buf[(1, 2)].symbol(), "\u{2801}");
        assert_eq!(buf[(2, 2)].symbol(), "\u{2801}");
        // Outside the area nothing was touched.
        assert_eq!(buf[(0, 2)].symbol(), " ");
        assert_eq!(buf[(3, 2)].symbol(), " ");
        assert_eq!(buf[(1, 0)].symbol(), " ");
    }

    #[test]
    fn scale_bar_seg_chars_snaps_to_even_divisors() {
        // At any ideal width the result must divide BAR_CHARS (20) evenly
        // and never be narrower than 4 chars.
        for ideal in [0usize, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 15, 20, 25, 50] {
            let seg = scale_bar_seg_chars(ideal);
            assert!(20 % seg == 0, "seg_chars={seg} must divide 20");
            assert!(seg >= 4, "seg_chars={seg} must be >= 4 for readability");
        }
        // Coarse zoom (ideal ≈ 10) should produce wide segments.
        assert_eq!(scale_bar_seg_chars(10), 10);
        // Fine zoom (ideal ≈ 5) should produce 4-segment bar.
        assert_eq!(scale_bar_seg_chars(5), 5);
    }

    #[test]
    fn scale_bar_label_matches_stripe_width() {
        // Verify that the label (nearest NICE to actual_seg_km) is within 50%
        // of the actual stripe distance.  This catches the bug where segment_km
        // was chosen before seg_chars was snapped, producing a ~2× mismatch.
        let cases: &[(f64, usize)] = &[
            (9.0, 2),   // kmpc=9, ideal_seg=2 → seg_chars snapped to 4; actual=36 km
            (9.0, 4),   // ideal_seg=4 → seg_chars=4; actual=36 km
            (9.79, 10), // ideal_seg=10 → seg_chars=10; actual≈98 km
            (2.45, 4),  // ideal_seg=4 → seg_chars=4; actual≈9.8 km
            (50.0, 5),  // ideal_seg=5 → seg_chars=5; actual=250 km
        ];
        for &(kmpc, ideal_seg) in cases {
            let seg_chars = scale_bar_seg_chars(ideal_seg);
            let actual_seg_km = seg_chars as f64 * kmpc;
            let label_km = NICE
                .iter()
                .copied()
                .min_by(|&a, &b| {
                    (a - actual_seg_km)
                        .abs()
                        .partial_cmp(&(b - actual_seg_km).abs())
                        .unwrap()
                })
                .unwrap_or(actual_seg_km);
            let ratio = label_km / actual_seg_km;
            assert!(
                (0.5..=2.0).contains(&ratio),
                "kmpc={kmpc} ideal={ideal_seg}: label={label_km} actual={actual_seg_km} ratio={ratio:.2}"
            );
        }
    }

    fn synthetic_radar_frame(z: u8, bounds: Bounds) -> RadarFrame {
        use crate::geo::visible_tiles;
        let tiles_coords = visible_tiles(bounds, z);
        let mut tiles = Vec::with_capacity(tiles_coords.len());
        for coord in tiles_coords {
            let size = 256u32;
            let mut rows = Vec::with_capacity(size as usize);
            for _ in 0..size {
                let row = vec![RadarRun {
                    start_x: 0,
                    end_x: size as u16,
                    color: Rgb8::new(180, 80, 160),
                    intensity: 3,
                }];
                rows.push(row);
            }
            tiles.push(RadarTile { coord, size, rows });
        }
        RadarFrame {
            time: 0,
            path: String::new(),
            tiles,
            missing_tiles: 0,
            target_zoom: z,
        }
    }

    #[test]
    fn bench_raster_radar_color_mode() {
        let width = 120u16;
        let height = 50u16;
        let z = 5u8;
        let bounds = Bounds {
            min_x: 0.35,
            max_x: 0.55,
            min_y: 0.30,
            max_y: 0.70,
        };
        let radar = synthetic_radar_frame(z, bounds);
        let modes = RenderModeState {
            braille: None,
            color: Some(LayerId::Radar),
            text: None,
            ..RenderModeState::new()
        };
        let cell_count = usize::from(width) * usize::from(height);
        let mut cells = vec![RasterCell::default(); cell_count];

        let iters = 500;
        let t0 = std::time::Instant::now();
        for _ in 0..iters {
            for c in &mut cells {
                c.clear();
            }
            raster_radar(&mut cells, &radar, bounds, width, height, &modes);
        }
        let elapsed = t0.elapsed();
        let per_frame_us = elapsed.as_micros() / iters;
        let per_frame_ms = per_frame_us as f64 / 1000.0;
        eprintln!(
            "bench: color mode — {iters} frames in {elapsed:?} = {per_frame_ms:.3} ms/frame ({:.0} fps max)",
            1000.0 / per_frame_ms
        );
        assert!(
            per_frame_ms < 10.0,
            "color mode should render in <10ms, got {per_frame_ms}ms"
        );
    }

    /// Render `radar` into a fresh grid, forcing the given rayon width so the
    /// sequential (1 thread) and banded-parallel paths can be compared.
    fn render_with_threads(
        threads: usize,
        radar: &RadarFrame,
        bounds: Bounds,
        width: u16,
        height: u16,
        modes: &RenderModeState,
    ) -> Vec<RasterCell> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap();
        pool.install(|| {
            let mut cells = vec![RasterCell::default(); usize::from(width) * usize::from(height)];
            raster_radar(&mut cells, radar, bounds, width, height, modes);
            cells
        })
    }

    #[test]
    fn parallel_bands_render_identically_to_sequential() {
        let bounds = Bounds {
            min_x: 0.35,
            max_x: 0.55,
            min_y: 0.30,
            max_y: 0.70,
        };
        let radar = synthetic_radar_frame(5, bounds);
        // Braille alone, and braille+colour+text together, exercise every
        // write path a band can take.
        let cases = [
            RenderModeState {
                braille: Some(LayerId::Radar),
                color: None,
                text: None,
                ..RenderModeState::new()
            },
            RenderModeState {
                braille: Some(LayerId::Radar),
                color: Some(LayerId::Radar),
                text: Some(LayerId::Radar),
                ..RenderModeState::new()
            },
        ];
        // Heights that divide evenly and that leave a short trailing band.
        for (width, height) in [(120u16, 50u16), (80, 17), (60, 5), (40, 3)] {
            for modes in &cases {
                let seq = render_with_threads(1, &radar, bounds, width, height, modes);
                for threads in [2usize, 4, 8] {
                    let par = render_with_threads(threads, &radar, bounds, width, height, modes);
                    assert_eq!(
                        seq, par,
                        "banded output differs at {width}x{height} with {threads} threads"
                    );
                }
            }
        }
    }

    #[test]
    fn bench_raster_radar_braille_mode() {
        let width = 120u16;
        let height = 50u16;
        let z = 5u8;
        let bounds = Bounds {
            min_x: 0.35,
            max_x: 0.55,
            min_y: 0.30,
            max_y: 0.70,
        };
        let radar = synthetic_radar_frame(z, bounds);
        let modes = RenderModeState {
            braille: Some(LayerId::Radar),
            color: None,
            text: None,
            ..RenderModeState::new()
        };
        let cell_count = usize::from(width) * usize::from(height);
        let mut cells = vec![RasterCell::default(); cell_count];

        let iters = 500;
        let t0 = std::time::Instant::now();
        for _ in 0..iters {
            for c in &mut cells {
                c.clear();
            }
            raster_radar(&mut cells, &radar, bounds, width, height, &modes);
        }
        let elapsed = t0.elapsed();
        let per_frame_us = elapsed.as_micros() / iters;
        let per_frame_ms = per_frame_us as f64 / 1000.0;
        eprintln!(
            "bench: braille mode — {iters} frames in {elapsed:?} = {per_frame_ms:.3} ms/frame ({:.0} fps max)",
            1000.0 / per_frame_ms
        );
        assert!(
            per_frame_ms < 20.0,
            "braille mode should render in <20ms, got {per_frame_ms}ms"
        );
    }

    /// Naive per-subcell reference rasteriser, used only to prove the
    /// optimised per-cell path in `raster_radar` produces identical output.
    fn raster_radar_reference(
        cells: &mut [RasterCell],
        radar: &RadarFrame,
        bounds: Bounds,
        width: u16,
        height: u16,
        modes: &RenderModeState,
    ) {
        let id = LayerId::Radar;
        let in_braille = modes.has(RenderMode::Braille, id);
        let in_color = modes.has(RenderMode::Color, id);
        let in_text = modes.has(RenderMode::Text, id);
        let sub_width = u32::from(width) * 2;
        let sub_height = u32::from(height) * 4;
        let cells_len = cells.len();
        let w_usize = usize::from(width);
        let sx_scale = sub_width as f64 / bounds.width().max(f64::EPSILON);
        let sy_scale = sub_height as f64 / bounds.height().max(f64::EPSILON);
        for tile in &radar.tiles {
            let tb = tile_bounds(tile.coord);
            if !bounds.intersects(tb) {
                continue;
            }
            let tww = tb.max_x - tb.min_x;
            let twh = tb.max_y - tb.min_y;
            let inv = 1.0 / f64::from(tile.size);
            for (row_index, runs) in tile.rows.iter().enumerate() {
                let wy0 = tb.min_y + row_index as f64 * inv * twh;
                let wy1 = tb.min_y + (row_index + 1) as f64 * inv * twh;
                let start_sy = (((wy0 - bounds.min_y) * sy_scale).floor() as i32)
                    .clamp(0, sub_height as i32) as u32;
                let end_sy = (((wy1 - bounds.min_y) * sy_scale).ceil() as i32)
                    .clamp(0, sub_height as i32) as u32;
                if start_sy >= end_sy {
                    continue;
                }
                for run in runs {
                    let wx0 = tb.min_x + f64::from(run.start_x) * inv * tww;
                    let wx1 = tb.min_x + f64::from(run.end_x) * inv * tww;
                    let start_sx = (((wx0 - bounds.min_x) * sx_scale).floor() as i32)
                        .clamp(0, sub_width as i32) as u32;
                    let end_sx = (((wx1 - bounds.min_x) * sx_scale).ceil() as i32)
                        .clamp(0, sub_width as i32) as u32;
                    if start_sx >= end_sx {
                        continue;
                    }
                    for sy in start_sy..end_sy {
                        let cell_y = (sy / 4) as usize;
                        let sub_y = sy % 4;
                        let row_base = cell_y * w_usize;
                        for sx in start_sx..end_sx {
                            let idx = row_base + (sx / 2) as usize;
                            if idx >= cells_len {
                                continue;
                            }
                            let cell = &mut cells[idx];
                            if in_braille {
                                cell.bits |= braille_bit(sx % 2, sub_y);
                                if run.intensity >= cell.intensity {
                                    cell.color = Some(run.color);
                                    cell.intensity = run.intensity;
                                }
                            }
                            if in_color {
                                cell.bg = Some(run.color);
                            }
                            if in_text {
                                cell.glyph = Some(radar_glyph(run.intensity));
                                cell.color = Some(run.color);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn raster_radar_matches_reference_all_modes() {
        let (width, height) = (120u16, 50u16);
        let bounds = Bounds {
            min_x: 0.35,
            max_x: 0.55,
            min_y: 0.30,
            max_y: 0.70,
        };
        let radar = synthetic_radar_frame(5, bounds);
        let n = usize::from(width) * usize::from(height);
        let mode = |b: bool, c: bool, t: bool| RenderModeState {
            braille: b.then_some(LayerId::Radar),
            color: c.then_some(LayerId::Radar),
            text: t.then_some(LayerId::Radar),
            ..RenderModeState::new()
        };
        // braille, color, text, and braille+color / braille+text overlays.
        for (b, c, t) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
            (true, true, false),
            (true, false, true),
        ] {
            let modes = mode(b, c, t);
            let mut got = vec![RasterCell::default(); n];
            let mut want = vec![RasterCell::default(); n];
            raster_radar(&mut got, &radar, bounds, width, height, &modes);
            raster_radar_reference(&mut want, &radar, bounds, width, height, &modes);
            for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
                assert_eq!(
                    g.packed(),
                    w.packed(),
                    "cell {i} packed mismatch (b={b} c={c} t={t})"
                );
                assert_eq!(
                    g.intensity, w.intensity,
                    "cell {i} intensity mismatch (b={b} c={c} t={t})"
                );
            }
        }
    }

    #[test]
    fn bench_blit_cells() {
        let width = 120u16;
        let height = 50u16;
        let cell_count = usize::from(width) * usize::from(height);
        let cells = vec![
            RasterCell {
                bits: 0x55,
                color: Some(Rgb8::new(180, 80, 160)),
                intensity: 3,
                glyph: None,
                bg: None,
                modifier: Modifier::empty(),
            };
            cell_count
        ];
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);

        let iters = 500;
        let t0 = std::time::Instant::now();
        for _ in 0..iters {
            blit_cells(&cells, area, width, height, &mut buf);
        }
        let elapsed = t0.elapsed();
        let per_frame_us = elapsed.as_micros() / iters;
        let per_frame_ms = per_frame_us as f64 / 1000.0;
        eprintln!(
            "bench: blit_cells — {iters} frames in {elapsed:?} = {per_frame_ms:.3} ms/frame ({:.0} fps max)",
            1000.0 / per_frame_ms
        );
        assert!(
            per_frame_ms < 5.0,
            "blit_cells should complete in <5ms, got {per_frame_ms}ms"
        );
    }
}
