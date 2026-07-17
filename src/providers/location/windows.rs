//! Windows Geolocator backend.
//!
//! NOTE: type-checked against the `windows` crate for `x86_64-pc-windows-msvc`,
//! but never run on real hardware — the project is developed on Linux.  The
//! access prompt and event delivery in particular are unverified.
//!
//! `PositionChanged` fires whenever the OS refines the position, which maps
//! directly onto the streaming model: every event becomes a fix and the
//! arbiter decides whether it wins.

use std::path::Path;

use color_eyre::eyre::{eyre, Result};
use tokio::sync::mpsc::UnboundedSender;
use windows::Devices::Geolocation::{
    GeolocationAccessStatus, Geolocator, PositionAccuracy, PositionChangedEventArgs,
};
use windows::Foundation::TypedEventHandler;

use super::{LocationFix, LocationSource};
use crate::cache::write_log;
use crate::geo::GeoPoint;

/// Minimum gap between reports, milliseconds.  Weather radar does not need
/// per-second updates and a longer interval lets the OS keep the GPS asleep.
const REPORT_INTERVAL_MS: u32 = 30_000;

pub async fn run(tx: UnboundedSender<LocationFix>, log_path: &Path) -> Result<()> {
    let access = Geolocator::RequestAccessAsync()
        .map_err(|e| eyre!("request location access: {e}"))?
        .await
        .map_err(|e| eyre!("await location access: {e}"))?;
    if access != GeolocationAccessStatus::Allowed {
        return Err(eyre!(
            "location access not granted (status {access:?}); enable it in Windows privacy settings"
        ));
    }

    let locator = Geolocator::new().map_err(|e| eyre!("create Geolocator: {e}"))?;
    if let Err(e) = locator.SetDesiredAccuracy(PositionAccuracy::High) {
        write_log(log_path, format!("location/windows: set accuracy: {e}"));
    }
    if let Err(e) = locator.SetReportInterval(REPORT_INTERVAL_MS) {
        write_log(log_path, format!("location/windows: set interval: {e}"));
    }

    let handler_tx = tx.clone();
    let token = locator
        .PositionChanged(&TypedEventHandler::<Geolocator, PositionChangedEventArgs>::new(
            move |_sender, args| {
                let args = args.ok()?;
                if let Some(fix) = fix_from_args(args) {
                    // Ignore send failure: the app is shutting down and the
                    // handler must not surface that as a WinRT error.
                    let _ = handler_tx.send(fix);
                }
                Ok(())
            },
        ))
        .map_err(|e| eyre!("subscribe to PositionChanged: {e}"))?;

    // Hold the Geolocator alive; dropping it unsubscribes and events stop.
    // Park until the app drops the receiver.
    tx.closed().await;

    if let Err(e) = locator.RemovePositionChanged(token) {
        write_log(log_path, format!("location/windows: unsubscribe: {e}"));
    }
    Ok(())
}

/// Pull a fix out of a `PositionChanged` event, or `None` if any field is
/// unreadable — a malformed report should be skipped, not kill the stream.
fn fix_from_args(args: &PositionChangedEventArgs) -> Option<LocationFix> {
    let coord = args.Position().ok()?.Coordinate().ok()?;
    let position = coord.Point().ok()?.Position().ok()?;
    // Accuracy is documented as always present, but treat it as optional so a
    // driver that omits it still yields a usable fix.
    let accuracy_m = coord.Accuracy().ok().filter(|a| *a >= 0.0);
    Some(LocationFix::new(
        GeoPoint::new(position.Longitude, position.Latitude),
        accuracy_m,
        LocationSource::Platform,
    ))
}
