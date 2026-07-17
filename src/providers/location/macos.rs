//! macOS CoreLocation backend.
//!
//! NOTE: type-checked against objc2 for `aarch64-apple-darwin`, but never run
//! on real hardware — the project is developed on Linux.  The authorization
//! prompt and delegate callbacks in particular are unverified.
//!
//! CoreLocation delivers updates to a delegate via a run loop, which a tokio
//! app does not have.  So the manager lives on its own dedicated thread
//! running an `NSRunLoop`, and fixes cross back to the async world through the
//! same mpsc every other backend uses.  `UnboundedSender` is `Send` and its
//! `send` takes `&self`, so the delegate can hold one directly.

use std::path::Path;

use color_eyre::eyre::{eyre, Result};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_location::{CLLocation, CLLocationManager, CLLocationManagerDelegate};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol, NSRunLoop};
use tokio::sync::mpsc::UnboundedSender;

use super::{LocationFix, LocationSource};
use crate::cache::write_log;
use crate::geo::GeoPoint;

/// Ignore refinements smaller than this (metres) so a jittering GPS does not
/// wake the render loop for sub-radar-pixel movement.
const DISTANCE_FILTER_M: f64 = 50.0;

pub struct DelegateIvars {
    tx: UnboundedSender<LocationFix>,
    log_path: std::path::PathBuf,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "FrontLocationDelegate"]
    #[ivars = DelegateIvars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl CLLocationManagerDelegate for Delegate {
        #[unsafe(method(locationManager:didUpdateLocations:))]
        fn did_update_locations(
            &self,
            _manager: &CLLocationManager,
            locations: &NSArray<CLLocation>,
        ) {
            // CoreLocation may coalesce several fixes into one callback; the
            // last element is the most recent.
            let Some(location) = locations.lastObject() else {
                return;
            };
            let coord = unsafe { location.coordinate() };
            // horizontalAccuracy is negative when the fix is invalid.
            let accuracy = unsafe { location.horizontalAccuracy() };
            if accuracy < 0.0 {
                return;
            }
            let fix = LocationFix::new(
                GeoPoint::new(coord.longitude, coord.latitude),
                Some(accuracy),
                LocationSource::Platform,
            );
            let _ = self.ivars().tx.send(fix);
        }

        #[unsafe(method(locationManager:didFailWithError:))]
        fn did_fail(&self, _manager: &CLLocationManager, error: &NSError) {
            // Transient failures are normal (no WiFi fix yet, indoors).
            // CoreLocation keeps trying, so just record and carry on.
            write_log(
                &self.ivars().log_path,
                format!("location/macos: update failed: {error:?}"),
            );
        }
    }
);

impl Delegate {
    fn new(tx: UnboundedSender<LocationFix>, log_path: std::path::PathBuf) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { tx, log_path });
        unsafe { msg_send![super(this), init] }
    }
}

pub async fn run(tx: UnboundedSender<LocationFix>, log_path: &Path) -> Result<()> {
    let log = log_path.to_path_buf();
    let thread_tx = tx.clone();

    // CLLocationManager must be created on a thread with a live run loop, and
    // that run loop blocks forever — hence a dedicated OS thread rather than a
    // tokio task.
    std::thread::Builder::new()
        .name("front-corelocation".to_string())
        .spawn(move || {
            let delegate = Delegate::new(thread_tx, log);
            let manager: Retained<CLLocationManager> =
                unsafe { CLLocationManager::new() };
            let proto = ProtocolObject::from_ref(&*delegate);
            unsafe {
                manager.setDelegate(Some(proto));
                manager.setDistanceFilter(DISTANCE_FILTER_M);
                manager.requestWhenInUseAuthorization();
                manager.startUpdatingLocation();
            }
            // Blocks for the lifetime of the process, driving the delegate.
            NSRunLoop::currentRunLoop().run();
        })
        .map_err(|e| eyre!("spawn CoreLocation thread: {e}"))?;

    // Park until the app drops the receiver so this task mirrors the other
    // backends' lifetimes.
    tx.closed().await;
    Ok(())
}
