use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};
use color_eyre::eyre::{Result, WrapErr};
use reqwest::Client;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::cache::{write_log, FrontDirs};
use crate::cli::Cli;
use crate::config::{Config, LayerRenderMode, StateConfig};
use crate::geo::{world_to_lat_lon, GeoPoint, Viewport, WorldPoint};
use crate::layers::{
    resolution_distance, BorderLayer, BorderLine, BorderLineKind, BorderResolution, LayerId,
    LayerRegistry, LayerStatus, ObservationLayer, ObservationPoint, RadarFrame, RadarTile,
    RenderMode, WarningLayer,
};
use crate::providers::eumetnet::EumetnetProvider;
use crate::providers::maps::NaturalEarthProvider;
use crate::providers::meteoalarm::MeteoAlarmProvider;
use crate::providers::meteogate::MeteoGateProvider;
use crate::ui::BrailleFrame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderMaskStamp {
    pub zoom_bits: u64,
    pub resolution: BorderResolution,
    pub show_regions: bool,
    pub show_roads: bool,
    pub width: u16,
    pub height: u16,
    /// Bumped every time the set of loaded border layers changes
    /// (insert into / removal from `border_layers`).  The renderer
    /// uses this to invalidate the mask cache when new data arrives,
    /// so the user sees best-effort delivery: whichever layers are
    /// currently loaded are drawn, even if the desired one is still
    /// fetching.
    pub layers_version: u64,
}

#[derive(Debug)]
pub struct BorderMask {
    pub cells: Vec<Option<BorderLineKind>>,
    pub marks: Vec<BorderMaskPoint>,
    /// The viewport center (world coords) for which this mask was
    /// computed.  When the camera pans the renderer shifts marks
    /// by the subcell offset from this stored centre to the current
    /// one, avoiding a full mask recompute.
    pub center: WorldPoint,
}

#[derive(Debug, Clone, Copy)]
pub struct BorderMaskPoint {
    pub sx: u32,
    pub sy: u32,
    pub kind: BorderLineKind,
}

#[derive(Debug)]
pub struct App {
    pub viewport: Viewport,
    pub layers: LayerRegistry,
    pub borders: Option<BorderLayer>,
    pub timestamps: Vec<i64>,
    pub radar_frame: Option<RadarFrame>,
    pub border_mask_cache: Option<(BorderMaskStamp, BorderMask)>,
    /// Mask from the _previous_ resolution level, kept alive for one
    /// frame across resolution cutovers to avoid a blank flash while
    /// the new mask is computed.
    pub fallback_mask_cache: Option<(BorderMaskStamp, BorderMask)>,
    pub border_layers: HashMap<BorderResolution, BorderLayer>,
    /// Monotonic counter incremented whenever `border_layers` is
    /// mutated.  Baked into the `BorderMaskStamp` so the renderer
    /// invalidates its mask as soon as new data lands.
    pub border_layers_version: u64,
    /// Border loading progress for the UI.
    pub border_tiles_built: u32,
    pub border_total_resolutions: u32,
    /// Tracks which resolutions have had tiles built (dedup).
    border_built_set: HashSet<BorderResolution>,
    frame_list_tx: UnboundedSender<Vec<i64>>,
    frame_list_rx: UnboundedReceiver<Vec<i64>>,
    pub braille_frame: BrailleFrame,
    pub frame_count: u64,
    pub frame_index: usize,
    pub show_help: bool,
    /// False when the layer panel is defocused (dimmed, no selection indicators,
    /// submenu hidden).  Toggled by Alt+← from the root list; set to true by
    /// any layer interaction.  True on startup.
    pub layer_panel_focused: bool,
    pub location_label: String,
    pub location_marker: Option<GeoPoint>,
    pub dirs: FrontDirs,
    pub config: Config,
    pub warning_layer: Option<WarningLayer>,
    pub obs_cache: Option<ObservationLayer>,
    /// Points streaming in from the in-flight observation refresh.
    obs_incoming: Vec<ObservationPoint>,
    /// Refresh id the `obs_incoming` buffer belongs to.
    obs_incoming_id: u64,
    /// Accumulates data across multiple PartialCommit signals within a single
    /// refresh so progressive phase commits extend rather than replace.
    obs_partial: Vec<ObservationPoint>,
    /// Last‑known map‑area dimensions, used when requesting observation
    /// refreshes (the viewport needs width/height to compute bounds).
    pub map_width: u16,
    pub map_height: u16,
    pub pending_warning_refresh: bool,
    /// When an observation refresh was last *started* — successful or
    /// not.  Basing staleness on attempts rather than successes gives a
    /// natural backoff: a failing endpoint is retried every poll
    /// interval instead of on every event-loop tick.
    pub obs_last_attempt: Option<Instant>,
    /// When a warning refresh was last started (same backoff rationale).
    pub warn_last_attempt: Option<Instant>,
    /// The timestamp the current radar refresh was requested for.  Used
    /// to detect frame stepping: tile coverage alone can't tell frames
    /// of different times apart.
    radar_requested_ts: Option<i64>,
    /// The request timestamp of the tiles currently in `radar_frame`.
    /// When it differs from `radar_requested_ts`, incoming streamed
    /// tiles belong to a different frame time and the old tiles must be
    /// evicted instead of merged.
    radar_frame_ts: Option<i64>,
    maps: NaturalEarthProvider,
    meteogate: MeteoGateProvider,
    meteoalarm: MeteoAlarmProvider,
    eumetnet: EumetnetProvider,
    refresh_tx: UnboundedSender<RadarRefreshResult>,
    refresh_rx: UnboundedReceiver<RadarRefreshResult>,
    refresh_task: Option<JoinHandle<()>>,
    refresh_id: u64,
    border_tx: UnboundedSender<BorderRefreshResult>,
    border_rx: UnboundedReceiver<BorderRefreshResult>,
    border_task: Option<JoinHandle<()>>,
    border_refresh_id: u64,
    /// The resolution that the current `border_task` is fetching, so
    /// `request_border_refresh_with_cache` can avoid restarting a task
    /// that is already downloading the right dataset.
    border_fetch_resolution: Option<BorderResolution>,
    /// Per‑task cancellation flag for the current border task's
    /// `spawn_blocking` work (GeoJSON parse / simplify).  Set to
    /// `true` when the task is aborted so the blocking thread exits
    /// promptly instead of continuing as a zombie on the blocking
    /// pool.
    border_spawn_cancel: Option<Arc<AtomicBool>>,
    obs_tx: UnboundedSender<ObsRefreshResult>,
    obs_rx: UnboundedReceiver<ObsRefreshResult>,
    obs_task: Option<JoinHandle<()>>,
    obs_refresh_id: u64,
    warn_tx: UnboundedSender<WarnRefreshResult>,
    warn_rx: UnboundedReceiver<WarnRefreshResult>,
    warn_task: Option<JoinHandle<()>>,
    warn_refresh_id: u64,
    /// Pre‑load border tasks (one per resolution).  Stored so they can
    /// be aborted on quit — they're not tied to a specific request.
    preload_tasks: Vec<JoinHandle<()>>,
    pub task_rx: UnboundedReceiver<TaskMsg>,
    pub task_tx: UnboundedSender<TaskMsg>,
    pub active_tasks: Vec<ActiveTask>,
    /// Shared cancellation flag.  Set to `true` on quit so that
    /// `spawn_blocking` threads (tile generation, cache loading)
    /// exit promptly instead of keeping the process alive.
    pub cancel: Arc<AtomicBool>,
}

impl App {
    pub async fn boot(cli: &Cli) -> Result<Self> {
        let dirs = FrontDirs::new()?;
        let log = dirs.log_path.clone();
        write_log(&log, "=== front boot ===");
        if cli.clear_cache {
            write_log(&log, "boot: clearing cache");
            dirs.clear_cache()?;
        }
        // Radar frames are only browsable for the last hour; anything
        // older than a day is dead weight on disk.
        dirs.prune_radar_cache(Duration::from_secs(24 * 3600));
        write_log(&log, "boot: loading config");
        let config = Config::load(&dirs.config_dir.join("config.toml")).wrap_err("load config")?;
        write_log(&log, "boot: building HTTP client");
        let client = Client::builder()
            .user_agent("front/0.1.0")
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .build()
            .wrap_err("build HTTP client")?;

        write_log(&log, "boot: initial viewport");
        let (viewport, location_label, location_marker) = initial_viewport(cli, &log).await;
        let (refresh_tx, refresh_rx) = unbounded_channel();
        let (border_tx, border_rx) = unbounded_channel();
        let (obs_tx, obs_rx) = unbounded_channel();
        let (warn_tx, warn_rx) = unbounded_channel();
        let (task_tx, task_rx) = unbounded_channel();
        let (frame_list_tx, frame_list_rx) = unbounded_channel();
        write_log(&log, "boot: creating providers");
        let cancel = Arc::new(AtomicBool::new(false));
        let meteogate = MeteoGateProvider::new(
            client.clone(),
            dirs.clone(),
            config.meteogate.clone(),
            cancel.clone(),
        );
        let meteoalarm = MeteoAlarmProvider::new(
            client.clone(),
            dirs.clone(),
            config.meteoalarm.clone(),
            cancel.clone(),
        );
        let eumetnet = EumetnetProvider::new(client.clone(), dirs.clone(), config.eumetnet.clone());
        let maps = NaturalEarthProvider::new(client, dirs.clone(), cancel.clone());
        let mut app = Self {
            viewport,
            layers: LayerRegistry::new(),
            borders: None,
            timestamps: Vec::new(),
            radar_frame: None,
            border_mask_cache: None,
            fallback_mask_cache: None,
            border_layers: HashMap::new(),
            border_layers_version: 0,
            border_tiles_built: 0,
            border_total_resolutions: 4,
            border_built_set: HashSet::new(),
            braille_frame: BrailleFrame::default(),
            frame_count: 0,
            frame_index: 0,
            show_help: false,
            layer_panel_focused: false,
            location_label,
            location_marker,
            warning_layer: None,
            obs_cache: None,
            obs_incoming: Vec::new(),
            obs_incoming_id: 0,
            obs_partial: Vec::new(),
            map_width: 100,
            map_height: 50,
            pending_warning_refresh: false,
            obs_last_attempt: None,
            warn_last_attempt: None,
            radar_requested_ts: None,
            radar_frame_ts: None,
            dirs,
            config,
            maps,
            meteogate,
            meteoalarm,
            eumetnet,
            refresh_tx,
            refresh_rx,
            refresh_task: None,
            refresh_id: 0,
            border_tx,
            border_rx,
            border_task: None,
            border_refresh_id: 0,
            border_fetch_resolution: None,
            border_spawn_cancel: None,
            obs_tx,
            obs_rx,
            obs_task: None,
            obs_refresh_id: 0,
            warn_tx,
            warn_rx,
            warn_task: None,
            warn_refresh_id: 0,
            task_tx,
            task_rx,
            active_tasks: Vec::new(),
            cancel,
            preload_tasks: Vec::new(),
            frame_list_tx,
            frame_list_rx,
        };

        write_log(&log, "boot: loading saved state");
        app.load_state();

        // Launch border loading in background — never block boot on it.
        write_log(
            &log,
            format!("boot: spawning border load for zoom {}", app.viewport.zoom),
        );
        app.request_border_refresh();

        // Launch radar frame list fetch in background.
        write_log(&log, "boot: spawning background frame list fetch");
        {
            let meteogate = app.meteogate.clone();
            let tx = app.frame_list_tx.clone();
            let task_tx = app.task_tx.clone();
            let task_id = next_task_id();
            let _ = task_tx.send(TaskMsg::Start {
                id: task_id,
                label: "frame list".into(),
                kind: TaskKind::FrameList,
            });
            let ll = log.clone();
            tokio::spawn(async move {
                match meteogate.frame_list().await {
                    Ok(ts) => {
                        write_log(&ll, format!("boot: got {} timestamps", ts.len()));
                        let _ = tx.send(ts);
                        let _ = task_tx.send(TaskMsg::Complete { id: task_id });
                    }
                    Err(e) => {
                        write_log(&ll, format!("boot: frame_list failed: {e}"));
                        let _ = task_tx.send(TaskMsg::Error {
                            id: task_id,
                            error: e.to_string(),
                        });
                    }
                }
            });
        }

        // Pre‑load all other border resolutions in the background too.
        app.preload_border_resolutions();

        write_log(&log, "=== front boot complete ===");
        Ok(app)
    }

    pub async fn refresh_meteogate(&mut self, width: u16, height: u16) {
        let log = self.dirs.log_path.clone();
        if self.timestamps.is_empty() {
            write_log(&log, "meteogate: fetching frame list");
            match self.meteogate.frame_list().await {
                Ok(ts) => {
                    write_log(&log, format!("meteogate: got {} timestamps", ts.len()));
                    self.frame_index = 0;
                    self.timestamps = ts;
                }
                Err(error) => {
                    write_log(&log, format!("meteogate: frame_list failed: {error}"));
                    self.layers
                        .set_status(LayerId::Radar, LayerStatus::Error(error.to_string()));
                    return;
                }
            }
        }

        if !self.layers.enabled(LayerId::Radar) || self.timestamps.is_empty() {
            write_log(&log, "meteogate: radar disabled or no timestamps, skipping");
            return;
        }

        let ts = self.timestamps[self.frame_index];
        let bounds = self.viewport.bounds(width, height);
        let zoom = self.viewport.zoom;
        self.layers.set_status(LayerId::Radar, LayerStatus::Loading);

        write_log(
            &log,
            format!("meteogate: loading frame ts={ts} zoom={zoom:.1}"),
        );

        let frame_result = self.meteogate.frame(ts, bounds, zoom).await;

        match &frame_result {
            Ok(frame) => {
                write_log(
                    &log,
                    format!(
                        "meteogate: frame ready ({} tiles, {} missing)",
                        frame.tiles.len(),
                        frame.missing_tiles,
                    ),
                );
            }
            Err(error) => {
                write_log(&log, format!("meteogate: frame failed: {error}"));
            }
        }
        match frame_result {
            Ok(frame) => {
                self.merge_radar_frame(frame);
                self.layers.set_status(LayerId::Radar, LayerStatus::Ready);
            }
            Err(error) => {
                self.layers
                    .set_status(LayerId::Radar, LayerStatus::Error(error.to_string()));
            }
        }
    }

    pub fn request_meteogate_refresh(&mut self, width: u16, height: u16) {
        if !self.layers.enabled(LayerId::Radar) {
            return;
        }
        let Some(ts) = self.timestamps.get(self.frame_index).copied() else {
            return;
        };
        let bounds = self.viewport.bounds(width, height);
        let tile_zoom = self.viewport.zoom.round().clamp(1.0, 7.0) as u8;
        // Skip only when the current frame already covers the viewport
        // AND is for the same timestamp — coverage alone can't tell two
        // frame times apart, which used to break `[`/`]` stepping.
        if self.radar_requested_ts == Some(ts)
            && self
                .radar_frame
                .as_ref()
                .is_some_and(|frame| frame.covers_bounds(bounds, tile_zoom))
        {
            return;
        }
        // Don't restart if a task for this timestamp is already
        // in-flight — the existing fetch will produce the same tiles.
        if self.refresh_task.is_some() && self.radar_requested_ts == Some(ts) {
            return;
        }
        self.radar_requested_ts = Some(ts);
        if let Some(task) = self.refresh_task.take() {
            task.abort();
        }
        self.refresh_id = self.refresh_id.wrapping_add(1);
        let id = self.refresh_id;
        let provider = self.meteogate.clone();
        let tx = self.refresh_tx.clone();
        let zoom = self.viewport.zoom;
        // Prefetch tiles for a slightly larger area so small pans hit
        // the cache instead of triggering a network round-trip.
        let fetch_bounds = bounds.expanded(0.5);
        self.layers.set_status(LayerId::Radar, LayerStatus::Loading);

        let task_id = next_task_id();
        let task_tx = self.task_tx.clone();
        let _ = task_tx.send(TaskMsg::Start {
            id: task_id,
            label: format!("frame {ts}"),
            kind: TaskKind::RadarFrame,
        });

        // Create a tile-level channel for streaming tiles.  The
        // provider will send each tile individually as it completes,
        // in centre-first clockwise spiral order.
        let (tile_tx, mut tile_rx) = tokio::sync::mpsc::unbounded_channel();

        self.refresh_task = Some(tokio::spawn(async move {
            // Forward each tile through the outer refresh channel
            // as it arrives, so the UI can render progressively.
            let tx2 = tx.clone();
            let task_tx2 = task_tx.clone();
            let forwarder = tokio::spawn(async move {
                let mut tile_count = 0u32;
                while let Some(tile_result) = tile_rx.recv().await {
                    let payload = match tile_result {
                        Ok(tile) => {
                            tile_count += 1;
                            // Asymptotic fraction: each tile moves the bar
                            // closer to 95 %.  Assume ~30 tiles is typical.
                            let fraction = (1.0 - 0.5f64.powi(tile_count as i32 / 3)).min(0.95);
                            let _ = task_tx2.send(TaskMsg::Progress {
                                id: task_id,
                                action: format!("{} tiles", tile_count),
                                fraction,
                            });
                            RadarRefreshPayload::Tile(tile)
                        }
                        Err(_) => continue, // skip errored tiles
                    };
                    let _ = tx2.send(RadarRefreshResult {
                        id,
                        result: payload,
                    });
                }
            });

            // Launch the streaming frame load.
            let frame_result = provider
                .frame_streamed(ts, fetch_bounds, zoom, tile_tx)
                .await;

            // Wait for the forwarder to drain any in-flight tiles.
            let _ = forwarder.await;

            let result = match frame_result {
                Ok(frame) => {
                    let _ = task_tx.send(TaskMsg::Complete { id: task_id });
                    RadarRefreshPayload::Ready(frame)
                }
                Err(error) => {
                    let _ = task_tx.send(TaskMsg::Error {
                        id: task_id,
                        error: error.to_string(),
                    });
                    RadarRefreshPayload::Error(error.to_string())
                }
            };
            let _ = tx.send(RadarRefreshResult { id, result });
        }));
    }

    pub fn request_border_refresh(&mut self) {
        self.request_border_refresh_with_cache(false);
    }

    pub fn request_border_refetch(&mut self) {
        self.request_border_refresh_with_cache(true);
    }

    /// Load a specific border resolution into the cache, regardless of
    /// the current zoom level.  Used when a layer (e.g. MajorRoads) is
    /// toggled ON so the data is ready when the user next views an area
    /// at that resolution.
    pub fn request_border_layer(&mut self, resolution: BorderResolution) {
        if self.border_layers.contains_key(&resolution) {
            return;
        }
        // Don't restart if we're already fetching this resolution —
        // the existing task will cache it for the layer too.
        if self.border_fetch_resolution == Some(resolution) {
            return;
        }
        // Signal the old task's spawn_blocking to exit.
        if let Some(cancel) = self.border_spawn_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        let _ = self.border_task.take();
        let spawn_cancel = Arc::new(AtomicBool::new(false));
        self.border_spawn_cancel = Some(spawn_cancel.clone());
        self.border_fetch_resolution = Some(resolution);
        self.border_refresh_id = self.border_refresh_id.wrapping_add(1);
        let id = self.border_refresh_id;
        let res_label = resolution.label().to_string();
        let maps = self.maps.clone();
        let tx = self.border_tx.clone();
        let log_path = self.dirs.log_path.clone();
        let task_tx = self.task_tx.clone();
        let task_id = next_task_id();
        let _ = task_tx.send(TaskMsg::Start {
            id: task_id,
            label: format!("border {res_label}"),
            kind: TaskKind::BorderDownload,
        });
        // Load tiles for the current viewport bounds so region/road data
        // is available when the user is at that resolution.
        let bounds = self.viewport.bounds(self.map_width, self.map_height);
        self.layers
            .set_status(LayerId::MapBorders, LayerStatus::Loading);
        self.border_task = Some(tokio::spawn(async move {
            let result = match maps
                .borders_for_resolution(resolution, bounds, spawn_cancel)
                .await
            {
                Ok(layer) => {
                    let _ = task_tx.send(TaskMsg::Complete { id: task_id });
                    let tile_task_id = next_task_id();
                    let _ = task_tx.send(TaskMsg::Start {
                        id: tile_task_id,
                        label: format!("tiles {res_label}"),
                        kind: TaskKind::BorderTileGen,
                    });
                    spawn_tile_gen(
                        maps.clone(),
                        layer.resolution,
                        layer.lines.clone(),
                        log_path,
                        id,
                        tx.clone(),
                        task_tx.clone(),
                        tile_task_id,
                    );
                    BorderRefreshPayload::Ready(layer)
                }
                Err(error) => {
                    let _ = task_tx.send(TaskMsg::Error {
                        id: task_id,
                        error: error.to_string(),
                    });
                    BorderRefreshPayload::Error(error.to_string())
                }
            };
            let _ = tx.send(BorderRefreshResult { id, result });
        }));
    }

    fn request_border_refresh_with_cache(&mut self, clear_cache: bool) {
        let desired = BorderResolution::for_zoom(self.viewport.zoom);
        // Already fetching exactly the resolution we need — let the
        // existing task finish instead of restarting it.
        if !clear_cache && self.border_fetch_resolution == Some(desired) {
            return;
        }
        if clear_cache {
            self.border_layers.clear();
            self.border_layers_version = self.border_layers_version.wrapping_add(1);
            self.border_mask_cache = None;
        } else if let Some(layer) = self.border_layers.get(&desired).cloned() {
            // Layer already cached — use it immediately and let the
            // in‑flight task (if any) finish on its own.  Its result
            // will carry a stale border_refresh_id and be discarded
            // by drain_refresh_results, but the spawn_blocking work
            // it does (tile loading / generation) populates the on‑
            // disk cache for future launches.
            self.borders = Some(layer);
            self.border_mask_cache = None;
            self.layers
                .set_status(LayerId::MapBorders, LayerStatus::Ready);
            return;
        }
        if self
            .borders
            .as_ref()
            .is_some_and(|borders| borders.resolution == desired)
            && !clear_cache
        {
            return;
        }
        // Signal the old task's spawn_blocking to exit (tile loading /
        // GeoJSON parsing checks this flag).  We do NOT abort the task
        // itself — dropping the JoinHandle detaches it, and the work
        // continues to populate the on‑disk cache.  The result carries
        // a stale border_refresh_id and gets discarded.
        if let Some(cancel) = self.border_spawn_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        let _ = self.border_task.take();
        let spawn_cancel = Arc::new(AtomicBool::new(false));
        self.border_spawn_cancel = Some(spawn_cancel.clone());
        self.border_fetch_resolution = Some(desired);
        self.border_refresh_id = self.border_refresh_id.wrapping_add(1);
        let id = self.border_refresh_id;
        let task_tx = self.task_tx.clone();
        let maps = self.maps.clone();
        let tx = self.border_tx.clone();
        let dirs = clear_cache.then(|| self.dirs.clone());
        let log_path = self.dirs.log_path.clone();
        let bounds = self.viewport.bounds(self.map_width, self.map_height);
        let task_id = next_task_id();
        let res_label = desired.label().to_string();
        let _ = task_tx.send(TaskMsg::Start {
            id: task_id,
            label: format!("{res_label} borders"),
            kind: TaskKind::BorderDownload,
        });
        self.layers
            .set_status(LayerId::MapBorders, LayerStatus::Loading);
        self.border_task = Some(tokio::spawn(async move {
            if let Some(d) = dirs {
                if let Err(e) = d.clear_map_cache() {
                    let _ = tx.send(BorderRefreshResult {
                        id,
                        result: BorderRefreshPayload::Error(e.to_string()),
                    });
                    let _ = task_tx.send(TaskMsg::Error {
                        id: task_id,
                        error: e.to_string(),
                    });
                    return;
                }
            }
            let _ = task_tx.send(TaskMsg::Progress {
                id: task_id,
                action: format!("downloading {res_label} geo…"),
                fraction: 0.3,
            });
            let result = match maps
                .borders_for_resolution(desired, bounds, spawn_cancel)
                .await
            {
                Ok(layer) => {
                    let _ = task_tx.send(TaskMsg::Progress {
                        id: task_id,
                        action: "building grid".into(),
                        fraction: 0.8,
                    });
                    let tile_task_id = next_task_id();
                    let _ = task_tx.send(TaskMsg::Complete { id: task_id });
                    let _ = task_tx.send(TaskMsg::Start {
                        id: tile_task_id,
                        label: format!("{} tiles", layer.resolution.label()),
                        kind: TaskKind::BorderTileGen,
                    });
                    spawn_tile_gen(
                        maps.clone(),
                        layer.resolution,
                        layer.lines.clone(),
                        log_path,
                        id,
                        tx.clone(),
                        task_tx.clone(),
                        tile_task_id,
                    );
                    BorderRefreshPayload::Ready(layer)
                }
                Err(ref error) => {
                    let _ = task_tx.send(TaskMsg::Error {
                        id: task_id,
                        error: error.to_string(),
                    });
                    BorderRefreshPayload::Error(error.to_string())
                }
            };
            let _ = tx.send(BorderRefreshResult { id, result });
        }));
    }

    /// Insert a border layer into the cache, evicting the resolution
    /// furthest from the current zoom if more than 2 are stored.  This
    /// keeps RAM bounded while preserving the current level plus one
    /// adjacent level for smooth zoom transitions.
    fn insert_border_layer(&mut self, layer: BorderLayer) {
        let res = layer.resolution;
        if self.border_layers.contains_key(&res) {
            return;
        }
        self.border_layers.insert(res, layer);
        self.border_layers_version = self.border_layers_version.wrapping_add(1);

        if self.border_layers.len() <= 2 {
            return;
        }
        let current = BorderResolution::for_zoom(self.viewport.zoom);
        if let Some(&victim) = self
            .border_layers
            .keys()
            .filter(|&&r| r != current)
            .max_by_key(|&&r| resolution_distance(r, current))
        {
            self.border_layers.remove(&victim);
            write_log(
                &self.dirs.log_path,
                format!(
                    "evict: {} borders from cache ({} resolutions stored)",
                    victim.label(),
                    self.border_layers.len()
                ),
            );
        }
    }

    /// Recompute the expected radar frame list (purely local) and
    /// adopt it if a newer slot has opened.  Returns `true` when the
    /// list changed, i.e. a refresh of the displayed frame may be due.
    /// Keeps the user anchored: viewing the live frame follows the
    /// newest slot; viewing an older frame stays on that timestamp.
    pub fn poll_radar_timestamps(&mut self) -> bool {
        let fresh = crate::providers::meteogate::compute_frame_list();
        if fresh.first() == self.timestamps.first() {
            return false;
        }
        let current_ts = self.timestamps.get(self.frame_index).copied();
        let was_latest = self.frame_index == 0;
        self.timestamps = fresh;
        self.frame_index = if was_latest {
            0
        } else {
            current_ts
                .and_then(|ts| self.timestamps.iter().position(|&t| t == ts))
                .unwrap_or(0)
        };
        true
    }

    /// Spawn a background MeteoAlarm warning fetch (disk-cached for
    /// 5 minutes inside the provider, so calling this often is cheap).
    pub fn request_warning_refresh(&mut self) {
        if !self.layers.enabled(LayerId::MeteoAlarm) {
            return;
        }
        if let Some(task) = self.warn_task.take() {
            task.abort();
        }
        self.warn_refresh_id = self.warn_refresh_id.wrapping_add(1);
        let id = self.warn_refresh_id;
        self.warn_last_attempt = Some(Instant::now());
        self.layers
            .set_status(LayerId::MeteoAlarm, LayerStatus::Loading);
        let provider = self.meteoalarm.clone();
        let tx = self.warn_tx.clone();
        let log = self.dirs.log_path.clone();
        let task_id = next_task_id();
        let task_tx = self.task_tx.clone();
        let _ = task_tx.send(TaskMsg::Start {
            id: task_id,
            label: "warnings".into(),
            kind: TaskKind::Warning,
        });
        self.warn_task = Some(tokio::spawn(async move {
            let result = match provider.warnings().await {
                Ok(layer) => {
                    write_log(
                        &log,
                        format!("meteoalarm: got {} warning features", layer.features.len()),
                    );
                    let _ = task_tx.send(TaskMsg::Complete { id: task_id });
                    WarnRefreshPayload::Ready(layer)
                }
                Err(error) => {
                    write_log(&log, format!("meteoalarm: {error}"));
                    let _ = task_tx.send(TaskMsg::Error {
                        id: task_id,
                        error: error.to_string(),
                    });
                    WarnRefreshPayload::Error(error.to_string())
                }
            };
            let _ = tx.send(WarnRefreshResult { id, result });
        }));
    }

    pub fn drain_warning_results(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.warn_rx.try_recv() {
            if result.id != self.warn_refresh_id {
                continue;
            }
            self.warn_task = None;
            changed = true;
            match result.result {
                WarnRefreshPayload::Ready(layer) => {
                    self.warning_layer = Some(layer);
                    self.layers
                        .set_status(LayerId::MeteoAlarm, LayerStatus::Ready);
                }
                WarnRefreshPayload::Error(error) => {
                    self.layers
                        .set_status(LayerId::MeteoAlarm, LayerStatus::Error(error));
                }
            }
        }
        changed
    }

    pub fn request_obs_refresh(&mut self) {
        if !self.any_obs_enabled() {
            return;
        }
        self.obs_last_attempt = Some(Instant::now());
        if let Some(task) = self.obs_task.take() {
            task.abort();
        }
        // Keep the existing cache visible until the new full set is ready
        // (committed atomically in drain_obs_results on the Ready message), so
        // a refresh never blanks the map mid-fetch.
        self.obs_refresh_id = self.obs_refresh_id.wrapping_add(1);
        let id = self.obs_refresh_id;
        let provider = self.eumetnet.clone();
        let tx = self.obs_tx.clone();
        let log = self.dirs.log_path.clone();
        let bounds = self.viewport.bounds(self.map_width, self.map_height);
        let zoom = self.viewport.zoom;

        self.set_obs_status(LayerStatus::Loading);

        let task_id = next_task_id();
        let task_tx = self.task_tx.clone();
        let _ = task_tx.send(TaskMsg::Start {
            id: task_id,
            label: "observations".into(),
            kind: TaskKind::Observation,
        });
        self.obs_task = Some(tokio::spawn(async move {
            let (ptx, mut prx) = unbounded_channel::<ObservationPoint>();
            // flush_tx: provider sends () to trigger a PartialCommit in the UI.
            let (flush_tx, mut flush_rx) = unbounded_channel::<()>();
            let tx_fwd = tx.clone();
            let task_tx_fwd = task_tx.clone();
            let (fetch_result, _) = tokio::join!(
                provider.observations(zoom, Some(bounds), ptx, flush_tx),
                async move {
                    let mut n = 0u32;
                    loop {
                        tokio::select! {
                            Some(pt) = prx.recv() => {
                                n += 1;
                                if n.is_multiple_of(5) {
                                    let frac = (1.0 - 0.5f64.powi(n as i32 / 10)).min(0.90);
                                    let _ = task_tx_fwd.send(TaskMsg::Progress {
                                        id: task_id,
                                        action: format!("{n} stations"),
                                        fraction: frac,
                                    });
                                }
                                let _ = tx_fwd.send(ObsRefreshResult {
                                    id,
                                    result: ObsRefreshPayload::Point(pt),
                                });
                            }
                            Some(()) = flush_rx.recv() => {
                                let _ = tx_fwd.send(ObsRefreshResult {
                                    id,
                                    result: ObsRefreshPayload::PartialCommit,
                                });
                            }
                            else => break,
                        }
                    }
                }
            );
            match fetch_result {
                Ok(()) => {
                    let _ = tx.send(ObsRefreshResult {
                        id,
                        result: ObsRefreshPayload::Ready,
                    });
                }
                Err(error) => {
                    write_log(&log, format!("obs: {error}"));
                    let _ = tx.send(ObsRefreshResult {
                        id,
                        result: ObsRefreshPayload::Error(error.to_string()),
                    });
                }
            }
            let _ = task_tx.send(TaskMsg::Complete { id: task_id });
            let _ = tx.send(ObsRefreshResult {
                id,
                result: ObsRefreshPayload::Done,
            });
        }));
    }

    /// Bit flag OR'd into the id of pre‑load border results so the
    /// drain handler can distinguish them from active‑task results.
    const PRELOAD_BIT: u64 = 1 << 63;

    /// Spawn background tasks to pre‑load border resolutions adjacent
    /// to the current zoom level (current ±1).  Loading all 4
    /// resolutions at once wastes memory; with the dedup cache, a cache
    /// hit is a single file read, so preloading just the neighbours
    /// keeps first‑pixel latency low without hoarding gigs of lines.
    fn preload_border_resolutions(&mut self) {
        let active_res = BorderResolution::for_zoom(self.viewport.zoom);
        // Adjacent resolutions: current ±1 step.
        let adjacents: Vec<BorderResolution> = [
            BorderResolution::Low110m,
            BorderResolution::Medium50m,
            BorderResolution::High10m,
            BorderResolution::Regional10m,
        ]
        .into_iter()
        .filter(|&r| {
            let d = resolution_distance(r, active_res);
            d == 15 || d == 0 // 0 = active, 15 = adjacent step
        })
        .collect();

        let bounds = self.viewport.bounds(self.map_width, self.map_height);
        let log_path = self.dirs.log_path.clone();
        let task_tx = self.task_tx.clone();
        for &res in &adjacents {
            if self.border_layers.contains_key(&res) {
                continue;
            }
            // The active refresh task (request_border_refresh) is
            // already fetching this resolution — don't duplicate it.
            if res == active_res && self.border_task.is_some() {
                continue;
            }
            // Preloads are silent background work — they don't send
            // TaskMsg so they never pollute the progress overlay.
            let maps = self.maps.clone();
            let tx = self.border_tx.clone();
            let tile_log = log_path.clone();
            let task_tx2 = task_tx.clone();
            let preload_cancel = maps.cancel.clone();
            self.preload_tasks.push(tokio::spawn(async move {
                let result = match maps
                    .borders_for_resolution(res, bounds, preload_cancel)
                    .await
                {
                    Ok(layer) => {
                        let tile_task_id = next_task_id();
                        spawn_tile_gen(
                            maps.clone(),
                            layer.resolution,
                            layer.lines.clone(),
                            tile_log.clone(),
                            Self::PRELOAD_BIT | layer.resolution as u64,
                            tx.clone(),
                            task_tx2,
                            tile_task_id,
                        );
                        BorderRefreshPayload::Ready(layer)
                    }
                    Err(e) => BorderRefreshPayload::Error(e.to_string()),
                };
                let _ = tx.send(BorderRefreshResult {
                    id: Self::PRELOAD_BIT | res as u64,
                    result,
                });
            }));
        }
    }

    pub fn drain_refresh_results(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.refresh_rx.try_recv() {
            if result.id != self.refresh_id {
                continue;
            }
            changed = true;
            match result.result {
                RadarRefreshPayload::Tile(tile) => {
                    // Tiles for a different frame time must not merge
                    // with the old frame — coords collide and the stale
                    // tile would win.  Evict on first new-time tile.
                    if self.radar_frame_ts != self.radar_requested_ts {
                        self.radar_frame = None;
                        self.radar_frame_ts = self.radar_requested_ts;
                    }
                    // Merge individual tile into the current frame
                    // so the UI renders progressively.
                    if let Some(frame) = &mut self.radar_frame {
                        // If zoom changed, evict stale tiles from the
                        // previous zoom so we don't render multiple
                        // detail levels at the same time.
                        if tile.coord.z != frame.target_zoom {
                            frame.tiles.retain(|t| t.coord.z == tile.coord.z);
                            frame.target_zoom = tile.coord.z;
                        }
                        if !frame.tiles.iter().any(|t| t.coord == tile.coord) {
                            frame.tiles.push(tile);
                        }
                    } else {
                        let z = tile.coord.z;
                        self.radar_frame = Some(RadarFrame {
                            time: self.radar_requested_ts.unwrap_or(0),
                            path: String::new(),
                            tiles: vec![tile],
                            missing_tiles: 0,
                            target_zoom: z,
                        });
                    }
                }
                RadarRefreshPayload::Ready(frame) => {
                    self.radar_frame_ts = self.radar_requested_ts;
                    if !frame.tiles.is_empty() {
                        // Non-streaming path (initial load): merge normally.
                        self.merge_radar_frame(frame);
                    } else {
                        // Streaming path: just update metadata.
                        if let Some(existing) = &mut self.radar_frame {
                            existing.time = frame.time;
                            existing.path = frame.path;
                            existing.target_zoom = frame.target_zoom;
                            existing.missing_tiles = frame.missing_tiles;
                        } else {
                            self.radar_frame = Some(frame);
                        }
                    }
                    self.layers.set_status(LayerId::Radar, LayerStatus::Ready);
                    self.refresh_task = None;
                }
                RadarRefreshPayload::Error(error) => {
                    self.layers
                        .set_status(LayerId::Radar, LayerStatus::Error(error));
                    self.refresh_task = None;
                }
            }
        }
        while let Ok(result) = self.border_rx.try_recv() {
            // `TilesBuilt` is unconditional — it only bumps the
            // progress counter regardless of staleness, but we dedup
            // via `border_built_set` so races never inflate the count.
            if let BorderRefreshPayload::TilesBuilt(res) = result.result {
                if self.border_built_set.insert(res) {
                    write_log(
                        &self.dirs.log_path,
                        format!("tile_gen: {} tiles built", res.label()),
                    );
                    self.border_tiles_built = self.border_built_set.len() as u32;
                }
                continue;
            }

            // Pre‑load results (bit 63 set) always insert into
            // border_layers but never touch the active border_task
            // or self.borders — they arrive asynchronously and may be
            // for a different zoom tier than the current viewport.
            if result.id & Self::PRELOAD_BIT != 0 {
                if let BorderRefreshPayload::Ready(layer) = result.result {
                    if !self.border_layers.contains_key(&layer.resolution) {
                        write_log(
                            &self.dirs.log_path,
                            format!(
                                "preload: {} borders ({} lines)",
                                layer.resolution.label(),
                                layer.lines.len()
                            ),
                        );
                        self.insert_border_layer(layer.clone());
                    }
                }
                continue;
            }

            if result.id != self.border_refresh_id {
                continue;
            }
            self.border_task = None;
            self.border_fetch_resolution = None;
            changed = true;
            match result.result {
                // TilesBuilt is caught unconditionally above, but the
                // compiler needs this arm for exhaustiveness.
                BorderRefreshPayload::TilesBuilt(_) => {}
                BorderRefreshPayload::Ready(layer) => {
                    write_log(
                        &self.dirs.log_path,
                        format!(
                            "drain: got {} borders ({} lines)",
                            layer.resolution.label(),
                            layer.lines.len()
                        ),
                    );
                    // Always cache the new layer so future requests
                    // for that resolution can be served from memory.
                    self.insert_border_layer(layer.clone());
                    self.border_mask_cache = None;
                    // Keep `borders` pointing at the layer that best
                    // matches the current zoom.  The renderer no
                    // longer relies on this pointer alone (it iterates
                    // `border_layers` for best-effort delivery), but
                    // having a sensible value here avoids confusion in
                    // logs and tests.
                    let desired = BorderResolution::for_zoom(self.viewport.zoom);
                    if layer.resolution == desired || self.borders.is_none() {
                        self.borders = Some(layer);
                        self.layers
                            .set_status(LayerId::MapBorders, LayerStatus::Ready);
                    } else {
                        write_log(
                            &self.dirs.log_path,
                            format!(
                                "drain: {} arrived after zoom change to {}, keeping active layer",
                                layer.resolution.label(),
                                desired.label()
                            ),
                        );
                    }
                }
                BorderRefreshPayload::Error(error) => {
                    write_log(&self.dirs.log_path, format!("drain: border error: {error}"));
                    self.layers
                        .set_status(LayerId::MapBorders, LayerStatus::Error(error));
                }
            }
        }
        changed
    }

    /// Check for an async‑fetched frame list and adopt it into
    /// `timestamps`.  Returns `true` if the list changed (caller should
    /// re‑render).  If we got an initial list and radar is enabled it
    /// also fires the first refresh.
    pub fn drain_frame_list(&mut self) -> bool {
        let mut changed = false;
        while let Ok(ts) = self.frame_list_rx.try_recv() {
            if ts.is_empty() || self.timestamps == ts {
                continue;
            }
            let n = ts.len();
            changed = true;
            self.timestamps = ts;
            self.frame_index = 0;
            write_log(
                &self.dirs.log_path,
                format!("frame_list: got {n} timestamps"),
            );
            if self.layers.enabled(LayerId::Radar) {
                write_log(
                    &self.dirs.log_path,
                    "frame_list: spawning first radar refresh",
                );
                self.request_meteogate_refresh(self.map_width, self.map_height);
            }
        }
        changed
    }

    /// Drain pending task-queue messages.
    ///
    /// One row per `TaskKind` is kept in `active_tasks`.  A new Start for a
    /// kind that already has an entry updates it in place so the overlay
    /// never flickers or doubles up.  Completed/errored entries are pruned
    /// after 3 s; running entries persist until they finish.
    pub fn drain_task_messages(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.task_rx.try_recv() {
            changed = true;
            match msg {
                TaskMsg::Start { id, label, kind } => {
                    write_log(
                        &self.dirs.log_path,
                        format!("task: start {kind:?} - {label}"),
                    );
                    // Upsert: reuse the existing row for this kind so the
                    // overlay never shows two rows for the same download type.
                    let now = Instant::now();
                    if let Some(task) = self.active_tasks.iter_mut().find(|t| t.kind == kind) {
                        task.id = id;
                        task.label = label;
                        task.action = String::new();
                        task.fraction = 0.0;
                        task.anim_from = task.display_fraction;
                        task.anim_t = 0.0;
                        task.state = TaskState::Running;
                        task.completed_at = None;
                        task.last_anim = now;
                    } else {
                        self.active_tasks.push(ActiveTask {
                            id,
                            label,
                            action: String::new(),
                            fraction: 0.0,
                            display_fraction: 0.0,
                            anim_from: 0.0,
                            anim_t: 0.0,
                            kind,
                            state: TaskState::Running,
                            completed_at: None,
                            last_anim: now,
                        });
                    }
                }
                TaskMsg::Progress {
                    id,
                    action,
                    fraction,
                } => {
                    if let Some(task) = self.active_tasks.iter_mut().find(|t| t.id == id) {
                        task.action = action;
                        if (fraction - task.fraction).abs() > 0.001 {
                            task.anim_from = task.display_fraction;
                            task.anim_t = 0.0;
                        }
                        task.fraction = fraction;
                    }
                }
                TaskMsg::Complete { id } => {
                    if let Some(task) = self.active_tasks.iter_mut().find(|t| t.id == id) {
                        task.anim_from = task.display_fraction;
                        task.anim_t = 0.0;
                        task.fraction = 1.0;
                        task.state = TaskState::Completed;
                        task.completed_at = Some(Instant::now());
                    }
                }
                TaskMsg::Error { id, error } => {
                    write_log(&self.dirs.log_path, format!("task: error {id} - {error}"));
                    if let Some(task) = self.active_tasks.iter_mut().find(|t| t.id == id) {
                        task.action = error;
                        task.state = TaskState::Error;
                        task.completed_at = Some(Instant::now());
                    }
                }
            }
        }
        // Animate display_fraction with ease-in / ease-out using smoothstep.
        // Each time `fraction` changes, `anim_t` resets to 0 and advances to
        // 1 over 0.25 s.  smoothstep(t) = t²(3−2t) gives zero velocity at
        // both endpoints, producing natural ease-in and ease-out.
        let now = Instant::now();
        for task in &mut self.active_tasks {
            let dt = now.duration_since(task.last_anim).as_secs_f64();
            task.last_anim = now;
            if task.anim_t < 1.0 {
                task.anim_t = (task.anim_t + dt * 4.0).min(1.0); // 0.25 s max
                let t = task.anim_t;
                let eased = t * t * (3.0 - 2.0 * t); // smoothstep
                task.display_fraction = task.anim_from + (task.fraction - task.anim_from) * eased;
                changed = true;
            }
        }

        // Prune only finished entries that have completed their fill animation
        // AND been visible for at least 1 s so the user can read the result.
        let before = self.active_tasks.len();
        self.active_tasks.retain(|t| {
            t.state == TaskState::Running
                || t.display_fraction < 0.999 // animation still in progress
                || t.completed_at.is_none_or(|c| now.duration_since(c).as_secs_f64() < 1.0)
        });
        changed |= self.active_tasks.len() < before;
        changed
    }

    /// Signal all background tasks to stop and abort tracked ones.
    /// Called on quit so `spawn_blocking` threads exit promptly
    /// instead of keeping the process alive for minutes.
    pub fn shutdown(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(cancel) = self.border_spawn_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        for task in self.preload_tasks.drain(..) {
            task.abort();
        }
        if let Some(task) = self.refresh_task.take() {
            task.abort();
        }
        if let Some(task) = self.border_task.take() {
            task.abort();
        }
        if let Some(task) = self.obs_task.take() {
            task.abort();
        }
        if let Some(task) = self.warn_task.take() {
            task.abort();
        }
    }

    pub fn drain_obs_results(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.obs_rx.try_recv() {
            if result.id != self.obs_refresh_id {
                continue;
            }
            match result.result {
                ObsRefreshPayload::Point(point) => {
                    if result.id != self.obs_incoming_id {
                        self.obs_incoming.clear();
                        self.obs_incoming_id = result.id;
                        self.obs_partial.clear(); // reset partial accumulator for new refresh
                    }
                    self.obs_incoming.push(point);
                }
                ObsRefreshPayload::PartialCommit => {
                    // Commit what we have so far into obs_cache so the UI shows
                    // progressive results (capitals → cities → full viewport).
                    // Accumulates across phases: obs_partial grows with each commit.
                    if self.obs_incoming_id == result.id && !self.obs_incoming.is_empty() {
                        self.obs_partial
                            .extend(std::mem::take(&mut self.obs_incoming));
                        self.obs_cache = Some(ObservationLayer {
                            points: self.obs_partial.clone(),
                            updated_at: Some(Utc::now().timestamp()),
                        });
                        changed = true;
                    }
                }
                ObsRefreshPayload::Ready => {
                    // Final commit: fold any remaining incoming into the partial
                    // accumulator and replace obs_cache with the complete set.
                    if self.obs_incoming_id == result.id {
                        self.obs_partial
                            .extend(std::mem::take(&mut self.obs_incoming));
                        if !self.obs_partial.is_empty() {
                            self.obs_cache = Some(ObservationLayer {
                                points: std::mem::take(&mut self.obs_partial),
                                updated_at: Some(Utc::now().timestamp()),
                            });
                            changed = true;
                        }
                    }
                    self.set_obs_status(LayerStatus::Ready);
                }
                ObsRefreshPayload::Error(error) => {
                    self.set_obs_status(LayerStatus::Error(error));
                }
                ObsRefreshPayload::Done => {
                    self.obs_task = None;
                    // If nothing was cached (empty result / failure), clear
                    // obs_last_attempt so staleness retries on the next tick
                    // instead of waiting the full refresh interval.
                    if self.obs_cache.is_none()
                        || self.obs_cache.as_ref().is_some_and(|c| c.points.is_empty())
                    {
                        self.obs_last_attempt = None;
                    }
                }
            }
        }
        changed
    }

    pub fn next_frame(&mut self) {
        if !self.timestamps.is_empty() {
            self.frame_index = (self.frame_index + 1).min(self.timestamps.len() - 1);
        }
    }

    pub fn previous_frame(&mut self) {
        self.frame_index = self.frame_index.saturating_sub(1);
    }

    fn merge_radar_frame(&mut self, frame: RadarFrame) {
        if let Some(existing) = &mut self.radar_frame {
            existing.merge_tiles(frame);
        } else {
            self.radar_frame = Some(frame);
        }
    }

    pub fn frame_label(&self) -> String {
        let Some(frame) = self.radar_frame.as_ref() else {
            return "no frame".to_string();
        };
        match DateTime::from_timestamp(frame.time, 0) {
            Some(dt) => dt.with_timezone(&Local).format("%H:%M").to_string(),
            None => frame.time.to_string(),
        }
    }

    pub fn save_state(&self) {
        let center = world_to_lat_lon(self.viewport.center);
        let modes = self.layers.mode_state();
        let mut render_modes = Vec::new();
        if let Some(id) = modes.braille {
            render_modes.push(LayerRenderMode {
                layer: id,
                mode: RenderMode::Braille,
            });
        }
        if let Some(id) = modes.color {
            render_modes.push(LayerRenderMode {
                layer: id,
                mode: RenderMode::Color,
            });
        }
        if let Some(id) = modes.text {
            render_modes.push(LayerRenderMode {
                layer: id,
                mode: RenderMode::Text,
            });
        }
        let state = StateConfig {
            center_lat: center.lat,
            center_lon: center.lon,
            zoom: self.viewport.zoom,
            enabled_layers: self.layers.saved_enabled(),
            selected_layer: self.layers.selected_layer(),
            render_modes,
            braille_layer: None,
            color_layer: None,
            text_layer: None,
        };
        let path = self.dirs.config_dir.join("state.toml");
        let _ = state.save(&path);
    }

    pub fn load_state(&mut self) {
        let path = self.dirs.config_dir.join("state.toml");
        let state = match StateConfig::load(&path) {
            Ok(Some(s)) => s,
            // No saved state — use config/viewport defaults already set in boot.
            Ok(None) => return,
            Err(_) => return,
        };
        self.viewport = Viewport::from_lat_lon(state.center_lat, state.center_lon, state.zoom);
        self.layers.restore_enabled(&state.enabled_layers);
        self.layers.set_selected(state.selected_layer);
        let modes = self.layers.mode_state_mut();
        if !state.render_modes.is_empty() {
            // New format: explicit (layer, mode) pairs.
            for entry in &state.render_modes {
                modes.assign(entry.mode, entry.layer);
            }
        } else {
            // Backward compat: old state.toml with scalar braille/color/text fields.
            if let Some(id) = state.braille_layer {
                modes.assign(RenderMode::Braille, id);
            }
            if let Some(id) = state.color_layer {
                modes.assign(RenderMode::Color, id);
            }
            if let Some(id) = state.text_layer {
                modes.assign(RenderMode::Text, id);
            }
        }
        // Guarantee an obs layer is always in text mode so the staleness
        // check can trigger obs refresh.  Old state files may have saved
        // text mode on a non-obs layer (e.g. Radar), which would leave
        // any_surface_enabled() returning false forever.
        if !self.any_obs_enabled() {
            self.layers
                .mode_state_mut()
                .assign(RenderMode::Text, LayerId::SurfTemp);
        }
    }

    pub fn any_obs_enabled(&self) -> bool {
        [
            LayerId::SurfTemp,
            LayerId::SurfWind,
            LayerId::SurfHumidity,
            LayerId::SurfPressure,
        ]
        .iter()
        .any(|id| self.layers.enabled(*id))
    }

    pub fn set_obs_status(&mut self, status: LayerStatus) {
        for id in [
            LayerId::SurfTemp,
            LayerId::SurfWind,
            LayerId::SurfHumidity,
            LayerId::SurfPressure,
        ] {
            self.layers.set_status(id, status.clone());
        }
    }

    pub fn has_obs_task(&self) -> bool {
        self.obs_task.is_some()
    }
}

#[derive(Debug)]
struct RadarRefreshResult {
    id: u64,
    result: RadarRefreshPayload,
}

#[derive(Debug)]
enum RadarRefreshPayload {
    /// One tile ready in a streaming frame load.  The receiver should
    /// merge this into the current `radar_frame` incrementally.
    Tile(RadarTile),
    /// All tiles have been streamed.  The frame carries metadata
    /// (time/path/zoom).  Its `tiles` vec may be empty — tiles were
    /// already delivered via `Tile`.
    Ready(RadarFrame),
    Error(String),
}

#[derive(Debug)]
struct BorderRefreshResult {
    id: u64,
    result: BorderRefreshPayload,
}

#[derive(Debug)]
enum BorderRefreshPayload {
    Ready(BorderLayer),
    /// Background tile generation finished for this resolution.
    TilesBuilt(BorderResolution),
    Error(String),
}

#[derive(Debug)]
struct WarnRefreshResult {
    id: u64,
    result: WarnRefreshPayload,
}

#[derive(Debug)]
enum WarnRefreshPayload {
    Ready(WarningLayer),
    Error(String),
}

#[derive(Debug)]
struct ObsRefreshResult {
    id: u64,
    result: ObsRefreshPayload,
}

#[derive(Debug)]
enum ObsRefreshPayload {
    /// A single observation point streamed as it arrives.
    Point(ObservationPoint),
    /// Commit the points received so far into obs_cache without finalising;
    /// more points are still coming (e.g. from the next loading phase).
    PartialCommit,
    /// All points have been sent successfully.
    Ready,
    /// The fetch failed.
    Error(String),
    /// The fetch is complete (success or failure).
    Done,
}

async fn initial_viewport(cli: &Cli, log_path: &Path) -> (Viewport, String, Option<GeoPoint>) {
    if let (Some(lat), Some(lon)) = (cli.lat, cli.lon) {
        let point = GeoPoint::new(lon, lat);
        return (
            Viewport::from_lat_lon(lat, lon, cli.zoom.unwrap_or(5.0)),
            "CLI".to_string(),
            Some(point),
        );
    }

    if !cli.no_location {
        if let Ok(Some(fix)) = crate::providers::geoclue::locate(log_path).await {
            let point = fix.point;
            return (
                Viewport::from_lat_lon(fix.point.lat, fix.point.lon, cli.zoom.unwrap_or(5.0)),
                fix.label,
                Some(point),
            );
        }
    }

    (
        Viewport::from_lat_lon(
            crate::geo::EUROPE_LAT,
            crate::geo::EUROPE_LON,
            cli.zoom.unwrap_or(crate::geo::EUROPE_ZOOM),
        ),
        "Europe fallback".to_string(),
        None,
    )
}

// ── Background task queue ───────────────────────────────────────────

use std::sync::atomic::AtomicU64;

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

/// Kinds of background tasks for colour-coding in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    BorderDownload,
    BorderTileGen,
    RadarFrame,
    Observation,
    Warning,
    FrameList,
}

impl TaskKind {
    pub fn color(&self) -> ratatui::style::Color {
        match self {
            Self::BorderDownload => ratatui::style::Color::LightBlue,
            Self::BorderTileGen => ratatui::style::Color::LightGreen,
            Self::RadarFrame => ratatui::style::Color::LightYellow,
            Self::Observation => ratatui::style::Color::LightMagenta,
            Self::Warning => ratatui::style::Color::LightRed,
            Self::FrameList => ratatui::style::Color::LightCyan,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::BorderDownload => "border",
            Self::BorderTileGen => "tiles",
            Self::RadarFrame => "radar",
            Self::Observation => "obs",
            Self::Warning => "warn",
            Self::FrameList => "frames",
        }
    }
}

/// A task progress message sent from background tasks to the UI.
#[derive(Debug)]
pub enum TaskMsg {
    Start {
        id: u64,
        label: String,
        kind: TaskKind,
    },
    Progress {
        id: u64,
        action: String,
        fraction: f64,
    },
    Complete {
        id: u64,
    },
    Error {
        id: u64,
        error: String,
    },
}

/// Current state of a background task as known by the UI.
#[derive(Debug)]
pub struct ActiveTask {
    pub id: u64,
    pub label: String,
    pub action: String,
    pub fraction: f64,
    /// Smoothly-animated display value.  Updated by animating `anim_t` from
    /// 0→1 and applying smoothstep so the bar eases in and out between
    /// each discrete progress update.
    pub display_fraction: f64,
    /// Value of `display_fraction` when the current animation segment began.
    pub anim_from: f64,
    /// 0→1 progress through the current animation segment (smoothstep applied).
    pub anim_t: f64,
    pub kind: TaskKind,
    pub state: TaskState,
    pub completed_at: Option<Instant>,
    pub last_anim: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Running,
    Completed,
    Error,
    /// A new task of the same kind was started after this one was
    /// aborted — the entry is kept briefly so the UI doesn't flicker.
    Superseded,
}

pub fn next_task_id() -> u64 {
    NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed)
}

/// Spawn a background tile-generation task.  Generates all cached tiles for
/// `res` from `lines`, sends `TilesBuilt` on `tx` when done, and completes
/// `tile_task_id` on `task_tx`.
#[allow(clippy::too_many_arguments)]
fn spawn_tile_gen(
    maps: NaturalEarthProvider,
    res: BorderResolution,
    lines: Vec<BorderLine>,
    log_path: std::path::PathBuf,
    result_id: u64,
    tx: UnboundedSender<BorderRefreshResult>,
    task_tx: UnboundedSender<TaskMsg>,
    tile_task_id: u64,
) {
    tokio::spawn(async move {
        let gen_start = Instant::now();
        let gen_ok = tokio::task::spawn_blocking(move || maps.generate_all_tiles(res, &lines))
            .await
            .is_ok_and(|r| r.is_ok());
        if gen_ok {
            write_log(
                &log_path,
                format!(
                    "tile_gen: {} tiles built in {:?}",
                    res.label(),
                    gen_start.elapsed()
                ),
            );
            let _ = tx.send(BorderRefreshResult {
                id: result_id,
                result: BorderRefreshPayload::TilesBuilt(res),
            });
        } else {
            write_log(
                &log_path,
                format!("tile_gen: {} generation failed", res.label()),
            );
        }
        let _ = task_tx.send(TaskMsg::Complete { id: tile_task_id });
    });
}
