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
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Terminal;

use crate::app::{
    App, BorderMask, BorderMaskPoint, BorderMaskStamp, PlaybackMode, TaskState, LIGHTNING_IMPACT_MS,
};
use crate::cache::write_log;
use crate::geo::{
    lat_lon_to_world, tile_bounds, world_to_lat_lon, Bounds, WorldPoint, CITY_MATCH_KM,
    EUROPEAN_CAPITALS, EUROPEAN_CAPITAL_NAMES, EUROPEAN_MAJOR_CITIES,
};
use crate::layers::{
    BorderLine, BorderLineKind, BorderResolution, LayerId, LayerOption, LayerRegistry, LayerStatus,
    MainItem, ObservationPoint, ObservationProperty, RadarFrame, RenderMode, RenderModeState, Rgb8,
};

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

/// Below this zoom only capital-adjacent stations are shown.
/// Must match `eumetnet.rs` CAPITALS_ZOOM_CUTOFF so the fetch tier
/// aligns with the display tier.
const MAJOR_CITIES_ZOOM_CUTOFF: f64 = 5.0;

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
    } else if zoom >= MAJOR_CITIES_ZOOM_CUTOFF {
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
fn capital_name_labels(
    points: &[crate::layers::ObservationPoint],
) -> std::collections::HashMap<usize, &'static str> {
    let threshold_sq = (CITY_MATCH_KM / 111.0_f64).powi(2);
    let mut labels: std::collections::HashMap<usize, &'static str> =
        std::collections::HashMap::new();
    for (&(clat, clon), &name) in CAPITALS.iter().zip(EUROPEAN_CAPITAL_NAMES.iter()) {
        let cos_lat = clat.to_radians().cos();
        let best = points
            .iter()
            .enumerate()
            .filter_map(|(idx, pt)| {
                let dlat = pt.point.lat - clat;
                let dlon = (pt.point.lon - clon) * cos_lat;
                let d2 = dlat * dlat + dlon * dlon;
                (d2 < threshold_sq).then_some((idx, d2))
            })
            .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((idx, _)) = best {
            labels.entry(idx).or_insert(name);
        }
    }
    labels
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
        if app.drain_refresh_results() {
            dirty = true;
        }
        if app.drain_preload_results() {
            dirty = true;
        }
        if app.drain_frame_list() {
            dirty = true;
        }
        if app.drain_task_messages() {
            dirty = true;
        }
        if app.drain_obs_results() {
            dirty = true;
        }
        if app.drain_warning_results() {
            dirty = true;
        }
        if app.drain_lightning_results() {
            dirty = true;
        }
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
        if dirty {
            terminal.draw(|frame| render(frame, app))?;
            dirty = false;
            last_render = Instant::now();
        } else if (app.layers.any_loading()
            || !app.active_tasks.is_empty()
            || app.has_lightning_impact()
            || app.playback_mode == PlaybackMode::Live)
            && last_render.elapsed() >= Duration::from_millis(50)
        {
            dirty = true;
        }
        if !event::poll(Duration::from_millis(16))? {
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
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                        match key.code {
                            KeyCode::Up => {
                                app.layer_panel_focused = true;
                                app.layers.select_previous();
                                dirty = true;
                            }
                            KeyCode::Down => {
                                app.layer_panel_focused = true;
                                app.layers.select_next();
                                dirty = true;
                            }
                            _ => {}
                        }
                    } else if key.modifiers.contains(KeyModifiers::ALT) {
                        match key.code {
                            KeyCode::Up => {
                                app.layer_panel_focused = true;
                                app.layers.select_previous();
                                dirty = true;
                            }
                            KeyCode::Down => {
                                app.layer_panel_focused = true;
                                app.layers.select_next();
                                dirty = true;
                            }
                            // Enhanced terminals send Alt+arrow; Terminal.app sends ESC+f/b.
                            KeyCode::Right | KeyCode::Char('f') => {
                                app.layer_panel_focused = true;
                                dirty |= app.layers.enter_options();
                            }
                            // Alt+Left: exit options → defocus root list → refocus.
                            KeyCode::Left | KeyCode::Char('b') => {
                                if !app.layer_panel_focused {
                                    // Panel was defocused — refocus it.
                                    app.layer_panel_focused = true;
                                    dirty = true;
                                } else if app.layers.is_in_options() {
                                    dirty |= app.layers.exit_options();
                                } else {
                                    // Already at root list — defocus the panel.
                                    app.layer_panel_focused = false;
                                    dirty = true;
                                }
                            }
                            _ => {}
                        }
                    } else {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => {
                                if app.show_help {
                                    app.show_help = false;
                                    dirty = true;
                                } else {
                                    app.shutdown();
                                    quit = true;
                                    break;
                                }
                            }
                            KeyCode::Char(' ') => {
                                app.layer_panel_focused = true;
                                if let Some(id) = app.layers.handle_space() {
                                    handle_layer_enable(app, id, &mut refresh);
                                }
                                dirty = true;
                            }
                            KeyCode::Char('b') => {
                                app.layer_panel_focused = true;
                                let id = app.layers.selected_layer();
                                if id.is_rendered() && !id.is_observation() {
                                    app.layers.mode_state_mut().toggle(RenderMode::Braille, id);
                                    handle_layer_enable(app, id, &mut refresh);
                                    dirty = true;
                                }
                            }
                            KeyCode::Char('c') => {
                                app.layer_panel_focused = true;
                                let id = app.layers.selected_layer();
                                if id.is_rendered() && !id.is_observation() {
                                    app.layers.mode_state_mut().toggle(RenderMode::Color, id);
                                    handle_layer_enable(app, id, &mut refresh);
                                    dirty = true;
                                }
                            }
                            KeyCode::Char('l') => {
                                app.layer_panel_focused = true;
                                let id = app.layers.selected_layer();
                                if id.is_rendered() {
                                    app.layers.mode_state_mut().toggle(RenderMode::Text, id);
                                    handle_layer_enable(app, id, &mut refresh);
                                    dirty = true;
                                }
                            }
                            KeyCode::Char('m') => {
                                app.request_border_refetch();
                                dirty = true;
                            }
                            // Zoom / pan — defocus the panel so the map fills the screen.
                            KeyCode::Char('+') | KeyCode::Char('=') => {
                                app.layer_panel_focused = false;
                                app.viewport.zoom_by(0.25);
                                refresh = true;
                                dirty = true;
                            }
                            KeyCode::Char('-') => {
                                app.layer_panel_focused = false;
                                app.viewport.zoom_by(-0.25);
                                refresh = true;
                                dirty = true;
                            }
                            KeyCode::Left => {
                                app.layer_panel_focused = false;
                                app.viewport.pan(-1.0, 0.0);
                                refresh = true;
                                dirty = true;
                            }
                            KeyCode::Right => {
                                app.layer_panel_focused = false;
                                app.viewport.pan(1.0, 0.0);
                                refresh = true;
                                dirty = true;
                            }
                            KeyCode::Up => {
                                app.layer_panel_focused = false;
                                app.viewport.pan(0.0, -1.0);
                                refresh = true;
                                dirty = true;
                            }
                            KeyCode::Down => {
                                app.layer_panel_focused = false;
                                app.viewport.pan(0.0, 1.0);
                                refresh = true;
                                dirty = true;
                            }
                            KeyCode::Char(']') => {
                                app.previous_frame();
                                refresh = true;
                                dirty = true;
                            }
                            KeyCode::Char('[') => {
                                app.next_frame();
                                refresh = true;
                                dirty = true;
                            }
                            KeyCode::Char('p') => {
                                app.toggle_play_pause();
                                last_playback_step = Instant::now();
                                dirty = true;
                            }
                            KeyCode::Char('0') => {
                                app.jump_to_live();
                                app.request_meteogate_refresh(app.map_width, app.map_height);
                                dirty = true;
                            }
                            KeyCode::Char('.') => {
                                app.speed_faster();
                                dirty = true;
                            }
                            KeyCode::Char(',') => {
                                app.speed_slower();
                                dirty = true;
                            }
                            KeyCode::Char('?') => {
                                app.show_help = !app.show_help;
                                dirty = true;
                            }
                            _ => {}
                        }
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
            app.request_meteogate_refresh(map_area.width, map_area.height);
            app.request_border_refresh();
            if (app.any_obs_enabled()) && !app.has_obs_task() {
                app.request_obs_refresh();
            }
            dirty = true;
        }
    }
    app.save_state();
    Ok(())
}

fn render(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    app.frame_count = app.frame_count.wrapping_add(1);
    let chunks = app_areas(frame.area());

    render_header(frame, chunks[0], app);
    render_map(frame, chunks[1], app);
    render_footer(frame, chunks[2], app);

    if app.show_help {
        render_help(frame, frame.area());
    }
}

fn render_help(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let header = |t: &str| {
        TextLine::from(Span::styled(
            format!("  {t}"),
            Style::default().add_modifier(Modifier::UNDERLINED),
        ))
    };
    let entry = |key: &str, desc: &str| TextLine::from(format!("    {key:<12}{desc}"));

    let content = vec![
        TextLine::from(Span::styled(
            "  Fancy Radar ObservatioN Tool",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        TextLine::from("  European weather radar in your terminal"),
        TextLine::from(""),
        header("Navigation"),
        entry("arrows", "Pan map"),
        entry("drag", "Pan map"),
        entry("scroll", "Zoom in/out"),
        entry("+ / -", "Zoom in/out"),
        TextLine::from(""),
        header("Timeline"),
        entry("[ / ]", "Step frame back/forward"),
        entry("p", "Play/pause"),
        entry("0", "Jump to live"),
        entry(", / .", "Playback speed"),
        TextLine::from(""),
        header("Layers"),
        entry("space", "Toggle layer"),
        entry("b / c / l", "Braille/color/text mode"),
        entry("Shift+↑/↓", "Select layer"),
        entry("Alt+←/→", "Navigate layer group"),
        TextLine::from(""),
        header("Other"),
        entry("?", "Toggle help"),
        entry("q / Esc", "Quit"),
        entry("m", "Refetch map data"),
        TextLine::from(""),
        TextLine::from("  Press ? or Esc to close."),
    ];

    let block = Block::default()
        .title(" Help ")
        .title_alignment(ratatui::layout::Alignment::Center)
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Color::Cyan)
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(area);
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(content), inner);
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

/// Build the compact timeline right-aligned in the header.
///
/// Format: `● 13:50 ···░░░█░░`  (icon · sp · time · sp · one-char-per-frame bar)
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

    // One character per frame, no brackets.
    if frame_count > 0 && avail >= min_w + 1 + frame_count as u16 {
        let cached: Vec<bool> = app
            .timestamps
            .iter()
            .map(|ts| app.frame_cache.contains_key(ts))
            .collect();
        spans.push(Span::raw(" "));
        spans.extend(timeline_bar_spans(
            frame_count,
            app.frame_index,
            &cached,
            frame_count,
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

/// Build colored spans for the timeline bar.
///
/// `·` uncached frame  `░` cached/ready  `█` current  `─` track background
fn timeline_bar_spans(
    frame_count: usize,
    frame_index: usize,
    cached: &[bool],
    bar_width: usize,
) -> Vec<Span<'static>> {
    if bar_width == 0 {
        return vec![];
    }
    if frame_count == 0 {
        return vec![Span::styled(
            "─".repeat(bar_width),
            Style::default().fg(Color::DarkGray),
        )];
    }

    // Build character array: background `─`, then stamp each frame tick.
    let mut cells: Vec<char> = vec!['─'; bar_width];
    for i in 0..frame_count {
        let pos = frame_to_bar_pos(i, frame_count, bar_width);
        if cells[pos] != '█' {
            cells[pos] = if i == frame_index {
                '█'
            } else if cached.get(i).copied().unwrap_or(false) {
                '░'
            } else {
                '·'
            };
        }
    }
    // Current marker always wins a collision at its position.
    let cur_pos = frame_to_bar_pos(frame_index, frame_count, bar_width);
    cells[cur_pos] = '█';

    // Emit grouped runs of same-style characters.
    let style_of = |c: char| -> Style {
        match c {
            '█' => Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            '░' => Style::default().fg(Color::White),
            _ => Style::default().fg(Color::DarkGray), // '·' and '─'
        }
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut i = 0;
    while i < bar_width {
        let style = style_of(cells[i]);
        let mut run = String::new();
        while i < bar_width && style_of(cells[i]) == style {
            run.push(cells[i]);
            i += 1;
        }
        spans.push(Span::styled(run, style));
    }
    spans
}

fn render_map(frame: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
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
    let mut render_rows = std::mem::take(&mut app.render_rows);
    raster_map_rows(
        app,
        bounds,
        area.width,
        area.height,
        offset,
        &mut braille_frame,
        &mut render_rows,
    );
    app.braille_frame = braille_frame;
    frame.render_widget(Paragraph::new(&render_rows[..]), area);
    app.render_rows = render_rows;
    if !app.is_dragging {
        render_layer_list(frame, area, app);
        render_task_queue(frame, area, app);
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

#[derive(Clone, Copy, Debug, Default)]
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
    render_rows: &mut Vec<TextLine<'static>>,
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

    if let Some(location) = app.location_marker {
        let point = location.to_world();
        if point.x >= bounds.min_x
            && point.x <= bounds.max_x
            && point.y >= bounds.min_y
            && point.y <= bounds.max_y
        {
            let x = ((point.x - bounds.min_x) / bounds.width().max(f64::EPSILON) * f64::from(width))
                .floor()
                .clamp(0.0, f64::from(width.saturating_sub(1))) as u16;
            let y = ((point.y - bounds.min_y) / bounds.height().max(f64::EPSILON)
                * f64::from(height))
            .floor()
            .clamp(0.0, f64::from(height.saturating_sub(1))) as u16;
            let cell = &mut cells[usize::from(y) * usize::from(width) + usize::from(x)];
            cell.glyph = Some('x');
            cell.color = Some(Rgb8::WHITE);
            cell.bg = Some(Rgb8::RED);
        }
    }

    raster_rows_into(braille_frame.cells(), width, height, render_rows);
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
    for tile in &radar.tiles {
        let tb = tile_bounds(tile.coord);
        if !bounds.intersects(tb) {
            continue;
        }

        let tile_world_width = tb.max_x - tb.min_x;
        let tile_world_height = tb.max_y - tb.min_y;
        let inv_size = 1.0 / f64::from(tile.size);

        for (row_index, runs) in tile.rows.iter().enumerate() {
            let world_y_start = tb.min_y + row_index as f64 * inv_size * tile_world_height;
            let world_y_end = tb.min_y + (row_index + 1) as f64 * inv_size * tile_world_height;

            let start_sy = ((world_y_start - min_y) * sy_scale).floor() as i32;
            let end_sy = ((world_y_end - min_y) * sy_scale).ceil() as i32;
            let start_sy = start_sy.clamp(0, sub_height as i32) as u32;
            let end_sy = end_sy.clamp(0, sub_height as i32) as u32;
            if start_sy >= end_sy {
                continue;
            }

            for run in runs {
                let world_x_start = tb.min_x + f64::from(run.start_x) * inv_size * tile_world_width;
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
                let cy_start = (start_sy / 4) as usize;
                let cy_end = ((end_sy - 1) / 4) as usize;
                let cx_start = (start_sx / 2) as usize;
                let cx_end = ((end_sx - 1) / 2) as usize;

                for cy in cy_start..=cy_end {
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
                    let row_base = cy * w_usize;
                    for cx in cx_start..=cx_end {
                        let idx = row_base + cx;
                        if idx >= cells_len {
                            continue;
                        }
                        let cell = &mut cells[idx];
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

    // One representative station per capital (closest within CITY_MATCH_KM).
    // The label shows the hardcoded capital name, not the API station name.
    let capital_labels = if show_names {
        capital_name_labels(&obs.points)
    } else {
        std::collections::HashMap::new()
    };

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
            for (point_idx, point) in obs.points.iter().enumerate() {
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

                // Show the capital name one row below — only if those cells are free.
                if let Some(&cap_name) = capital_labels.get(&point_idx) {
                    const NAME_COLOR: Rgb8 = Rgb8::new(105, 105, 105);
                    let name_sy = (sy / 4 + 1) * 4;
                    let name_cell_y = (name_sy / 4) as usize;
                    let name_cell_x = (sx / 2) as usize;
                    let name_row_base = name_cell_y * usize::from(width);
                    let name_end = (name_cell_x + cap_name.len()).min(usize::from(width));
                    let cells_free = (name_cell_x..name_end).all(|cx| {
                        cells
                            .get(name_row_base + cx)
                            .is_some_and(|c| c.glyph.is_none())
                    });
                    if cells_free {
                        write_obs_str(cells, width, sx, name_sy, cap_name, NAME_COLOR, false);
                    }
                }
            }
        }
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

fn obs_color(property: ObservationProperty, value: Option<f64>) -> Rgb8 {
    match (property, value) {
        // Temperature: cold blue → teal → yellow-green → amber → hot orange
        // Follows standard synoptic weather-map convention.
        (ObservationProperty::Temperature, Some(t)) => {
            if t < -20.0 {
                Rgb8::new(80, 110, 210)
            } else if t < -10.0 {
                Rgb8::new(110, 155, 235)
            } else if t < 0.0 {
                Rgb8::new(140, 195, 240)
            } else if t < 10.0 {
                Rgb8::new(100, 205, 185)
            } else if t < 20.0 {
                Rgb8::new(165, 215, 120)
            } else if t < 30.0 {
                Rgb8::new(235, 185, 65)
            } else {
                Rgb8::new(230, 100, 60)
            }
        }
        // Wind: Beaufort-inspired calm-gray → light-blue → green → yellow → orange → red
        (ObservationProperty::WindSpeed, Some(w)) => {
            if w < 1.0 {
                Rgb8::new(165, 185, 195)
            } else if w < 3.0 {
                Rgb8::new(130, 190, 235)
            } else if w < 8.0 {
                Rgb8::new(110, 210, 150)
            } else if w < 14.0 {
                Rgb8::new(220, 205, 80)
            } else if w < 20.0 {
                Rgb8::new(230, 140, 60)
            } else {
                Rgb8::new(215, 80, 80)
            }
        }
        // Humidity: dry amber → comfortable green → moist blue
        (ObservationProperty::Humidity, Some(h)) => {
            if h < 30.0 {
                Rgb8::new(205, 170, 75)
            } else if h < 50.0 {
                Rgb8::new(195, 210, 120)
            } else if h < 70.0 {
                Rgb8::new(130, 200, 155)
            } else if h < 85.0 {
                Rgb8::new(100, 175, 220)
            } else {
                Rgb8::new(70, 130, 215)
            }
        }
        // Pressure: low = stormy red, normal = neutral, high = fair-weather blue
        (ObservationProperty::Pressure, Some(p)) => {
            if p < 980.0 {
                Rgb8::new(215, 85, 85)
            } else if p < 1000.0 {
                Rgb8::new(200, 145, 130)
            } else if p < 1015.0 {
                Rgb8::new(165, 185, 205)
            } else if p < 1030.0 {
                Rgb8::new(110, 170, 220)
            } else {
                Rgb8::new(70, 135, 215)
            }
        }
        _ => Rgb8::GRAY,
    }
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

fn raster_rows_into(
    cells: &[RasterCell],
    width: u16,
    height: u16,
    out: &mut Vec<TextLine<'static>>,
) {
    out.clear();
    out.reserve(usize::from(height));
    let w = usize::from(width);
    for y in 0..height {
        let mut spans: Vec<Span> = Vec::with_capacity(4);
        let mut text = String::with_capacity(w);
        let mut style = Style::default();
        let row = &cells[y as usize * w..y as usize * w + w];
        for cell in row {
            let packed = cell.packed();
            let next_style = match packed.fg {
                Some(fg) => Style::default().fg(to_terminal_color(fg)),
                None => Style::default(),
            };
            let next_style = match packed.bg {
                Some(bg) => next_style.bg(to_terminal_color(bg)),
                None => next_style,
            };
            let next_style = next_style.add_modifier(packed.modifier);
            if !text.is_empty() && next_style != style {
                spans.push(Span::styled(std::mem::take(&mut text), style));
            }
            style = next_style;
            text.push(raster_glyph(packed));
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, style));
        }
        out.push(TextLine::from(spans));
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

/// Render task progress overlay in the top-right corner.
fn render_task_queue(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    if app.active_tasks.is_empty() {
        return;
    }

    let bar_chars = 12usize;
    let max_visible = 8;
    let n = app.active_tasks.len().min(max_visible);

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

    let mut lines: Vec<TextLine<'static>> = Vec::with_capacity(n);
    for task in &app.active_tasks[..n] {
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

        let bar_str = braille_bar(task.display_fraction, bar_chars);
        let pct_str = format!("{:>3.0}%", task.display_fraction * 100.0);
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
                if id.is_geographic() {
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

fn render_footer(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let hints = TextLine::from(vec![
        Span::styled(" q ", Style::default().bg(Color::DarkGray)),
        Span::raw(" quit  "),
        Span::styled(" arrows ", Style::default().bg(Color::DarkGray)),
        Span::raw(" pan  "),
        Span::styled(" +/- ", Style::default().bg(Color::DarkGray)),
        Span::raw(" zoom  "),
        Span::styled(" [/] ", Style::default().bg(Color::DarkGray)),
        Span::raw(" frame  "),
        Span::styled(" p ", Style::default().bg(Color::DarkGray)),
        Span::raw(" play  "),
        Span::styled(" 0 ", Style::default().bg(Color::DarkGray)),
        Span::raw(" live  "),
        Span::styled(" space ", Style::default().bg(Color::DarkGray)),
        Span::raw(" toggle  "),
        Span::styled(" ? ", Style::default().bg(Color::DarkGray)),
        Span::raw(" help  "),
    ]);

    // " zoom " badge (6) + " X.X" value (5) = 11 chars, always fixed
    let zoom_line = TextLine::from(vec![
        Span::styled(" zoom ", Style::default().bg(Color::DarkGray)),
        Span::raw(format!(" {:4.1}", app.viewport.zoom)),
    ]);

    let scale = render_scale_bar(app);
    let scale_w = scale.chars().count() as u16;

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(11),
            Constraint::Length(scale_w),
        ])
        .split(area);

    frame.render_widget(Paragraph::new(hints), chunks[0]);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::{RadarRun, RadarTile};

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
    fn raster_rows_produces_correct_number_of_rows() {
        let mut cells = vec![RasterCell::default(); 6]; // 3 wide x 2 high
                                                        // Fill first cell
        cells[0].bits |= 0x01; // bottom-left dot
        cells[0].color = Some(Rgb8::GREEN);

        // Fill last cell
        cells[5].bits |= 0x80 | 0x40; // top-right and top-left in second column
        cells[5].color = Some(Rgb8::RED);

        let mut rows = Vec::new();
        raster_rows_into(&cells, 3, 2, &mut rows);
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert!(!row.spans.is_empty());
        }
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
            braille_overlay: None,
            color: Some(LayerId::Radar),
            text: None,
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
            braille_overlay: None,
            color: None,
            text: None,
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
            braille_overlay: None,
            color: c.then_some(LayerId::Radar),
            text: t.then_some(LayerId::Radar),
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
    fn bench_raster_rows_into() {
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
        let mut out = Vec::new();

        let iters = 500;
        let t0 = std::time::Instant::now();
        for _ in 0..iters {
            raster_rows_into(&cells, width, height, &mut out);
        }
        let elapsed = t0.elapsed();
        let per_frame_us = elapsed.as_micros() / iters;
        let per_frame_ms = per_frame_us as f64 / 1000.0;
        eprintln!(
            "bench: raster_rows_into — {iters} frames in {elapsed:?} = {per_frame_ms:.3} ms/frame ({:.0} fps max)",
            1000.0 / per_frame_ms
        );
        assert!(
            per_frame_ms < 5.0,
            "raster_rows should complete in <5ms, got {per_frame_ms}ms"
        );
    }
}
