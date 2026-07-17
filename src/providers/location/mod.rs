//! Platform-agnostic location acquisition.
//!
//! Every backend (GeoClue on Linux, Geolocator on Windows, CoreLocation on
//! macOS, IP lookup anywhere) is a task that pushes [`LocationFix`] values into
//! one shared channel.  Backends never coordinate with each other and may
//! deliver fixes in any order, at any time, forever — the [`LocationArbiter`]
//! decides which fix actually wins.
//!
//! This mirrors the observation/warning providers: work happens off the event
//! loop and results arrive through an unbounded mpsc drained on each tick.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::LocationConfig;
use crate::geo::GeoPoint;

pub mod ip;

#[cfg(target_os = "linux")]
pub mod geoclue;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(windows)]
pub mod windows;

/// Where a fix came from.  Ordering is deliberate: a later variant is a
/// better source than an earlier one, so `PartialOrd` breaks ties when two
/// fixes report the same (or no) accuracy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LocationSource {
    /// Coarse city-level guess from an IP address lookup.
    Ip,
    /// The OS location service (GeoClue / Geolocator / CoreLocation).
    Platform,
    /// Explicitly supplied via `--lat/--lon`.  Never overridden.
    Manual,
}

impl LocationSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ip => "IP",
            Self::Platform => PLATFORM_LABEL,
            Self::Manual => "CLI",
        }
    }
}

#[cfg(target_os = "linux")]
const PLATFORM_LABEL: &str = "GeoClue";
#[cfg(target_os = "macos")]
const PLATFORM_LABEL: &str = "CoreLocation";
#[cfg(windows)]
const PLATFORM_LABEL: &str = "Windows";
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
const PLATFORM_LABEL: &str = "Platform";

/// A single position report.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocationFix {
    pub point: GeoPoint,
    /// Horizontal accuracy in metres — the radius of the 95% confidence
    /// circle.  `None` when the backend does not report one, which is treated
    /// as "worse than any known accuracy" by the arbiter.
    pub accuracy_m: Option<f64>,
    pub source: LocationSource,
    pub at: SystemTime,
}

impl LocationFix {
    pub fn new(point: GeoPoint, accuracy_m: Option<f64>, source: LocationSource) -> Self {
        Self {
            point,
            accuracy_m,
            source,
            at: SystemTime::now(),
        }
    }

    /// Human-readable summary for the layer status line, e.g. `GeoClue ±12 m`.
    pub fn label(&self) -> String {
        match self.accuracy_m {
            Some(a) if a >= 1000.0 => format!("{} ±{:.0} km", self.source.label(), a / 1000.0),
            Some(a) => format!("{} ±{:.0} m", self.source.label(), a),
            None => self.source.label().to_string(),
        }
    }
}

/// A fix is considered stale once it is this old.  A stale incumbent loses to
/// any fresh fix regardless of accuracy, so a laptop that moved while the GPS
/// was asleep still converges instead of pinning the marker to a dead fix.
const STALE_AFTER: Duration = Duration::from_secs(5 * 60);

/// Decides which of a stream of competing fixes is the current one.
#[derive(Debug, Default, Clone)]
pub struct LocationArbiter {
    current: Option<LocationFix>,
}

impl LocationArbiter {
    pub fn new() -> Self {
        Self { current: None }
    }

    pub fn current(&self) -> Option<LocationFix> {
        self.current
    }

    /// Offer a fix. Returns `true` when it became the current fix.
    ///
    /// Accepted when any of these hold:
    /// - there is no current fix;
    /// - the current fix is [`Manual`](LocationSource::Manual) — never replaced;
    /// - the candidate is `Manual`;
    /// - the current fix is stale;
    /// - the candidate comes from the same source (a refresh of that source);
    /// - the candidate is strictly more accurate.
    pub fn offer(&mut self, fix: LocationFix) -> bool {
        let accept = match self.current {
            None => true,
            Some(cur) if cur.source == LocationSource::Manual => false,
            Some(_) if fix.source == LocationSource::Manual => true,
            Some(cur) => {
                let stale = fix
                    .at
                    .duration_since(cur.at)
                    .is_ok_and(|age| age > STALE_AFTER);
                stale || fix.source == cur.source || is_better(&fix, &cur)
            }
        };
        if accept {
            self.current = Some(fix);
        }
        accept
    }
}

/// True when `candidate` is a better fix than `incumbent`.
///
/// A known accuracy always beats an unknown one; when both are known the
/// smaller radius wins.  On an exact tie the better source wins: GeoClue and
/// the IP fallback both report ±25 km when GeoClue is itself resolving over
/// GeoIP, and without the tie-break whichever happened to arrive first would
/// hold the fix forever — locking out the OS service that is the one able to
/// refine later over WiFi or GPS.
fn is_better(candidate: &LocationFix, incumbent: &LocationFix) -> bool {
    match (candidate.accuracy_m, incumbent.accuracy_m) {
        (Some(new), Some(old)) => {
            new < old || (new == old && candidate.source > incumbent.source)
        }
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => candidate.source > incumbent.source,
    }
}

/// Handle to the running location backends.  Dropping it stops nothing — the
/// tasks are detached and live for the process lifetime, matching how the
/// other providers spawn work.
pub struct LocationStream {
    pub rx: tokio::sync::mpsc::UnboundedReceiver<LocationFix>,
}

/// Start every backend available on this platform.
///
/// Backends are independent: one failing (no GeoClue daemon, denied
/// permission, no network) never prevents the others from delivering.
pub fn spawn(config: &LocationConfig, log_path: &Path) -> LocationStream {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let log: PathBuf = log_path.to_path_buf();

    #[cfg(target_os = "linux")]
    {
        let tx = tx.clone();
        let log = log.clone();
        tokio::spawn(async move {
            if let Err(e) = geoclue::run(tx, &log).await {
                crate::cache::write_log(&log, format!("location: geoclue backend stopped: {e}"));
            }
        });
    }

    #[cfg(windows)]
    {
        let tx = tx.clone();
        let log = log.clone();
        tokio::spawn(async move {
            if let Err(e) = windows::run(tx, &log).await {
                crate::cache::write_log(&log, format!("location: windows backend stopped: {e}"));
            }
        });
    }

    #[cfg(target_os = "macos")]
    {
        let tx = tx.clone();
        let log = log.clone();
        tokio::spawn(async move {
            if let Err(e) = macos::run(tx, &log).await {
                crate::cache::write_log(&log, format!("location: macos backend stopped: {e}"));
            }
        });
    }

    if config.ip_fallback {
        let endpoint = config.ip_endpoint.clone();
        let tx = tx.clone();
        let log = log.clone();
        tokio::spawn(async move {
            if let Err(e) = ip::run(tx, endpoint, &log).await {
                crate::cache::write_log(&log, format!("location: ip backend stopped: {e}"));
            }
        });
    }

    drop(tx);
    LocationStream { rx }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix(source: LocationSource, accuracy_m: Option<f64>) -> LocationFix {
        LocationFix::new(GeoPoint::new(14.5, 46.0), accuracy_m, source)
    }

    fn aged(source: LocationSource, accuracy_m: Option<f64>, age: Duration) -> LocationFix {
        let mut f = fix(source, accuracy_m);
        f.at = SystemTime::now() - age;
        f
    }

    #[test]
    fn first_fix_is_always_accepted() {
        let mut arb = LocationArbiter::new();
        assert!(arb.offer(fix(LocationSource::Ip, Some(20_000.0))));
        assert_eq!(arb.current().unwrap().source, LocationSource::Ip);
    }

    #[test]
    fn more_accurate_fix_replaces_coarser_one() {
        let mut arb = LocationArbiter::new();
        arb.offer(fix(LocationSource::Ip, Some(20_000.0)));
        assert!(arb.offer(fix(LocationSource::Platform, Some(12.0))));
        assert_eq!(arb.current().unwrap().accuracy_m, Some(12.0));
    }

    #[test]
    fn coarser_fix_does_not_replace_accurate_one() {
        let mut arb = LocationArbiter::new();
        arb.offer(fix(LocationSource::Platform, Some(12.0)));
        assert!(!arb.offer(fix(LocationSource::Ip, Some(20_000.0))));
        assert_eq!(arb.current().unwrap().accuracy_m, Some(12.0));
    }

    #[test]
    fn same_source_refreshes_even_when_less_accurate() {
        let mut arb = LocationArbiter::new();
        arb.offer(fix(LocationSource::Platform, Some(12.0)));
        assert!(arb.offer(fix(LocationSource::Platform, Some(80.0))));
        assert_eq!(arb.current().unwrap().accuracy_m, Some(80.0));
    }

    #[test]
    fn manual_fix_is_never_overridden() {
        let mut arb = LocationArbiter::new();
        arb.offer(fix(LocationSource::Manual, None));
        assert!(!arb.offer(fix(LocationSource::Platform, Some(1.0))));
        assert_eq!(arb.current().unwrap().source, LocationSource::Manual);
    }

    #[test]
    fn manual_fix_overrides_anything_else() {
        let mut arb = LocationArbiter::new();
        arb.offer(fix(LocationSource::Platform, Some(1.0)));
        assert!(arb.offer(fix(LocationSource::Manual, None)));
        assert_eq!(arb.current().unwrap().source, LocationSource::Manual);
    }

    #[test]
    fn stale_incumbent_loses_to_fresh_coarse_fix() {
        let mut arb = LocationArbiter::new();
        arb.offer(aged(
            LocationSource::Platform,
            Some(12.0),
            STALE_AFTER + Duration::from_secs(60),
        ));
        assert!(arb.offer(fix(LocationSource::Ip, Some(20_000.0))));
        assert_eq!(arb.current().unwrap().source, LocationSource::Ip);
    }

    /// Regression: on this dev box GeoClue and the IP fallback both report
    /// ±25 km, and IP usually lands first.  A strict `<` comparison left the
    /// coarse IP guess holding the fix and the OS service permanently locked
    /// out, so it could never refine over WiFi/GPS later.
    #[test]
    fn platform_fix_beats_ip_fix_of_identical_accuracy() {
        let mut arb = LocationArbiter::new();
        arb.offer(fix(LocationSource::Ip, Some(25_000.0)));
        assert!(arb.offer(fix(LocationSource::Platform, Some(25_000.0))));
        assert_eq!(arb.current().unwrap().source, LocationSource::Platform);
    }

    /// The tie-break must not run backwards: IP never displaces the OS.
    #[test]
    fn ip_fix_does_not_beat_platform_fix_of_identical_accuracy() {
        let mut arb = LocationArbiter::new();
        arb.offer(fix(LocationSource::Platform, Some(25_000.0)));
        assert!(!arb.offer(fix(LocationSource::Ip, Some(25_000.0))));
        assert_eq!(arb.current().unwrap().source, LocationSource::Platform);
    }

    #[test]
    fn known_accuracy_beats_unknown_accuracy() {
        let mut arb = LocationArbiter::new();
        arb.offer(fix(LocationSource::Platform, None));
        assert!(arb.offer(fix(LocationSource::Ip, Some(20_000.0))));
    }

    #[test]
    fn better_source_wins_when_neither_reports_accuracy() {
        let mut arb = LocationArbiter::new();
        arb.offer(fix(LocationSource::Ip, None));
        assert!(arb.offer(fix(LocationSource::Platform, None)));
        assert_eq!(arb.current().unwrap().source, LocationSource::Platform);
    }

    #[test]
    fn label_renders_metres_and_kilometres() {
        assert_eq!(
            fix(LocationSource::Platform, Some(12.0)).label(),
            format!("{PLATFORM_LABEL} ±12 m")
        );
        assert_eq!(fix(LocationSource::Ip, Some(20_000.0)).label(), "IP ±20 km");
        assert_eq!(fix(LocationSource::Manual, None).label(), "CLI");
    }
}
