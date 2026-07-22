use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};
use color_eyre::eyre::{Result, WrapErr};
use futures::stream::{FuturesUnordered, StreamExt};
use reqwest::Client;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::cache::{write_log, FrontDirs};
use crate::cli::Cli;
use crate::config::{
    apply_config_edits, Config, ConfigEditValue, EumetnetConfig, LayerRenderMode, StateConfig,
};
use crate::geo::{haversine_m, world_to_lat_lon, GeoPoint, Viewport, WorldPoint};
use crate::layers::{
    resolution_distance, BorderLayer, BorderLine, BorderLineKind, BorderResolution, LayerId,
    LayerRegistry, LayerStatus, ObservationLayer, ObservationPoint, RadarFrame, RadarTile,
    RenderMode, WarningLayer,
};
use crate::providers::eumetnet::EumetnetProvider;
use crate::providers::geocode::{GeocodeProvider, Place};
use crate::providers::location::{LocationArbiter, LocationFix, LocationSource};
use crate::providers::maps::NaturalEarthProvider;
use crate::providers::meteoalarm::MeteoAlarmProvider;
use crate::providers::meteogate::MeteoGateProvider;
use crate::providers::verify::VerifyOutcome;
use crate::retry::RetryPolicy;
use crate::settings::SettingsModel;
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

/// Impact animation duration shared between `App` (prune/animate logic)
/// and `ui` (frame selection).
pub(crate) const LIGHTNING_IMPACT_MS: u32 = 900;

struct RadarPreloadResult {
    timestamp: i64,
    tile_zoom: u8,
    /// `None` when the fetch failed; the slot is then queued for another
    /// attempt rather than left as a permanent hole in the timeline.
    frame: Option<RadarFrame>,
}

/// Retry bookkeeping for a radar slot whose fetch failed.
///
/// A frame that fails once is not abandoned: the timeline would keep a gap
/// that nothing ever fills, since a slot is only re-requested when something
/// else happens to trigger a preload pass.
#[derive(Debug, Clone, Copy)]
struct FrameRetry {
    attempts: u32,
    /// Earliest instant at which this slot may be requested again.
    next_at: Instant,
}

/// How far a location fix must move before its settlement label is looked up
/// again.  Well under the size of any town, so the name stays correct, but far
/// enough that GPS jitter and accuracy refinements cost nothing.
const LABEL_REFRESH_M: f64 = 2_000.0;

/// First wait before re-requesting a failed radar slot.  Doubles per attempt.
const FRAME_RETRY_BASE: Duration = Duration::from_secs(2);

/// Ceiling on the retry backoff.  A slot that has been failing for a while is
/// polled at this interval, so a frame that becomes available after an outage
/// still fills itself in.
const FRAME_RETRY_MAX: Duration = Duration::from_secs(90);

/// Attempts after which a slot is given up on for this session.
///
/// Retrying forever is only right when the cause is transient.  Some failures
/// are properties of the data itself — a composite this build cannot decode —
/// and those repeat identically no matter how long you wait, burning a download
/// and a decode each time.  Giving up keeps the timeline honest (the slot reads
/// as unavailable rather than perpetually loading) and stops the churn; a
/// history reload or restart clears the slate and tries again.
const FRAME_RETRY_GIVE_UP: u32 = 8;

/// Shared backoff policy for radar frame retries, built from the constants
/// above. `attempts` here is 1-based (pre-incremented before use in
/// `note_frame_failure`), so `delay_for(entry.attempts)` — not `attempts - 1`
/// — reproduces the existing sequence: first failure is `base * 2^1`.
const FRAME_RETRY_POLICY: RetryPolicy =
    RetryPolicy::new(FRAME_RETRY_BASE, FRAME_RETRY_MAX, Some(FRAME_RETRY_GIVE_UP));

/// How many uncached frames a single preload pass will pull in, nearest the
/// playhead first.  Caps the work per trigger independently of how deep the
/// history window is; the window re-centres as the playhead moves, so the rest
/// of a 24 h timeline streams in as it is approached rather than all at once.
const PRELOAD_WINDOW: usize = 36;

/// Cap on decoded frames held in RAM, evicted by distance from the playhead.
///
/// `frame_cache` holds built tiles, ~5 MB per frame at zoom 7.  Without a cap
/// it grows to the full timeline as the playhead sweeps: 24 h is 288 slots,
/// about 1.4 GB.  The GeoTIFFs stay on disk, so an evicted frame reloads
/// without touching the network — RAM holds a window, disk holds the day.
const FRAME_CACHE_MAX: usize = 48;

/// Steps between `index` and `playhead` along the timeline, the short way
/// round.
///
/// The timeline is a ring, not a line: playback runs oldest → newest and then
/// wraps straight back to the oldest, and `[`/`]` step across the same seam.
/// So the oldest frame is one step from the newest, not `len - 1` steps.  A
/// plain `abs_diff` ranks the frames just past the seam as the most distant on
/// the timeline — precisely the ones about to be reached — which left preload
/// skipping them and eviction dropping them first, stalling every lap at the
/// wrap.
///
/// The metric is symmetric rather than looking only ahead of the playhead:
/// stepping goes both ways, and frames behind the playhead are the ones the
/// next lap replays.
fn ring_distance(index: usize, playhead: usize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let raw = index.abs_diff(playhead);
    raw.min(len - raw)
}

/// Which way a single step moves the playhead.  Index 0 is the newest frame,
/// `len - 1` the oldest, so `Older` counts up and `Newer` counts down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Older,
    Newer,
}

/// The index one step from `current` around a timeline of `len` slots, wrapping
/// at both ends.  `None` when there is no timeline to step along.
fn stepped_index(current: usize, len: usize, dir: Step) -> Option<usize> {
    let oldest = len.checked_sub(1)?;
    Some(match dir {
        // `>=`, not `==`: a shrinking history window can leave the playhead
        // past the end until the next redraw resettles it.
        Step::Older if current >= oldest => 0,
        Step::Older => current + 1,
        Step::Newer if current == 0 => oldest,
        Step::Newer => (current - 1).min(oldest),
    })
}

/// Pick which cached frames to drop so at most `cap` remain.
///
/// Ranks by [`ring_distance`] from the playhead rather than by insert order:
/// preload fills outward from the playhead, so the frames worth keeping are the
/// ones it is about to reach, not the ones most recently decoded.  The
/// displayed frame is at distance zero and is never evicted.  Frames no longer
/// on the timeline rank last and go first.
fn frames_to_evict(cached: &[i64], timestamps: &[i64], frame_index: usize, cap: usize) -> Vec<i64> {
    if cached.len() <= cap {
        return Vec::new();
    }
    let index: HashMap<i64, usize> = timestamps
        .iter()
        .enumerate()
        .map(|(i, &ts)| (ts, i))
        .collect();
    let mut keys = cached.to_vec();
    keys.sort_by_key(|ts| {
        index
            .get(ts)
            .map(|&i| ring_distance(i, frame_index, timestamps.len()))
            .unwrap_or(usize::MAX)
    });
    keys.split_off(cap)
}

/// Resets the observation accumulator for a freshly kicked-off refresh.
///
/// Called once per refresh, unconditionally, rather than lazily on the first
/// `Point` — a refresh that errors before producing any `Point` still needs
/// the previous refresh's leftover `obs_partial` cleared.
fn reset_obs_accumulator(
    obs_incoming: &mut Vec<ObservationPoint>,
    obs_incoming_id: &mut u64,
    obs_partial: &mut Vec<ObservationPoint>,
    new_id: u64,
) {
    obs_incoming.clear();
    *obs_incoming_id = new_id;
    obs_partial.clear();
}

/// Remove `req_ts` from `timestamps` and work out which index should stay
/// displayed, or `None` when the slot wasn't on the timeline.
///
/// The viewer keeps looking at the same *time* across the removal rather
/// than the same index, which would otherwise slide onto a neighbour.  If
/// the viewed slot was the phantom itself, it falls back to the time the
/// provider actually resolved to.
fn timeline_without_phantom(
    timestamps: &[i64],
    frame_index: usize,
    req_ts: i64,
    resolved_ts: i64,
    live: bool,
) -> Option<(Vec<i64>, usize)> {
    if !timestamps.contains(&req_ts) {
        return None;
    }
    let viewing = timestamps.get(frame_index).copied();
    let remaining: Vec<i64> = timestamps
        .iter()
        .copied()
        .filter(|&t| t != req_ts)
        .collect();
    let index = if live {
        0
    } else {
        let target = viewing.filter(|&t| t != req_ts).unwrap_or(resolved_ts);
        remaining.iter().position(|&t| t == target).unwrap_or(0)
    };
    Some((remaining, index))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackMode {
    /// Always displays the latest frame; auto-advances as new data arrives.
    Live,
    /// Holds on the current frame; no auto-advance.
    Paused,
    /// Steps forward through history automatically at `playback_speed`.
    Playing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackSpeed {
    Half,
    Normal,
    Double,
    Quad,
}

impl PlaybackSpeed {
    pub fn interval_ms(self) -> u64 {
        match self {
            PlaybackSpeed::Half => 2000,
            PlaybackSpeed::Normal => 1000,
            PlaybackSpeed::Double => 500,
            PlaybackSpeed::Quad => 250,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PlaybackSpeed::Half => "½×",
            PlaybackSpeed::Normal => "1×",
            PlaybackSpeed::Double => "2×",
            PlaybackSpeed::Quad => "4×",
        }
    }

    pub fn faster(self) -> Self {
        match self {
            PlaybackSpeed::Half => PlaybackSpeed::Normal,
            PlaybackSpeed::Normal => PlaybackSpeed::Double,
            PlaybackSpeed::Double => PlaybackSpeed::Quad,
            PlaybackSpeed::Quad => PlaybackSpeed::Quad,
        }
    }

    pub fn slower(self) -> Self {
        match self {
            PlaybackSpeed::Half => PlaybackSpeed::Half,
            PlaybackSpeed::Normal => PlaybackSpeed::Half,
            PlaybackSpeed::Double => PlaybackSpeed::Normal,
            PlaybackSpeed::Quad => PlaybackSpeed::Double,
        }
    }
}

#[derive(Debug)]
pub struct App {
    pub viewport: Viewport,
    pub layers: LayerRegistry,
    pub borders: Option<BorderLayer>,
    pub timestamps: Vec<i64>,
    /// Depth of radar history on the timeline, in hours.  Cycled with `i`
    /// through [`HISTORY_OPTIONS`](crate::providers::meteogate::HISTORY_OPTIONS).
    pub history_hours: u8,
    /// Timestamps whose GeoTIFF is on disk.  A superset of `frame_cache` once
    /// eviction starts: these still load without a fetch, so the timeline marks
    /// them as available rather than missing.
    pub disk_frames: HashSet<i64>,
    /// Slots whose fetch failed, with when to try them again.  Entries are
    /// removed on success and pruned when the slot leaves the timeline.
    radar_failures: HashMap<i64, FrameRetry>,
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
    pub frame_index: usize,
    pub playback_mode: PlaybackMode,
    pub playback_speed: PlaybackSpeed,
    pub show_help: bool,
    /// Whether the bottom-right colour-scale key is drawn.  Toggled by `g`;
    /// on by default.
    pub show_legend: bool,
    pub is_dragging: bool,
    /// False when the layer panel is defocused (dimmed, no selection indicators,
    /// submenu hidden).  Toggled by Alt+← from the root list; set to true by
    /// any layer interaction.  True on startup.
    pub layer_panel_focused: bool,
    /// Picks the winning fix out of the competing backend streams.
    location: LocationArbiter,
    /// The `/` prompt's buffer while open; `None` when the prompt is closed.
    pub search_input: Option<String>,
    /// The settings modal's editing session; `Some` while open. One overlay
    /// at a time — guarded against opening alongside the search prompt or
    /// help modal in `open_settings`.
    pub settings: Option<SettingsState>,
    /// Screen rects of clickable provider links in the settings modal, recorded
    /// by the renderer each frame as `(x, y, width, url)` so the event loop can
    /// hit-test a click and open the browser. Only meaningful while the modal
    /// is open; stale entries are never read otherwise.
    pub settings_links: Vec<(u16, u16, u16, &'static str)>,
    /// Index into `settings_links` the mouse is currently hovering, if any —
    /// drives the link's hover highlight.
    pub settings_link_hover: Option<usize>,
    /// Index into `settings_links` currently pressed (left button down on it) —
    /// drives the link's pressed (inverted) style until the button releases.
    pub settings_link_pressed: Option<usize>,
    /// Status line shown under the prompt: the matched place, "searching…",
    /// or why the search failed.
    pub search_status: Option<String>,
    /// Where the search pin currently sits, if a search matched.
    search_pin: Option<GeoPoint>,
    pub dirs: FrontDirs,
    pub config: Config,
    /// HTTP client shared with every provider. `Client` is cheap to clone
    /// (internally `Arc`-backed), kept here so an edited provider can be
    /// rebuilt (see `rebuild_eumetnet_provider`) without re-building a client.
    client: Client,
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
    /// Rendered radar frames cached by timestamp, valid for `frame_cache_zoom`.
    pub frame_cache: HashMap<i64, RadarFrame>,
    /// Tile zoom the frame_cache entries were built at; cleared on change.
    frame_cache_zoom: u8,
    /// Single sequential background task preloading uncached radar frames.
    radar_preload_task: Option<JoinHandle<()>>,
    radar_preload_tx: UnboundedSender<RadarPreloadResult>,
    radar_preload_rx: UnboundedReceiver<RadarPreloadResult>,
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
    verify_tx: UnboundedSender<VerifyResult>,
    verify_rx: UnboundedReceiver<VerifyResult>,
    verify_task: Option<JoinHandle<()>>,
    verify_refresh_id: u64,
    /// Outcome of the most recently completed verify probe, keyed by which
    /// provider it targeted. Overwritten by each new verify; CP-5 reads this
    /// to render Valid/Invalid/Unreachable next to the field. `None` until
    /// the first verify for that target completes.
    pub last_verify: Option<(VerifyTarget, VerifyOutcome)>,
    /// Fixes from every location backend.  `None` when location is disabled
    /// (`--no-location`) or fixed by `--lat/--lon`, in which case no backend
    /// is ever started.
    location_rx: Option<UnboundedReceiver<LocationFix>>,
    geocode: Arc<GeocodeProvider>,
    search_tx: UnboundedSender<SearchResult>,
    search_rx: UnboundedReceiver<SearchResult>,
    search_task: Option<JoinHandle<()>>,
    /// Discriminates in-flight searches so a slow earlier query cannot
    /// overwrite the pin set by a later one.
    search_id: u64,
    /// Settlement name for the "you are here" marker, once reverse geocoding
    /// has resolved one.  `None` until then, or when the fix is somewhere with
    /// no named settlement.
    location_label: Option<String>,
    /// Where `location_label` was resolved for, so a refined fix a few metres
    /// away doesn't spend another Nominatim request.
    location_label_at: Option<GeoPoint>,
    location_label_task: Option<tokio::task::JoinHandle<()>>,
    location_label_tx: UnboundedSender<(GeoPoint, Option<String>)>,
    location_label_rx: UnboundedReceiver<(GeoPoint, Option<String>)>,
    /// Settlement name for the search pin, taken from the geocoding result
    /// that placed it — no extra request needed.
    search_label: Option<String>,
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
    /// Active strikes `(world_position, arrival_instant, polarity)`.
    /// polarity > 0 = positive (rare), ≤ 0 = negative (common).  Pruned each tick.
    pub lightning_strikes: Vec<(WorldPoint, std::time::Instant, i8)>,
    #[cfg_attr(not(feature = "lightning"), allow(dead_code))]
    lightning_tx: UnboundedSender<(WorldPoint, i8)>,
    lightning_rx: UnboundedReceiver<(WorldPoint, i8)>,
    lightning_task: Option<JoinHandle<()>>,
    lightning_cancel: Option<Arc<AtomicBool>>,
    lightning_close_tx: Option<tokio::sync::oneshot::Sender<()>>,
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
            .pool_max_idle_per_host(64)
            .build()
            .wrap_err("build HTTP client")?;

        write_log(&log, "boot: initial viewport");
        let (viewport, location, location_rx) = initial_viewport(cli, &config, &log).await;
        let (refresh_tx, refresh_rx) = unbounded_channel();
        let (border_tx, border_rx) = unbounded_channel();
        let (obs_tx, obs_rx) = unbounded_channel();
        let (warn_tx, warn_rx) = unbounded_channel();
        let (verify_tx, verify_rx) = unbounded_channel();
        let (search_tx, search_rx) = unbounded_channel();
        let (location_label_tx, location_label_rx) = unbounded_channel();
        let (task_tx, task_rx) = unbounded_channel();
        let (frame_list_tx, frame_list_rx) = unbounded_channel();
        let (radar_preload_tx, radar_preload_rx) = unbounded_channel::<RadarPreloadResult>();
        let (lightning_tx, lightning_rx) = unbounded_channel::<(WorldPoint, i8)>();
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
        let maps = NaturalEarthProvider::new(client.clone(), dirs.clone(), cancel.clone());
        let mut app = Self {
            viewport,
            layers: LayerRegistry::new(),
            borders: None,
            timestamps: Vec::new(),
            history_hours: crate::providers::meteogate::DEFAULT_HISTORY_HOURS,
            disk_frames: HashSet::new(),
            radar_failures: HashMap::new(),
            radar_frame: None,
            border_mask_cache: None,
            fallback_mask_cache: None,
            border_layers: HashMap::new(),
            border_layers_version: 0,
            border_tiles_built: 0,
            border_total_resolutions: 4,
            border_built_set: HashSet::new(),
            braille_frame: BrailleFrame::default(),
            frame_index: 0,
            playback_mode: PlaybackMode::Live,
            playback_speed: PlaybackSpeed::Normal,
            show_help: false,
            show_legend: true,
            is_dragging: false,
            layer_panel_focused: false,
            location,
            location_rx,
            search_input: None,
            settings: None,
            settings_links: Vec::new(),
            settings_link_hover: None,
            settings_link_pressed: None,
            search_status: None,
            search_pin: None,
            geocode: Arc::new(
                GeocodeProvider::new(config.geocode.endpoint.clone())
                    .wrap_err("build geocoding provider")?,
            ),
            search_tx,
            search_rx,
            search_task: None,
            search_id: 0,
            location_label: None,
            location_label_at: None,
            location_label_task: None,
            location_label_tx,
            location_label_rx,
            search_label: None,
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
            frame_cache: HashMap::new(),
            frame_cache_zoom: 0,
            radar_preload_task: None,
            radar_preload_tx,
            radar_preload_rx,
            dirs,
            config,
            client,
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
            verify_tx,
            verify_rx,
            verify_task: None,
            verify_refresh_id: 0,
            last_verify: None,
            task_tx,
            task_rx,
            active_tasks: Vec::new(),
            cancel,
            preload_tasks: Vec::new(),
            frame_list_tx,
            frame_list_rx,
            lightning_strikes: Vec::new(),
            lightning_tx,
            lightning_rx,
            lightning_task: None,
            lightning_cancel: None,
            lightning_close_tx: None,
        };

        write_log(&log, "boot: loading saved state");
        app.load_state();

        // If lightning was enabled in the last session, connect immediately
        // so data starts arriving before the user interacts with the panel.
        if app.layers.enabled(LayerId::Lightning) {
            write_log(&log, "boot: lightning enabled, connecting");
            app.request_lightning_connect();
        }

        // Launch border loading in background — never block boot on it.
        write_log(
            &log,
            format!("boot: spawning border load for zoom {}", app.viewport.zoom),
        );
        app.request_border_refresh();

        // Caches from builds that stored the source GeoTIFF are dead weight.
        let freed = app.meteogate.purge_legacy_tiffs();
        if freed > 0 {
            write_log(
                &log,
                format!(
                    "boot: freed {} MB of legacy geotiff cache",
                    freed / 1_000_000
                ),
            );
        }

        // What the last session left on disk is already usable — show it on the
        // timeline from the first render rather than after a fetch proves it.
        app.disk_frames = app.meteogate.cached_timestamps();
        write_log(
            &log,
            format!("boot: {} radar frames on disk", app.disk_frames.len()),
        );

        // Launch radar frame list fetch in background.
        write_log(&log, "boot: spawning background frame list fetch");
        {
            let meteogate = app.meteogate.clone();
            let tx = app.frame_list_tx.clone();
            let task_tx = app.task_tx.clone();
            let hours = app.history_hours;
            let task_id = next_task_id();
            let _ = task_tx.send(TaskMsg::Start {
                id: task_id,
                label: "frame list".into(),
                kind: TaskKind::FrameList,
            });
            let ll = log.clone();
            tokio::spawn(async move {
                match meteogate.frame_list(hours).await {
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

    pub fn request_meteogate_refresh(&mut self, width: u16, height: u16) {
        if !self.layers.enabled(LayerId::Radar) {
            return;
        }
        let Some(ts) = self.timestamps.get(self.frame_index).copied() else {
            return;
        };
        let tile_zoom = self.viewport.zoom.round().clamp(1.0, 7.0) as u8;

        // Invalidate frame cache when zoom level changes.
        if tile_zoom != self.frame_cache_zoom {
            self.frame_cache.clear();
            self.frame_cache_zoom = tile_zoom;
            if let Some(task) = self.radar_preload_task.take() {
                task.abort();
            }
        }

        let bounds = self.viewport.bounds(width, height);

        // Serve from cache when the cached frame already covers the viewport.
        if let Some(cached) = self.frame_cache.get(&ts) {
            if cached.covers_bounds(bounds, tile_zoom) {
                let mut frame = cached.clone();
                frame.trim_to_bounds(bounds);
                self.radar_frame = Some(frame);
                self.radar_requested_ts = Some(ts);
                self.radar_frame_ts = Some(ts);
                self.layers.set_status(LayerId::Radar, LayerStatus::Ready);
                self.trigger_radar_preload();
                return;
            }
        }

        // Cache miss or stale coverage: abort preload to avoid grid_cache
        // mutex contention, then proceed with normal streaming fetch.
        if let Some(task) = self.radar_preload_task.take() {
            task.abort();
        }

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
                                fraction: Some(fraction),
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
        if let Some(task) = self.radar_preload_task.take() {
            task.abort();
        }
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
        if let Some(task) = self.radar_preload_task.take() {
            task.abort();
        }
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
                fraction: Some(0.3),
            });
            let result = match maps
                .borders_for_resolution(desired, bounds, spawn_cancel)
                .await
            {
                Ok(layer) => {
                    let _ = task_tx.send(TaskMsg::Progress {
                        id: task_id,
                        action: "building grid".into(),
                        fraction: Some(0.8),
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
        let fresh = crate::providers::meteogate::compute_frame_list(self.history_hours);
        if fresh.first() == self.timestamps.first() {
            return false;
        }
        let current_ts = self.timestamps.get(self.frame_index).copied();
        let is_live = self.playback_mode == PlaybackMode::Live;
        self.timestamps = fresh;
        self.frame_index = if is_live {
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

    /// Spawn a background verify probe for `target` against `candidate_key`
    /// — the staged value in the settings modal, not necessarily the stored
    /// config value — and report through the task overlay as an
    /// indeterminate task (CP-5 will call this from the modal). The outcome
    /// is delivered back via `drain_verify_results` into `self.last_verify`.
    pub fn request_verify(&mut self, target: VerifyTarget, candidate_key: String) {
        if let Some(task) = self.verify_task.take() {
            task.abort();
        }
        self.verify_refresh_id = self.verify_refresh_id.wrapping_add(1);
        let id = self.verify_refresh_id;
        let task_id = next_task_id();
        let task_tx = self.task_tx.clone();
        let _ = task_tx.send(TaskMsg::Start {
            id: task_id,
            label: "verify".into(),
            kind: TaskKind::Verify,
        });
        // Verify has no measurable progress steps — flip to an indeterminate
        // marquee immediately so the row doesn't freeze at a determinate 0%.
        let _ = task_tx.send(TaskMsg::Progress {
            id: task_id,
            action: String::new(),
            fraction: None,
        });
        let tx = self.verify_tx.clone();
        let eumetnet = self.eumetnet.clone();
        self.verify_task = Some(tokio::spawn(async move {
            let outcome = match target {
                VerifyTarget::Eumetnet => eumetnet.verify_api_key(&candidate_key).await,
            };
            match outcome {
                VerifyOutcome::Unreachable => {
                    let _ = task_tx.send(TaskMsg::Error {
                        id: task_id,
                        error: "unreachable".into(),
                    });
                }
                VerifyOutcome::Valid | VerifyOutcome::Invalid => {
                    let _ = task_tx.send(TaskMsg::Complete { id: task_id });
                }
            }
            let _ = tx.send(VerifyResult {
                id,
                target,
                outcome,
            });
        }));
    }

    pub fn drain_verify_results(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.verify_rx.try_recv() {
            if result.id != self.verify_refresh_id {
                continue;
            }
            self.verify_task = None;
            self.last_verify = Some((result.target, result.outcome));
            changed = true;
        }
        changed
    }

    /// Whether an edited eumetnet config requires rebuilding the provider.
    /// Only `api_key` affects provider behavior (budget/quota, auth header);
    /// trims both sides so whitespace-only edits don't trigger a needless
    /// rebuild (which would reset `backdrop_loaded` and the request budget).
    pub fn eumetnet_rebuild_needed(old: &EumetnetConfig, new: &EumetnetConfig) -> bool {
        old.api_key.trim() != new.api_key.trim()
    }

    /// Rebuild the eumetnet provider from the current config and re-kick its
    /// refresh so an edited `api_key` takes effect without a restart.
    ///
    /// Precondition: the caller has already mutated `self.config.eumetnet`
    /// (e.g. after a settings-modal save) — this method does not write
    /// config to disk. `request_obs_refresh` bumps `obs_refresh_id`, so
    /// results already in flight from the old provider are discarded by the
    /// existing staleness check.
    pub fn rebuild_eumetnet_provider(&mut self) {
        self.eumetnet = EumetnetProvider::new(
            self.client.clone(),
            self.dirs.clone(),
            self.config.eumetnet.clone(),
        );
        // Intentional no-op when no obs layer is enabled: the swap above still
        // happens, there's just nothing to refresh.
        self.request_obs_refresh();
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
        // Reset the accumulator here — once per refresh, unconditionally —
        // rather than lazily on the first `Point`. A refresh that errors
        // before producing any `Point` would otherwise never clear
        // `obs_partial`, leaving the previous refresh's data to linger.
        reset_obs_accumulator(
            &mut self.obs_incoming,
            &mut self.obs_incoming_id,
            &mut self.obs_partial,
            id,
        );
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
                                        fraction: Some(frac),
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
                    // Trim off-screen tiles so the tile list doesn't grow
                    // unbounded across successive pan-triggered refreshes.
                    if let Some(rf) = &mut self.radar_frame {
                        let b = self.viewport.bounds(self.map_width, self.map_height);
                        rf.trim_to_bounds(b);
                    }
                    self.layers.set_status(LayerId::Radar, LayerStatus::Ready);
                    self.refresh_task = None;
                    // If the provider resolved a different timestamp than requested,
                    // that slot is a phantom (not yet published on S3). Remove it so
                    // it can't masquerade as a distinct frame on the timeline.
                    if let (Some(req_ts), Some(actual_ts)) = (
                        self.radar_requested_ts,
                        self.radar_frame.as_ref().map(|f| f.time),
                    ) {
                        if actual_ts != req_ts {
                            self.drop_phantom_slot(req_ts, actual_ts);
                        }
                    }
                    // Cache the completed frame and kick off preload for the rest.
                    // Key by the frame's real time, not the requested slot, so a
                    // resolved-elsewhere frame can't occupy two timeline slots.
                    // It loaded — drop any retry bookkeeping for this slot.
                    if let Some(req) = self.radar_requested_ts {
                        self.radar_failures.remove(&req);
                    }
                    if let Some(f) = self.radar_frame.as_ref() {
                        let ts = f.time;
                        self.radar_failures.remove(&ts);
                        let zoom = self.viewport.zoom.round().clamp(1.0, 7.0) as u8;
                        if zoom == self.frame_cache_zoom && self.timestamps.contains(&ts) {
                            let f = f.clone();
                            self.frame_cache.insert(ts, f);
                            // Evict oldest entries beyond 6 to bound memory.
                            if self.frame_cache.len() > 6 {
                                let evict_ts = self.frame_cache.keys().copied().min().unwrap_or(ts);
                                self.frame_cache.remove(&evict_ts);
                            }
                        }
                    }
                    self.trigger_radar_preload();
                }
                RadarRefreshPayload::Error(error) => {
                    self.layers
                        .set_status(LayerId::Radar, LayerStatus::Error(error));
                    self.refresh_task = None;
                    // Queue the displayed slot for another attempt.  Showing an
                    // error and then never trying again left the map blank until
                    // the user happened to pan.
                    if let Some(ts) = self.radar_requested_ts {
                        self.note_frame_failure(ts);
                    }
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
            let current_ts = self.timestamps.get(self.frame_index).copied();
            self.timestamps = ts;
            self.frame_index = if self.playback_mode == PlaybackMode::Live {
                0
            } else {
                current_ts
                    .and_then(|t| self.timestamps.iter().position(|&x| x == t))
                    .unwrap_or(0)
            };
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

    /// The current best-known position, or `None` if nothing has been fixed
    /// yet.
    pub fn location_fix(&self) -> Option<LocationFix> {
        self.location.current()
    }

    /// Where the search pin sits, or `None` when nothing is pinned.
    pub fn search_pin(&self) -> Option<GeoPoint> {
        self.search_pin
    }

    /// Settlement name to draw beside the "you are here" marker.
    pub fn location_label(&self) -> Option<&str> {
        self.location_label.as_deref()
    }

    /// Settlement name to draw beside the search pin.
    pub fn search_label(&self) -> Option<&str> {
        self.search_label.as_deref()
    }

    /// Ask Nominatim what settlement `point` is in, unless we already know.
    ///
    /// Skipped when the previous lookup was for somewhere within
    /// [`LABEL_REFRESH_M`]: platform backends emit refinements continuously,
    /// and each is a fresh fix even when it moves the marker by metres.  One
    /// request per genuine relocation keeps us well inside Nominatim's 1 req/s
    /// policy without any extra throttling here.
    fn request_location_label(&mut self, point: GeoPoint) {
        if let Some(prev) = self.location_label_at {
            if haversine_m(prev, point) < LABEL_REFRESH_M {
                return;
            }
        }
        self.location_label_at = Some(point);
        if let Some(task) = self.location_label_task.take() {
            task.abort();
        }
        let geocode = self.geocode.clone();
        let tx = self.location_label_tx.clone();
        let log = self.dirs.log_path.clone();
        self.location_label_task = Some(tokio::spawn(async move {
            // A failed lookup simply leaves the pin unlabelled; it is a nicety,
            // not something worth surfacing as a layer error.
            let name = geocode.reverse(point, &log).await.ok().flatten();
            let _ = tx.send((point, name));
        }));
    }

    /// Drain resolved pin labels.  Returns true when the label changed.
    pub fn drain_pin_labels(&mut self) -> bool {
        let mut changed = false;
        while let Ok((point, name)) = self.location_label_rx.try_recv() {
            // Ignore a result for a position we have already moved on from.
            if self.location_label_at != Some(point) {
                continue;
            }
            if self.location_label != name {
                self.location_label = name;
                changed = true;
            }
        }
        changed
    }

    /// Re-fetch everything that depends on the viewport.  Used after any jump
    /// or pan, so a moved map does not keep showing the old area's data.
    pub fn request_viewport_refresh(&mut self) {
        self.request_meteogate_refresh(self.map_width, self.map_height);
        self.request_border_refresh();
        if self.any_obs_enabled() && !self.has_obs_task() {
            self.request_obs_refresh();
        }
    }

    // ── Place search (`/`) ─────────────────────────────────────────────

    /// Open the `/` prompt with an empty buffer.
    pub fn open_search(&mut self) {
        self.search_input = Some(String::new());
        self.search_status = None;
    }

    /// Close the prompt, discarding the buffer.  The pin itself is left alone
    /// — Esc dismisses the prompt, it does not undo a previous search.
    pub fn cancel_search(&mut self) {
        self.search_input = None;
        self.search_status = None;
    }

    pub fn search_is_open(&self) -> bool {
        self.search_input.is_some()
    }

    pub fn search_push_char(&mut self, c: char) {
        if let Some(buf) = self.search_input.as_mut() {
            buf.push(c);
        }
    }

    pub fn search_backspace(&mut self) {
        if let Some(buf) = self.search_input.as_mut() {
            buf.pop();
        }
    }

    /// Drop the pin and turn the layer off again.
    pub fn clear_search_pin(&mut self) {
        self.search_pin = None;
        self.search_label = None;
        self.layers.mode_state_mut().remove_all(LayerId::SearchPin);
        self.layers
            .set_status(LayerId::SearchPin, LayerStatus::Idle);
    }

    /// Submit the prompt's buffer as a geocoding query.
    ///
    /// The prompt closes immediately and the lookup runs in the background,
    /// so a slow Nominatim response never blocks the event loop.
    pub fn submit_search(&mut self) {
        let Some(query) = self.search_input.take() else {
            return;
        };
        let query = query.trim().to_string();
        if query.is_empty() {
            self.search_status = None;
            return;
        }

        // Abort any in-flight search: only the latest query matters.
        if let Some(task) = self.search_task.take() {
            task.abort();
        }
        self.search_id = self.search_id.wrapping_add(1);
        let id = self.search_id;

        self.search_status = Some(format!("Searching for \"{query}\"…"));
        self.layers
            .set_status(LayerId::SearchPin, LayerStatus::Loading);

        let geocode = self.geocode.clone();
        let tx = self.search_tx.clone();
        let log = self.dirs.log_path.clone();
        self.search_task = Some(tokio::spawn(async move {
            let outcome = geocode.search(&query, &log).await;
            let _ = tx.send(SearchResult {
                id,
                query,
                outcome: match outcome {
                    Ok(Some(place)) => SearchOutcome::Found(place),
                    Ok(None) => SearchOutcome::NoMatch,
                    Err(e) => SearchOutcome::Error(e.to_string()),
                },
            });
        }));
    }

    /// Drain finished searches, moving the pin and the viewport on a hit.
    pub fn drain_search_results(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.search_rx.try_recv() {
            // A superseded query — the user has already searched again.
            if result.id != self.search_id {
                continue;
            }
            changed = true;
            match result.outcome {
                SearchOutcome::Found(place) => {
                    self.search_pin = Some(place.point);
                    // The first component of Nominatim's display_name is the
                    // place itself, so the pin gets its label for free — no
                    // second request against the 1 req/s policy.
                    self.search_label = place
                        .display_name
                        .split(',')
                        .next()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    // A found place is only useful if you can see it, so jump
                    // there — this is an explicit user request, unlike a
                    // location fix arriving on its own.
                    self.viewport = Viewport::from_lat_lon(
                        place.point.lat,
                        place.point.lon,
                        self.viewport.zoom.max(SEARCH_MIN_ZOOM),
                    );
                    // Show the pin: the layer owns no mode until a hit.
                    let modes = self.layers.mode_state_mut();
                    if !modes.has_any(LayerId::SearchPin) {
                        modes.toggle_overlay(RenderMode::Text, LayerId::SearchPin);
                    }
                    self.layers
                        .set_status(LayerId::SearchPin, LayerStatus::Ready);
                    self.search_status = Some(place.display_name);
                    self.request_viewport_refresh();
                }
                SearchOutcome::NoMatch => {
                    self.search_status = Some(format!("No match for \"{}\"", result.query));
                    self.layers
                        .set_status(LayerId::SearchPin, LayerStatus::Idle);
                }
                SearchOutcome::Error(e) => {
                    self.search_status = Some(format!("Search failed: {e}"));
                    self.layers
                        .set_status(LayerId::SearchPin, LayerStatus::Error(e));
                }
            }
        }
        changed
    }

    // ── Settings modal ───────────────────────────────────────────────

    /// Open the settings modal, staged from the current config. No-op while
    /// the search prompt or help modal already owns the screen — one
    /// overlay at a time.
    pub fn open_settings(&mut self) {
        if self.search_is_open() || self.show_help {
            return;
        }
        self.settings = Some(SettingsState {
            model: SettingsModel::from_config(&self.config),
            editing: false,
            apply_error: None,
        });
        // Show the current key's status right away: verify it on open when one
        // is set (empty = anonymous, nothing to check).
        self.last_verify = None;
        let key = self.config.eumetnet.api_key.trim().to_string();
        if !key.is_empty() {
            self.request_verify(VerifyTarget::Eumetnet, key);
        }
    }

    pub fn settings_is_open(&self) -> bool {
        self.settings.is_some()
    }

    /// Index into `settings_links` whose rendered link contains screen cell
    /// `(col, row)`, if any — used for hover, press, and click hit-testing in
    /// the settings modal.
    pub fn settings_link_index_at(&self, col: u16, row: u16) -> Option<usize> {
        self.settings_links
            .iter()
            .position(|(x, y, w, _)| row == *y && col >= *x && col < x.saturating_add(*w))
    }

    /// Up/Down — move between fields, only while browsing (not mid-edit). The
    /// verify status persists across focus moves (it's cleared on edit, not
    /// navigation) so the key's green/red stays put while you browse.
    pub fn settings_focus_next(&mut self) {
        if let Some(s) = self.settings.as_mut().filter(|s| !s.editing) {
            s.model.focus_next();
        }
    }

    pub fn settings_focus_prev(&mut self) {
        if let Some(s) = self.settings.as_mut().filter(|s| !s.editing) {
            s.model.focus_prev();
        }
    }

    /// A printable key — edits the focused secret, only while editing it.
    pub fn settings_push_char(&mut self, c: char) {
        if let Some(s) = self.settings.as_mut().filter(|s| s.editing) {
            s.model.push_char(c);
            s.apply_error = None;
            self.last_verify = None;
        }
    }

    pub fn settings_backspace(&mut self) {
        if let Some(s) = self.settings.as_mut().filter(|s| s.editing) {
            s.model.backspace();
            s.apply_error = None;
            self.last_verify = None;
        }
    }

    /// Left/Right — flips the focused bool, only while editing it.
    pub fn settings_toggle_bool(&mut self) {
        if let Some(s) = self.settings.as_mut().filter(|s| s.editing) {
            s.model.toggle_bool();
            s.apply_error = None;
        }
    }

    /// Enter — start editing the focused field, or (already editing) save it:
    /// persist to `config.toml`, mirror into `self.config`, and for the
    /// eumetnet key rebuild the provider live and verify the new value
    /// automatically. A write-back failure is surfaced on `apply_error`.
    pub fn settings_confirm(&mut self) {
        let Some(editing) = self.settings.as_ref().map(|s| s.editing) else {
            return;
        };
        if !editing {
            if let Some(s) = self.settings.as_mut() {
                s.editing = true;
                s.apply_error = None;
            }
            return;
        }

        // Editing → save the focused field.
        let Some(edit) = self
            .settings
            .as_ref()
            .and_then(|s| s.model.focused_pending_edit())
        else {
            // No change — just leave edit mode.
            if let Some(s) = self.settings.as_mut() {
                s.editing = false;
            }
            return;
        };

        let path = self.dirs.config_dir.join("config.toml");
        if let Err(e) = apply_config_edits(&path, std::slice::from_ref(&edit)) {
            write_log(
                &self.dirs.log_path,
                format!("settings: apply_config_edits failed: {e}"),
            );
            if let Some(s) = self.settings.as_mut() {
                s.apply_error = Some("save failed — see log".to_string());
                s.editing = false;
            }
            return;
        }

        if let Some(s) = self.settings.as_mut() {
            s.model.commit_focused();
            s.editing = false;
            s.apply_error = None;
        }

        match (edit.key.as_str(), &edit.value) {
            ("eumetnet.api_key", ConfigEditValue::Str(v)) => {
                self.config.eumetnet.api_key = v.clone();
                self.rebuild_eumetnet_provider();
                // Verify the new key automatically, unless it was cleared —
                // an empty key is valid anonymous access, nothing to check.
                if !v.trim().is_empty() {
                    self.request_verify(VerifyTarget::Eumetnet, v.clone());
                }
            }
            ("location.ip_fallback", ConfigEditValue::Bool(b)) => {
                self.config.location.ip_fallback = *b;
            }
            _ => {}
        }
    }

    /// Esc — cancel the edit in progress (revert the focused field), or close
    /// the modal when not editing.
    pub fn settings_back(&mut self) {
        let Some(editing) = self.settings.as_ref().map(|s| s.editing) else {
            return;
        };
        if editing {
            if let Some(s) = self.settings.as_mut() {
                s.model.revert_focused();
                s.editing = false;
                s.apply_error = None;
            }
            self.last_verify = None;
        } else {
            self.settings = None;
            self.last_verify = None;
        }
    }

    /// Drain fixes from the location backends.
    ///
    /// Returns true only when the winning fix actually changed — losing fixes
    /// (a coarse IP result arriving after GPS) are discarded without forcing a
    /// redraw.  The viewport is never touched here; see `initial_viewport` for
    /// why only the first fix re-centres.
    pub fn drain_location_updates(&mut self) -> bool {
        let Some(rx) = self.location_rx.as_mut() else {
            return false;
        };
        let mut fixes = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(fix) => fixes.push(fix),
                // All backends exited; stop polling a dead channel.
                Err(TryRecvError::Disconnected) => {
                    self.location_rx = None;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }

        let mut changed = false;
        for fix in fixes {
            if self.location.offer(fix) {
                changed = true;
                write_log(
                    &self.dirs.log_path,
                    format!("location: fix {}", fix.label()),
                );
                self.layers
                    .set_status(LayerId::Location, LayerStatus::Ready);
                self.request_location_label(fix.point);
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
                        task.fraction = Some(0.0);
                        task.anim_from = task.display_fraction;
                        task.anim_t = 0.0;
                        task.state = TaskState::Running;
                        task.completed_at = None;
                        task.last_anim = now;
                        // A reused row is a new task — reset its start time
                        // so the visibility threshold gates on this run, not
                        // whatever the prior occupant had going.
                        task.started_at = now;
                    } else {
                        self.active_tasks.push(ActiveTask {
                            id,
                            label,
                            action: String::new(),
                            fraction: Some(0.0),
                            display_fraction: 0.0,
                            anim_from: 0.0,
                            anim_t: 0.0,
                            kind,
                            state: TaskState::Running,
                            started_at: now,
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
                        task.apply_progress(action, fraction);
                    }
                }
                TaskMsg::Complete { id } => {
                    if let Some(task) = self.active_tasks.iter_mut().find(|t| t.id == id) {
                        task.apply_complete(Instant::now());
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
            // Indeterminate tasks have no fraction to ease toward — the
            // marquee derives its position from wall-clock time at render
            // instead, so there is nothing for smoothstep to animate here.
            let Some(target) = task.fraction else {
                continue;
            };
            if task.anim_t < 1.0 {
                task.anim_t = (task.anim_t + dt * 4.0).min(1.0); // 0.25 s max
                let t = task.anim_t;
                let eased = t * t * (3.0 - 2.0 * t); // smoothstep
                task.display_fraction = task.anim_from + (target - task.anim_from) * eased;
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
        if let Some(task) = self.radar_preload_task.take() {
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
        self.abort_lightning();
    }

    /// Spawn the background Blitzortung WebSocket task.  No-op when the
    /// `lightning` feature is disabled.
    pub fn request_lightning_connect(&mut self) {
        self.abort_lightning();
        let cancel = Arc::new(AtomicBool::new(false));
        self.lightning_cancel = Some(cancel.clone());
        self.layers
            .set_status(LayerId::Lightning, LayerStatus::Loading);
        let (close_tx, close_rx) = tokio::sync::oneshot::channel::<()>();
        self.lightning_close_tx = Some(close_tx);
        #[cfg(feature = "lightning")]
        {
            let tx = self.lightning_tx.clone();
            let log = self.dirs.log_path.clone();
            self.lightning_task = Some(tokio::spawn(async move {
                crate::providers::lightning::connect_and_stream(tx, log, cancel, close_rx).await;
            }));
        }
        #[cfg(not(feature = "lightning"))]
        drop(close_rx);
        write_log(&self.dirs.log_path, "lightning: connection requested");
    }

    /// Signal the WS task to send a Close frame and exit, then abort as
    /// fallback.  Resets the layer status to Idle.
    pub fn abort_lightning(&mut self) {
        // Send WS Close signal first so the server receives a proper goodbye.
        if let Some(tx) = self.lightning_close_tx.take() {
            let _ = tx.send(());
        }
        if let Some(cancel) = self.lightning_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(task) = self.lightning_task.take() {
            task.abort();
        }
        self.layers
            .set_status(LayerId::Lightning, LayerStatus::Idle);
    }

    /// Drain `(position, polarity)` pairs from the background task channel,
    /// prune expired strikes, and return `true` when the set changed.
    pub fn drain_lightning_results(&mut self) -> bool {
        let now = std::time::Instant::now();
        let trail_dur =
            std::time::Duration::from_secs(u64::from(self.layers.lightning_trail_minutes) * 60);
        let impact_dur = std::time::Duration::from_millis(u64::from(LIGHTNING_IMPACT_MS));

        let mut changed = false;
        while let Ok((point, pol)) = self.lightning_rx.try_recv() {
            // x < 0 is the CONNECTED_SENTINEL — marks a successful handshake.
            if point.x < 0.0 {
                self.layers
                    .set_status(LayerId::Lightning, LayerStatus::Ready);
                changed = true;
                continue;
            }
            self.lightning_strikes.push((point, now, pol));
            changed = true;
        }

        // Prune strikes beyond the trail window.
        let before = self.lightning_strikes.len();
        self.lightning_strikes
            .retain(|(_, t, _)| now.duration_since(*t) < trail_dur);
        changed |= self.lightning_strikes.len() < before;

        // Cap to prevent unbounded growth during heavy storm activity.
        const MAX_STRIKES: usize = 5_000;
        if self.lightning_strikes.len() > MAX_STRIKES {
            let excess = self.lightning_strikes.len() - MAX_STRIKES;
            self.lightning_strikes.drain(..excess);
            changed = true;
        }

        // Keep animating as long as any strike is still in impact phase.
        if !changed {
            changed = self
                .lightning_strikes
                .iter()
                .any(|(_, t, _)| now.duration_since(*t) < impact_dur);
        }

        changed
    }

    /// True when at least one strike is in its impact animation window.
    pub fn has_lightning_impact(&self) -> bool {
        let impact_dur = std::time::Duration::from_millis(u64::from(LIGHTNING_IMPACT_MS));
        let now = std::time::Instant::now();
        self.lightning_strikes
            .iter()
            .any(|(_, t, _)| now.duration_since(*t) < impact_dur)
    }

    pub fn drain_obs_results(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.obs_rx.try_recv() {
            if result.id != self.obs_refresh_id {
                continue;
            }
            match result.result {
                ObsRefreshPayload::Point(point) => {
                    // The accumulator is reset once per refresh at kickoff
                    // (`request_obs_refresh`), so by the time any `Point` for
                    // this refresh id reaches here `obs_incoming_id` already
                    // matches — no lazy reset needed.
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

    /// Step one frame toward older, wrapping past the oldest to the newest.
    ///
    /// Both step directions wrap around the current history window, over the
    /// same seam [`Self::playback_step`] crosses — stepping and playing
    /// traverse the range identically, and neither dead-ends at an edge.
    pub fn next_frame(&mut self) {
        if let Some(i) = stepped_index(self.frame_index, self.timestamps.len(), Step::Older) {
            self.frame_index = i;
            self.playback_mode = self.mode_for_index();
        }
    }

    /// Step one frame toward newer, wrapping past the newest to the oldest.
    pub fn previous_frame(&mut self) {
        if let Some(i) = stepped_index(self.frame_index, self.timestamps.len(), Step::Newer) {
            self.frame_index = i;
            self.playback_mode = self.mode_for_index();
        }
    }

    /// Landing on the newest frame re-enters live mode automatically, however
    /// the playhead got there — including by wrapping.
    fn mode_for_index(&self) -> PlaybackMode {
        if self.frame_index == 0 {
            PlaybackMode::Live
        } else {
            PlaybackMode::Paused
        }
    }

    /// Return to Live mode and snap to the newest frame.
    pub fn jump_to_live(&mut self) {
        self.playback_mode = PlaybackMode::Live;
        self.frame_index = 0;
    }

    /// Toggle between Playing and Paused; Live transitions to Playing.
    /// Stopping playback while on the newest frame re-enters live mode.
    pub fn toggle_play_pause(&mut self) {
        self.playback_mode = match self.playback_mode {
            PlaybackMode::Live | PlaybackMode::Paused => PlaybackMode::Playing,
            PlaybackMode::Playing => {
                if self.frame_index == 0 {
                    PlaybackMode::Live
                } else {
                    PlaybackMode::Paused
                }
            }
        };
    }

    /// Advance one frame toward newer (decrement index toward 0), wrapping
    /// from newest back to oldest so the loop repeats.  Only called by the
    /// playback tick — does not change `playback_mode`.
    pub fn playback_step(&mut self) {
        if self.timestamps.is_empty() {
            return;
        }
        if self.frame_index == 0 {
            self.frame_index = self.timestamps.len() - 1;
        } else {
            self.frame_index -= 1;
        }
    }

    pub fn speed_faster(&mut self) {
        self.playback_speed = self.playback_speed.faster();
    }

    pub fn speed_slower(&mut self) {
        self.playback_speed = self.playback_speed.slower();
    }

    /// Advance to the next history depth (3 → 6 → 12 → 24 → 3 h) and rebuild
    /// the timeline in place.
    ///
    /// The list is recomputed locally rather than refetched: the slot times are
    /// pure arithmetic, and any frame already on disk stays cached, so
    /// deepening the window costs nothing until those frames are actually
    /// loaded.  The viewer keeps its current timestamp when the shorter window
    /// still contains it, so cycling 24 → 3 h while parked on an old frame
    /// lands on the nearest slot still in range rather than jumping to live.
    pub fn cycle_history(&mut self) {
        self.history_hours = crate::providers::meteogate::next_history_hours(self.history_hours);
        let current_ts = self.timestamps.get(self.frame_index).copied();
        self.timestamps = crate::providers::meteogate::compute_frame_list(self.history_hours);

        self.frame_index = if self.playback_mode == PlaybackMode::Live {
            0
        } else {
            match current_ts.and_then(|ts| self.timestamps.iter().position(|&t| t == ts)) {
                Some(i) => i,
                // The frame we were on fell outside the new window; park on the
                // oldest slot that survives so the view stays as close in time
                // as the window allows.
                None => self.timestamps.len().saturating_sub(1),
            }
        };
        // Frames outside the new window are dead weight in memory; the on-disk
        // GeoTIFFs remain, so re-widening reloads them without touching S3.
        let keep: HashSet<i64> = self.timestamps.iter().copied().collect();
        self.frame_cache.retain(|ts, _| keep.contains(ts));
        // A deeper window exposes older slots that earlier sessions may already
        // have fetched; rescan so they show as available immediately.
        self.disk_frames = self.meteogate.cached_timestamps();
        write_log(
            &self.dirs.log_path,
            format!(
                "history: {} h ({} slots)",
                self.history_hours,
                self.timestamps.len()
            ),
        );
    }

    /// Spawn a background task that loads uncached radar frames near the
    /// playhead at the current viewport, in parallel.  Uses a small semaphore
    /// (2) so preload doesn't steal HTTP/CPU resources from higher-priority
    /// work (borders, current frame, observations).  Aborts any existing
    /// preload.
    pub fn trigger_radar_preload(&mut self) {
        // Never preload mid-drag.  `request_meteogate_refresh` runs on
        // every mouse-move tick and, while the current frame still covers
        // the viewport, reaches this call each time.  Aborting and
        // relaunching the 11-frame preload pipeline every tick spawns
        // GeoTIFF decodes that `spawn_blocking` cannot cancel, so they
        // pile up on the blocking pool and make dragging progressively
        // slower.  Preload resumes once the drag ends.
        if self.is_dragging {
            return;
        }
        if let Some(task) = self.radar_preload_task.take() {
            task.abort();
        }
        if self.timestamps.is_empty() || !self.layers.enabled(LayerId::Radar) {
            return;
        }
        let zoom = self.viewport.zoom;
        let tile_zoom = zoom.round().clamp(1.0, 7.0) as u8;
        let bounds = self.viewport.bounds(self.map_width, self.map_height);
        let fetch_bounds = bounds.expanded(0.5);
        let current_ts = self.timestamps.get(self.frame_index).copied();
        // Preload a window around the playhead rather than the whole timeline.
        // At 24 h that would be 288 slots, and every zoom change clears
        // `frame_cache` and re-decodes them: the GeoTIFFs come off disk, but
        // each still costs a ~140 ms parse, so a full sweep would burn a minute
        // of CPU per zoom.  The window re-centres as the playhead moves, so
        // playback stays fed while the cost per trigger stays bounded.
        //
        // Distance is measured around the ring, so the window spans the wrap
        // instead of stopping dead at either end: sitting on the newest frame
        // preloads the oldest ones too, which is where the loop lands next.
        let len = self.timestamps.len();
        let now = Instant::now();
        let mut by_distance: Vec<(usize, i64)> = self
            .timestamps
            .iter()
            .enumerate()
            .filter(|(_, ts)| !self.frame_cache.contains_key(ts) && Some(**ts) != current_ts)
            // A slot that just failed is held back until its backoff expires,
            // so a genuinely unavailable frame doesn't get hammered on every
            // pan while a transient one still recovers quickly.
            .filter(|(_, ts)| {
                self.radar_failures.get(ts).is_none_or(|retry| {
                    now >= retry.next_at && !FRAME_RETRY_POLICY.exhausted(retry.attempts)
                })
            })
            .map(|(i, &ts)| (ring_distance(i, self.frame_index, len), ts))
            .collect();
        // Nearest the playhead first: those are the frames playback reaches
        // soonest, and the truncation drops the most distant.
        by_distance.sort_by_key(|&(d, _)| d);
        by_distance.truncate(PRELOAD_WINDOW);
        let to_load: Vec<i64> = by_distance.into_iter().map(|(_, ts)| ts).collect();
        if to_load.is_empty() {
            return;
        }
        // Push the retry clock forward for every failed slot we are about to
        // request.  `retry_due_frames` runs each tick, so without this a slot
        // would still read as "due" while its fetch was in flight and every
        // tick would abort and relaunch the pass — the frame would never land.
        for ts in &to_load {
            if let Some(retry) = self.radar_failures.get_mut(ts) {
                retry.next_at = Instant::now() + FRAME_RETRY_POLICY.delay_for(retry.attempts);
            }
        }
        let provider = self.meteogate.clone();
        let tx = self.radar_preload_tx.clone();
        self.radar_preload_task = Some(tokio::spawn(async move {
            const MAX_CONCURRENT_PRELOADS: usize = 2;
            let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_PRELOADS));
            let mut futs: FuturesUnordered<_> = FuturesUnordered::new();
            for ts in to_load {
                let provider = provider.clone();
                let tx = tx.clone();
                let sem = Arc::clone(&sem);
                futs.push(async move {
                    let _permit = sem.acquire().await;
                    // Failures are reported too, so the slot can be retried
                    // instead of silently staying blank forever.
                    let frame = provider.frame(ts, fetch_bounds, zoom).await.ok();
                    let _ = tx.send(RadarPreloadResult {
                        timestamp: ts,
                        tile_zoom,
                        frame,
                    });
                });
            }
            while futs.next().await.is_some() {}
        }));
    }

    /// Drain completed preload results into `frame_cache`.
    pub fn drain_preload_results(&mut self) -> bool {
        let mut changed = false;
        let current_zoom = self.viewport.zoom.round().clamp(1.0, 7.0) as u8;
        while let Ok(result) = self.radar_preload_rx.try_recv() {
            if result.tile_zoom != current_zoom {
                continue;
            }
            if !self.timestamps.contains(&result.timestamp) {
                continue;
            }
            let Some(frame) = result.frame else {
                self.note_frame_failure(result.timestamp);
                continue;
            };
            // The provider resolves a requested slot to whatever is actually
            // published, so key the cache by the time the frame really holds.
            // Caching under the requested slot instead would show the same
            // data on two timeline positions and mark both as loaded.
            let actual_ts = frame.time;
            if actual_ts != result.timestamp {
                changed |= self.drop_phantom_slot(result.timestamp, actual_ts);
                if !self.timestamps.contains(&actual_ts) {
                    continue;
                }
            }
            // Loading it wrote the GeoTIFF to disk, so it stays reloadable
            // after `prune_frame_cache` drops it from RAM.
            self.disk_frames.insert(actual_ts);
            self.frame_cache.insert(actual_ts, frame);
            // Both the requested and the resolved slot are settled now.
            self.radar_failures.remove(&result.timestamp);
            self.radar_failures.remove(&actual_ts);
            changed = true;
        }
        self.prune_frame_cache();
        changed
    }

    /// Record a failed slot and schedule the next attempt.
    ///
    /// The backoff grows per attempt but is capped, so retries continue for as
    /// long as the slot is on the timeline: a frame missing because of an
    /// outage fills itself in once the outage ends, with no user action.
    fn note_frame_failure(&mut self, ts: i64) {
        let entry = self.radar_failures.entry(ts).or_insert(FrameRetry {
            attempts: 0,
            next_at: Instant::now(),
        });
        entry.attempts = entry.attempts.saturating_add(1);
        let backoff = FRAME_RETRY_POLICY.delay_for(entry.attempts);
        entry.next_at = Instant::now() + backoff;
        let msg = if entry.attempts >= FRAME_RETRY_GIVE_UP {
            format!(
                "radar: frame {ts} failed (attempt {}) — giving up this session",
                entry.attempts
            )
        } else {
            format!(
                "radar: frame {ts} failed (attempt {}) — retrying in {backoff:?}",
                entry.attempts
            )
        };
        write_log(&self.dirs.log_path, msg);
    }

    /// Re-launch a preload pass when a failed slot has become due for another
    /// attempt.  Called every tick; cheap when nothing is pending.
    ///
    /// Without this, a failed slot would only be retried if some *other* event
    /// happened to trigger a preload, so a frame that failed while the user sat
    /// still would stay missing indefinitely.
    pub fn retry_due_frames(&mut self) {
        if self.radar_failures.is_empty() || self.is_dragging {
            return;
        }
        // Forget slots that have scrolled off the timeline entirely.
        let live: HashSet<i64> = self.timestamps.iter().copied().collect();
        self.radar_failures.retain(|ts, _| live.contains(ts));

        let now = Instant::now();

        // The frame on screen is not part of a preload pass, so it needs its
        // own re-request — and it is the one the user is actually waiting for.
        let current_ts = self.timestamps.get(self.frame_index).copied();
        if let Some(ts) = current_ts {
            let due = self.radar_failures.get(&ts).is_some_and(|retry| {
                now >= retry.next_at && !FRAME_RETRY_POLICY.exhausted(retry.attempts)
            });
            if due && self.refresh_task.is_none() && !self.frame_cache.contains_key(&ts) {
                if let Some(retry) = self.radar_failures.get_mut(&ts) {
                    // Same in-flight guard as the preload path.
                    retry.next_at = now + FRAME_RETRY_POLICY.delay_for(retry.attempts);
                }
                self.request_meteogate_refresh(self.map_width, self.map_height);
                return;
            }
        }

        let due = self.radar_failures.iter().any(|(ts, retry)| {
            now >= retry.next_at
                && !FRAME_RETRY_POLICY.exhausted(retry.attempts)
                && !self.frame_cache.contains_key(ts)
        });
        if due {
            self.trigger_radar_preload();
        }
    }

    /// How readily `ts` can be displayed: decoded in RAM, on disk needing only
    /// a decode, or absent and needing a fetch.
    pub fn slot_state(&self, ts: i64) -> crate::ui::SlotState {
        if self.frame_cache.contains_key(&ts) {
            crate::ui::SlotState::InRam
        } else if self.disk_frames.contains(&ts) {
            crate::ui::SlotState::OnDisk
        } else {
            crate::ui::SlotState::Missing
        }
    }

    /// Evict cached frames furthest from the playhead once the cache exceeds
    /// [`FRAME_CACHE_MAX`].
    fn prune_frame_cache(&mut self) {
        for ts in frames_to_evict(
            &self.frame_cache.keys().copied().collect::<Vec<_>>(),
            &self.timestamps,
            self.frame_index,
            FRAME_CACHE_MAX,
        ) {
            self.frame_cache.remove(&ts);
        }
    }

    /// Drop a requested slot that the provider resolved to a different time.
    ///
    /// Such a slot is not actually published, so leaving it on the timeline
    /// would render its neighbour's data a second time and report it as
    /// loaded.  Returns `true` when the timeline changed.
    fn drop_phantom_slot(&mut self, req_ts: i64, resolved_ts: i64) -> bool {
        let Some((timestamps, frame_index)) = timeline_without_phantom(
            &self.timestamps,
            self.frame_index,
            req_ts,
            resolved_ts,
            self.playback_mode == PlaybackMode::Live,
        ) else {
            return false;
        };
        self.timestamps = timestamps;
        self.frame_index = frame_index;
        self.frame_cache.remove(&req_ts);
        write_log(
            &self.dirs.log_path,
            format!("meteogate: removed phantom slot {req_ts} (resolved to {resolved_ts})"),
        );
        true
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
        // Overlays are saved with the same mode tag as the primary slot;
        // load_state routes them back by layer identity via `overlay_modes`.
        for &(mode, layer) in &modes.overlays {
            render_modes.push(LayerRenderMode { layer, mode });
        }
        let state = StateConfig {
            center_lat: center.lat,
            center_lon: center.lon,
            zoom: self.viewport.zoom,
            enabled_layers: self.layers.saved_enabled(),
            known_layers: self.layers.known_layers(),
            selected_layer: self.layers.selected_layer(),
            render_modes,
            braille_layer: None,
            color_layer: None,
            text_layer: None,
            lightning_trail_minutes: Some(self.layers.lightning_trail_minutes),
            history_hours: Some(self.history_hours),
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
        if let Some(minutes) = state.lightning_trail_minutes {
            self.layers.lightning_trail_minutes = minutes.clamp(1, 30);
        }
        // Ignore a depth that isn't one of the offered options, so a stale or
        // hand-edited state.toml can't wedge the `i` cycle on a value it would
        // never produce.
        if let Some(hours) = state.history_hours {
            if crate::providers::meteogate::HISTORY_OPTIONS.contains(&hours) {
                self.history_hours = hours;
            }
        }
        self.layers
            .restore_enabled(&state.enabled_layers, &state.known_layers);
        self.layers.set_selected(state.selected_layer);
        let known = LayerRegistry::known_from_state(&state.known_layers);
        let modes = self.layers.mode_state_mut();
        if !state.render_modes.is_empty() {
            // The saved file is authoritative for every layer it knew about,
            // so clear those first: without this, a mode the user switched off
            // is silently re-enabled by the constructor default on next boot.
            // Layers the file never knew keep their default — that is what
            // stops a newly added layer from booting up disabled.
            for id in known {
                modes.remove_all(id);
            }
            // Explicit (layer, mode) pairs.  Overlay layers (Lightning
            // braille, Location text/background) are stored with the same mode
            // tag but must go back to the overlay slot, or they would evict
            // the primary owner on load.
            for entry in &state.render_modes {
                modes.restore(entry.mode, entry.layer);
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

/// Which provider a verify probe targeted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyTarget {
    Eumetnet,
}

/// The settings modal's in-progress editing session, layered over the pure
/// [`SettingsModel`]: whether the focused field is being actively edited, and
/// the last apply failure to surface instead of crashing.
#[derive(Debug)]
pub struct SettingsState {
    pub model: SettingsModel,
    /// `false` = browsing the field list; `true` = editing the focused field
    /// (typing into a secret, or toggling a bool) before Enter saves it.
    pub editing: bool,
    /// Set when `apply_config_edits` fails (malformed `config.toml`); shown
    /// in the modal instead of crashing. Cleared on the next edit or open.
    pub apply_error: Option<String>,
}

#[derive(Debug)]
struct VerifyResult {
    id: u64,
    target: VerifyTarget,
    outcome: VerifyOutcome,
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

/// Zoom to at least this when jumping to a search hit, so a result found while
/// looking at the whole continent is actually visible.
const SEARCH_MIN_ZOOM: f64 = 7.0;

/// A finished place search, tagged with the query that produced it.
#[derive(Debug)]
struct SearchResult {
    id: u64,
    query: String,
    outcome: SearchOutcome,
}

#[derive(Debug)]
enum SearchOutcome {
    Found(Place),
    /// The search ran but matched nothing — a typo, not a failure.
    NoMatch,
    Error(String),
}

/// How long boot waits for the first fix before falling back to the Europe
/// view.  Backends keep running past this deadline — a slow fix still lands on
/// the map, it just does not get to hold up the UI.
const INITIAL_FIX_TIMEOUT: Duration = Duration::from_secs(2);

/// Resolve the starting viewport and kick off location tracking.
///
/// `--lat/--lon` short-circuits everything: it produces a `Manual` fix and no
/// backend is started, so nothing can move the marker afterwards.
async fn initial_viewport(
    cli: &Cli,
    config: &Config,
    log_path: &Path,
) -> (
    Viewport,
    LocationArbiter,
    Option<UnboundedReceiver<LocationFix>>,
) {
    let mut arbiter = LocationArbiter::new();

    if let (Some(lat), Some(lon)) = (cli.lat, cli.lon) {
        arbiter.offer(LocationFix::new(
            GeoPoint::new(lon, lat),
            None,
            LocationSource::Manual,
        ));
        return (
            Viewport::from_lat_lon(lat, lon, cli.zoom.unwrap_or(5.0)),
            arbiter,
            None,
        );
    }

    let europe = Viewport::from_lat_lon(
        crate::geo::EUROPE_LAT,
        crate::geo::EUROPE_LON,
        cli.zoom.unwrap_or(crate::geo::EUROPE_ZOOM),
    );

    if cli.no_location {
        return (europe, arbiter, None);
    }

    let mut stream = crate::providers::location::spawn(&config.location, log_path);

    // Only the first fix is allowed to move the viewport: once the map is up
    // the user may have panned, and yanking the view out from under them
    // because the GPS sharpened by 30 m would be hostile.  Later fixes move
    // the marker only.
    let first = tokio::time::timeout(INITIAL_FIX_TIMEOUT, stream.rx.recv()).await;
    let viewport = match first {
        Ok(Some(fix)) => {
            write_log(log_path, format!("boot: initial fix from {}", fix.label()));
            arbiter.offer(fix);
            viewport_for_fix(&fix, cli.zoom)
        }
        Ok(None) => {
            // Every backend gave up (no GeoClue daemon, IP fallback off).
            write_log(log_path, "boot: no location backend available");
            europe
        }
        Err(_) => {
            write_log(
                log_path,
                "boot: no fix within timeout, starting at Europe view",
            );
            europe
        }
    };

    (viewport, arbiter, Some(stream.rx))
}

/// Where the first fix centres the viewport, regardless of accuracy.
///
/// This is deliberately independent of [`LocationFix::is_displayable`]: a
/// 10 km GeoIP fix is still far better than the Europe fallback for choosing
/// where to start, even though it is too coarse to draw a marker for. The
/// accuracy gate governs the "you are here" dot only, never where the map
/// boots.
fn viewport_for_fix(fix: &LocationFix, cli_zoom: Option<f64>) -> Viewport {
    Viewport::from_lat_lon(fix.point.lat, fix.point.lon, cli_zoom.unwrap_or(5.0))
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
    Verify,
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
            Self::Verify => ratatui::style::Color::White,
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
            Self::Verify => "verify",
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
        /// `None` means "working, no measurable progress" (e.g. a geocode or
        /// location fix) — rendered as a marquee, never a faked value.
        fraction: Option<f64>,
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
    /// `None` = indeterminate (marquee); `Some(f)` = determinate progress.
    pub fraction: Option<f64>,
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
    /// When this task started running. Used to gate visibility (CP-4): a
    /// task that starts and finishes inside `TASK_VISIBLE_AFTER` must never
    /// render, so this is distinct from `last_anim` (bumped every tick) and
    /// `completed_at` (terminal-only).
    pub started_at: Instant,
    pub completed_at: Option<Instant>,
    pub last_anim: Instant,
}

/// A task must run for at least this long before it renders a row, so a
/// fast task never flashes into view and vanishes. `Error` is exempt — a
/// failure is always worth showing regardless of how fast it arrived.
pub const TASK_VISIBLE_AFTER: Duration = Duration::from_millis(150);

impl ActiveTask {
    /// Apply a `TaskMsg::Progress` update, handling every `Option`
    /// transition explicitly rather than diffing raw floats:
    /// - `Some → Some`: today's diff-check — animate only on a real change.
    /// - `None → Some`: entering determinate — reset the animation so the
    ///   bar eases in from wherever the marquee left `display_fraction`.
    /// - `* → None`: entering indeterminate. The marquee animates off
    ///   wall-clock at render time, not smoothstep, so no reset is needed.
    fn apply_progress(&mut self, action: String, fraction: Option<f64>) {
        self.action = action;
        match (self.fraction, fraction) {
            (Some(old), Some(new)) if (new - old).abs() > 0.001 => {
                self.anim_from = self.display_fraction;
                self.anim_t = 0.0;
            }
            (None, Some(_)) => {
                self.anim_from = self.display_fraction;
                self.anim_t = 0.0;
            }
            _ => {}
        }
        self.fraction = fraction;
    }

    /// Apply a `TaskMsg::Complete` update. Always finishes as a full
    /// determinate bar — even a task that was indeterminate animates to
    /// 100% and prunes via the normal `display_fraction < 0.999` check,
    /// never lingering as an unprunable marquee.
    fn apply_complete(&mut self, now: Instant) {
        self.anim_from = self.display_fraction;
        self.anim_t = 0.0;
        self.fraction = Some(1.0);
        self.state = TaskState::Completed;
        self.completed_at = Some(now);
    }

    /// Whether this task has earned a row in the overlay yet (CP-4).
    ///
    /// `Error` is always visible. A still-`Running` task gates on wall-clock
    /// age (`now - started_at`) since it has no end yet. A terminal task
    /// (`Completed`/`Superseded`) gates on how long it actually *ran*
    /// (`completed_at - started_at`), not on wall-clock age — gating on age
    /// would wrongly reveal a fast task partway through its post-complete
    /// linger window, ~150ms after it started.
    pub fn is_visible(&self, now: Instant) -> bool {
        match self.state {
            TaskState::Error => true,
            TaskState::Running => now.duration_since(self.started_at) >= TASK_VISIBLE_AFTER,
            TaskState::Completed | TaskState::Superseded => self
                .completed_at
                .is_some_and(|c| c.duration_since(self.started_at) >= TASK_VISIBLE_AFTER),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// No cheap way to drive `App::drain_task_messages` end-to-end (`App`
    /// has no lightweight test constructor — dozens of provider/config
    /// fields). `ActiveTask` is a plain `pub`-field struct though, so these
    /// tests drive the extracted `apply_progress` / `apply_complete`
    /// transition methods directly at the field level.
    fn test_task(fraction: Option<f64>) -> ActiveTask {
        ActiveTask {
            id: 1,
            label: "test".into(),
            action: String::new(),
            fraction,
            display_fraction: 0.4,
            anim_from: 0.4,
            anim_t: 1.0, // animation already settled
            kind: TaskKind::RadarFrame,
            state: TaskState::Running,
            started_at: Instant::now(),
            completed_at: None,
            last_anim: Instant::now(),
        }
    }

    #[test]
    fn progress_some_to_some_below_threshold_leaves_animation_untouched() {
        let mut task = test_task(Some(0.5));
        task.apply_progress("still going".into(), Some(0.5005));
        assert_eq!(task.fraction, Some(0.5005));
        assert_eq!(task.anim_t, 1.0, "sub-threshold change must not reset anim");
        assert_eq!(task.anim_from, 0.4);
    }

    #[test]
    fn progress_some_to_some_above_threshold_resets_animation() {
        let mut task = test_task(Some(0.5));
        task.apply_progress("jumped".into(), Some(0.7));
        assert_eq!(task.fraction, Some(0.7));
        assert_eq!(task.anim_t, 0.0, "real change must reset anim_t");
        assert_eq!(
            task.anim_from, 0.4,
            "anim_from must snapshot display_fraction"
        );
    }

    #[test]
    fn progress_none_to_some_resets_animation() {
        // Entering determinate from indeterminate must animate in, not jump.
        let mut task = test_task(None);
        task.apply_progress("found a fraction".into(), Some(0.2));
        assert_eq!(task.fraction, Some(0.2));
        assert_eq!(task.anim_t, 0.0, "None→Some must reset anim_t");
        assert_eq!(task.anim_from, 0.4);
    }

    #[test]
    fn progress_to_none_sets_indeterminate_without_animation_reset() {
        // Entering indeterminate: the marquee runs off wall-clock, not
        // smoothstep, so no anim reset is needed or expected.
        let mut task = test_task(Some(0.5));
        task.apply_progress("no measurable progress".into(), None);
        assert_eq!(task.fraction, None);
        assert_eq!(
            task.anim_t, 1.0,
            "Some→None must not touch the smoothstep state"
        );
    }

    #[test]
    fn complete_always_sets_full_determinate_bar() {
        // Falsified by reverting `apply_complete` to leave `fraction`
        // untouched: a task that was `None` would then never reach
        // `Some(1.0)`, fail the `< 0.999` prune check's implicit
        // determinate assumption, and the percent column would keep
        // printing blank on a "completed" row.
        let mut task = test_task(None);
        let now = Instant::now();
        task.apply_complete(now);
        assert_eq!(task.fraction, Some(1.0));
        assert_eq!(task.state, TaskState::Completed);
        assert_eq!(task.completed_at, Some(now));
        assert_eq!(
            task.anim_t, 0.0,
            "completion must restart the fill animation"
        );
        assert_eq!(task.anim_from, 0.4);
    }

    #[test]
    fn fast_completed_task_never_becomes_visible() {
        // Started and finished inside the threshold: must never render, even
        // though "now" below is well past the threshold (mid post-complete
        // linger) — gating on run duration, not wall-clock age, is the point.
        let start = Instant::now();
        let mut task = test_task(Some(1.0));
        task.state = TaskState::Completed;
        task.started_at = start;
        task.completed_at = Some(start + Duration::from_millis(50));
        let now = start + Duration::from_millis(200);
        assert!(!task.is_visible(now));
    }

    #[test]
    fn slow_completed_task_becomes_visible() {
        let start = Instant::now();
        let mut task = test_task(Some(1.0));
        task.state = TaskState::Completed;
        task.started_at = start;
        task.completed_at = Some(start + Duration::from_millis(200));
        assert!(task.is_visible(start + Duration::from_millis(200)));
    }

    #[test]
    fn fast_error_task_is_always_visible() {
        // Error is exempt from the threshold entirely.
        let start = Instant::now();
        let mut task = test_task(Some(1.0));
        task.state = TaskState::Error;
        task.started_at = start;
        task.completed_at = Some(start + Duration::from_millis(50));
        assert!(task.is_visible(start + Duration::from_millis(50)));
    }

    #[test]
    fn running_task_crosses_the_threshold_at_150ms() {
        let start = Instant::now();
        let mut task = test_task(Some(0.5));
        task.state = TaskState::Running;
        task.started_at = start;
        assert!(!task.is_visible(start + Duration::from_millis(50)));
        assert!(task.is_visible(start + Duration::from_millis(200)));
    }

    #[test]
    fn frame_cache_eviction_keeps_the_window_around_the_playhead() {
        // A full 24 h timeline with every slot cached.
        let timestamps: Vec<i64> = (0..288).map(|i| 1_000_000 - i * 300).collect();
        let cached = timestamps.clone();
        let playhead = 100;
        let evicted = frames_to_evict(&cached, &timestamps, playhead, 48);

        assert_eq!(evicted.len(), 288 - 48, "cache must be brought down to cap");
        let kept: Vec<i64> = cached
            .iter()
            .copied()
            .filter(|ts| !evicted.contains(ts))
            .collect();
        assert_eq!(kept.len(), 48);
        // The displayed frame must survive — evicting it would blank the map.
        assert!(kept.contains(&timestamps[playhead]));
        // What survives is contiguous around the playhead, not scattered.
        let kept_idx: Vec<usize> = kept
            .iter()
            .map(|ts| timestamps.iter().position(|t| t == ts).unwrap())
            .collect();
        let far = kept_idx.iter().map(|i| i.abs_diff(playhead)).max().unwrap();
        assert!(
            far <= 24,
            "kept frames should hug the playhead, furthest={far}"
        );
    }

    #[test]
    fn frame_cache_eviction_drops_frames_no_longer_on_the_timeline_first() {
        let timestamps: Vec<i64> = (0..4).map(|i| 1_000_000 - i * 300).collect();
        // Two cached frames that fell off the timeline (e.g. after `i` narrowed
        // the window) plus the four live ones.
        let mut cached = vec![55_555, 66_666];
        cached.extend(timestamps.iter().copied());
        let evicted = frames_to_evict(&cached, &timestamps, 0, 4);
        assert_eq!(
            evicted.len(),
            2,
            "only the excess is dropped, not the whole tail"
        );
        assert!(evicted.contains(&55_555) && evicted.contains(&66_666));
        for ts in &timestamps {
            assert!(!evicted.contains(ts), "live timeline frames must be kept");
        }
    }

    /// `note_frame_failure` has no seam to drive without a fully constructed
    /// `App`, so this asserts the policy call it now delegates to
    /// (`FRAME_RETRY_POLICY.delay_for(entry.attempts)`, `attempts` already
    /// incremented to 1 on the first failure — see the doc comment on
    /// `FRAME_RETRY_POLICY`) against today's hand-rolled sequence: 4, 8, 16,
    /// 32, 64, then clamps to 90, and gives up at attempt 8.
    #[test]
    fn frame_retry_sequence_matches_today() {
        let expected = [4u64, 8, 16, 32, 64, 90, 90];
        for (i, secs) in expected.into_iter().enumerate() {
            let attempts = i as u32 + 1; // first failure increments to 1 before use
            assert_eq!(
                FRAME_RETRY_POLICY.delay_for(attempts),
                Duration::from_secs(secs),
                "attempts {attempts}"
            );
        }
        assert!(!FRAME_RETRY_POLICY.exhausted(7));
        assert!(FRAME_RETRY_POLICY.exhausted(8));
    }

    #[test]
    fn ring_distance_measures_across_the_wrap() {
        // 10 slots: index 0 is newest, 9 oldest, and playback joins them.
        assert_eq!(ring_distance(0, 0, 10), 0);
        assert_eq!(ring_distance(9, 0, 10), 1, "oldest is one step from newest");
        assert_eq!(ring_distance(0, 9, 10), 1, "and symmetrically back");
        assert_eq!(ring_distance(5, 0, 10), 5, "the far side stays far");
        assert_eq!(ring_distance(8, 1, 10), 3);
        assert_eq!(
            ring_distance(0, 0, 0),
            0,
            "an empty timeline has no distance"
        );
    }

    #[test]
    fn eviction_keeps_the_frames_just_past_the_wrap() {
        // Playhead on the newest frame, every slot cached, cap 48.  The next
        // frames playback shows are the oldest ones, across the seam.
        let timestamps: Vec<i64> = (0..288).map(|i| 1_000_000 - i * 300).collect();
        let evicted = frames_to_evict(&timestamps, &timestamps, 0, 48);
        let kept: Vec<i64> = timestamps
            .iter()
            .copied()
            .filter(|ts| !evicted.contains(ts))
            .collect();
        assert_eq!(kept.len(), 48);
        assert!(kept.contains(&timestamps[0]), "displayed frame survives");
        // The oldest frames — one step away round the ring — must survive too.
        assert!(
            kept.contains(&timestamps[287]),
            "the frame playback wraps onto was evicted"
        );
        assert!(kept.contains(&timestamps[286]));
        // The genuinely distant middle of the timeline is what goes.
        assert!(!kept.contains(&timestamps[144]));
    }

    fn dummy_obs_point() -> ObservationPoint {
        ObservationPoint {
            point: GeoPoint { lon: 0.0, lat: 0.0 },
            world: WorldPoint { x: 0.0, y: 0.0 },
            station_id: "test".into(),
            wigos_id: "0-0-0-test".into(),
            temperature: None,
            wind_speed: None,
            wind_direction: None,
            humidity: None,
            pressure: None,
        }
    }

    #[test]
    fn reset_obs_accumulator_clears_stale_partial_from_a_prior_refresh() {
        // Refresh A accumulated data via PartialCommit (e.g. capitals phase)
        // but never reached `Ready` — it errored out mid-flight, so
        // `obs_partial` is left holding A's data.
        let mut obs_incoming = vec![dummy_obs_point()];
        let mut obs_incoming_id = 1u64;
        let mut obs_partial = vec![dummy_obs_point(), dummy_obs_point()];

        // Refresh B kicks off with a new id. It is about to produce zero
        // `Point`s before erroring — the scenario the lazy, first-Point-only
        // reset could never catch.
        reset_obs_accumulator(&mut obs_incoming, &mut obs_incoming_id, &mut obs_partial, 2);

        assert!(
            obs_partial.is_empty(),
            "A's stale partial state must not survive B's kickoff"
        );
        assert!(obs_incoming.is_empty());
        assert_eq!(obs_incoming_id, 2);
    }

    #[test]
    fn stepping_older_wraps_past_the_oldest_onto_the_newest() {
        assert_eq!(stepped_index(0, 10, Step::Older), Some(1));
        assert_eq!(stepped_index(8, 10, Step::Older), Some(9));
        assert_eq!(
            stepped_index(9, 10, Step::Older),
            Some(0),
            "past the oldest comes the newest, not a dead end"
        );
    }

    #[test]
    fn stepping_newer_wraps_past_the_newest_onto_the_oldest() {
        assert_eq!(stepped_index(9, 10, Step::Newer), Some(8));
        assert_eq!(stepped_index(1, 10, Step::Newer), Some(0));
        assert_eq!(
            stepped_index(0, 10, Step::Newer),
            Some(9),
            "past the newest comes the oldest"
        );
    }

    #[test]
    fn stepping_an_empty_timeline_does_nothing() {
        assert_eq!(stepped_index(0, 0, Step::Older), None);
        assert_eq!(stepped_index(0, 0, Step::Newer), None);
    }

    #[test]
    fn stepping_a_single_frame_timeline_stays_put() {
        assert_eq!(stepped_index(0, 1, Step::Older), Some(0));
        assert_eq!(stepped_index(0, 1, Step::Newer), Some(0));
    }

    #[test]
    fn stepping_from_a_stale_index_lands_back_in_range() {
        // `i` narrowing the window can leave the playhead past the new end.
        assert_eq!(stepped_index(50, 10, Step::Older), Some(0));
        assert_eq!(stepped_index(50, 10, Step::Newer), Some(9));
    }

    #[test]
    fn a_full_lap_of_steps_returns_to_where_it_started() {
        let len = 7;
        let mut i = 0;
        for _ in 0..len {
            i = stepped_index(i, len, Step::Older).unwrap();
        }
        assert_eq!(i, 0, "stepping older through every slot closes the loop");
        for _ in 0..len {
            i = stepped_index(i, len, Step::Newer).unwrap();
        }
        assert_eq!(i, 0, "and so does stepping back the other way");
    }

    #[test]
    fn frame_cache_under_cap_is_left_alone() {
        let timestamps: Vec<i64> = (0..10).map(|i| 1_000_000 - i * 300).collect();
        assert!(frames_to_evict(&timestamps, &timestamps, 0, 48).is_empty());
    }

    #[test]
    fn phantom_slot_is_removed_from_the_timeline() {
        let ts = [500i64, 400, 300];
        // 400 was requested but the provider served 300's data.
        let (remaining, _) = timeline_without_phantom(&ts, 0, 400, 300, false).unwrap();
        assert_eq!(remaining, vec![500, 300], "phantom slot must not remain");
    }

    #[test]
    fn viewer_keeps_watching_the_same_time_across_a_removal() {
        let ts = [500i64, 400, 300];
        // Viewing 300 (index 2) while an earlier slot 400 turns out phantom.
        // Index must follow the time, not stay at 2 (which no longer exists).
        let (remaining, index) = timeline_without_phantom(&ts, 2, 400, 300, false).unwrap();
        assert_eq!(remaining[index], 300, "must still display the same time");
        assert_eq!(index, 1);
    }

    #[test]
    fn viewing_the_phantom_itself_falls_back_to_the_resolved_time() {
        let ts = [500i64, 400, 300];
        let (remaining, index) = timeline_without_phantom(&ts, 1, 400, 300, false).unwrap();
        assert_eq!(remaining[index], 300, "falls back to what was resolved");
    }

    #[test]
    fn live_mode_snaps_back_to_the_newest_frame() {
        let ts = [500i64, 400, 300];
        let (_, index) = timeline_without_phantom(&ts, 2, 400, 300, true).unwrap();
        assert_eq!(index, 0, "live always shows newest");
    }

    #[test]
    fn unknown_slot_is_not_a_removal() {
        let ts = [500i64, 400, 300];
        assert!(timeline_without_phantom(&ts, 0, 999, 300, false).is_none());
    }

    /// A coarse GeoIP-only first fix (this dev box measures 10 km) must still
    /// move the viewport to the fix's own point — it is far better than the
    /// Europe fallback — even though the same fix is too imprecise to draw a
    /// marker for. The display gate and the boot placement are independent.
    #[test]
    fn a_coarse_first_fix_still_moves_the_viewport_though_it_would_not_draw_a_marker() {
        let fix = LocationFix::new(
            GeoPoint::new(14.5, 46.0),
            Some(10_000.0),
            LocationSource::Ip,
        );
        assert!(
            !fix.is_displayable(),
            "sanity check: a 10 km fix must not be displayable"
        );

        let viewport = viewport_for_fix(&fix, None);
        let expected = Viewport::from_lat_lon(46.0, 14.5, 5.0);
        assert_eq!(
            viewport.center, expected.center,
            "viewport must centre on the fix's point regardless of accuracy"
        );
    }

    fn eumetnet_config(api_key: &str) -> EumetnetConfig {
        EumetnetConfig {
            surface_endpoint: "https://example.test".into(),
            api_key: api_key.into(),
        }
    }

    #[test]
    fn eumetnet_rebuild_not_needed_when_key_unchanged() {
        let old = eumetnet_config("abc123");
        let new = eumetnet_config("abc123");
        assert!(!App::eumetnet_rebuild_needed(&old, &new));
    }

    #[test]
    fn eumetnet_rebuild_needed_when_key_changed() {
        let old = eumetnet_config("abc123");
        let new = eumetnet_config("xyz789");
        assert!(App::eumetnet_rebuild_needed(&old, &new));
    }

    #[test]
    fn eumetnet_rebuild_not_needed_for_whitespace_only_difference() {
        let old = eumetnet_config("abc123");
        let new = eumetnet_config("  abc123  ");
        assert!(!App::eumetnet_rebuild_needed(&old, &new));
    }
}
