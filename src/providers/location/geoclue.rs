//! GeoClue2 backend (Linux).
//!
//! Unlike the previous one-shot implementation — which slept 350 ms and then
//! read the `Location` property exactly once, racing the daemon and discarding
//! accuracy — this subscribes to the client's `LocationUpdated` signal and
//! keeps streaming fixes for the process lifetime.  GeoClue refines its answer
//! over time (IP → WiFi → GPS), so each refinement arrives as its own fix and
//! the arbiter upstream decides whether it wins.

use std::path::Path;
use std::time::Duration;

use color_eyre::eyre::{eyre, Context, Result};
use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, Proxy};

use super::{LocationFix, LocationSource};
use crate::cache::write_log;
use crate::geo::GeoPoint;

/// GeoClue accuracy level 8 = "exact" (GPS-grade when hardware allows).
/// The daemon still downgrades this per its own policy, so asking for the
/// best available costs nothing when only WiFi positioning exists.
const ACCURACY_LEVEL_EXACT: u32 = 8;

/// `GetClient` blocks indefinitely when no GeoClue *agent* is running: the
/// daemon parks the call waiting for an authorisation decision that will never
/// come.  Desktop environments ship an agent (gnome-shell, phosh, …) but a
/// bare window manager has none, so this must not be an unbounded wait — the
/// task would hang for the process lifetime and the layer would sit on
/// "Loading" forever.
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Wrap a D-Bus call that GeoClue can park forever.
async fn with_timeout<T>(
    what: &'static str,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(SETUP_TIMEOUT, fut).await {
        Ok(v) => v,
        Err(_) => Err(eyre!(
            "{what} timed out after {}s — no GeoClue agent appears to be running. \
             Desktop environments provide one; on a bare window manager start \
             `/usr/lib/geoclue-2.0/demos/agent`, or set location.ip_fallback in config.toml",
            SETUP_TIMEOUT.as_secs()
        )),
    }
}

pub async fn run(tx: UnboundedSender<LocationFix>, log_path: &Path) -> Result<()> {
    let connection = Connection::system()
        .await
        .wrap_err("connect to system DBus for GeoClue")?;
    let manager = Proxy::new(
        &connection,
        "org.freedesktop.GeoClue2",
        "/org/freedesktop/GeoClue2/Manager",
        "org.freedesktop.GeoClue2.Manager",
    )
    .await
    .wrap_err("create GeoClue manager proxy")?;

    let client_path: OwnedObjectPath = with_timeout("GeoClue GetClient", async {
        manager
            .call("GetClient", &())
            .await
            .wrap_err("request GeoClue client")
    })
    .await?;
    let client = Proxy::new(
        &connection,
        "org.freedesktop.GeoClue2",
        client_path.as_str(),
        "org.freedesktop.GeoClue2.Client",
    )
    .await
    .wrap_err("create GeoClue client proxy")?;

    if let Err(e) = client.set_property("DesktopId", &"front").await {
        write_log(log_path, format!("geoclue: failed to set DesktopId: {e}"));
    }
    if let Err(e) = client
        .set_property("RequestedAccuracyLevel", &ACCURACY_LEVEL_EXACT)
        .await
    {
        write_log(
            log_path,
            format!("geoclue: failed to set RequestedAccuracyLevel: {e}"),
        );
    }

    // Subscribe before Start so the first update cannot be missed.
    let mut updates = client
        .receive_signal("LocationUpdated")
        .await
        .wrap_err("subscribe to GeoClue LocationUpdated")?;

    with_timeout("GeoClue Start", async {
        client
            .call::<_, _, ()>("Start", &())
            .await
            .wrap_err("start GeoClue client")
    })
    .await?;

    // The daemon may already hold a cached fix from a previous client; it is
    // published as the Location property without ever firing a signal.
    if let Ok(path) = client.get_property::<OwnedObjectPath>("Location").await {
        if path.as_str() != "/" {
            match read_fix(&connection, path.as_str()).await {
                Ok(fix) => {
                    if tx.send(fix).is_err() {
                        return Ok(());
                    }
                }
                Err(e) => write_log(log_path, format!("geoclue: initial location read: {e}")),
            }
        }
    }

    while let Some(signal) = updates.next().await {
        // LocationUpdated(o old, o new) — only the new path matters.
        let (_old, new): (OwnedObjectPath, OwnedObjectPath) = match signal.body().deserialize() {
            Ok(v) => v,
            Err(e) => {
                write_log(log_path, format!("geoclue: malformed LocationUpdated: {e}"));
                continue;
            }
        };
        match read_fix(&connection, new.as_str()).await {
            Ok(fix) => {
                // Receiver gone — the app is shutting down.
                if tx.send(fix).is_err() {
                    break;
                }
            }
            Err(e) => write_log(log_path, format!("geoclue: location read: {e}")),
        }
    }

    Ok(())
}

/// Read one `org.freedesktop.GeoClue2.Location` object into a fix.
async fn read_fix(connection: &Connection, path: &str) -> Result<LocationFix> {
    let location = Proxy::new(
        connection,
        "org.freedesktop.GeoClue2",
        path,
        "org.freedesktop.GeoClue2.Location",
    )
    .await
    .wrap_err("create GeoClue location proxy")?;

    let lat: f64 = location
        .get_property("Latitude")
        .await
        .wrap_err("read GeoClue latitude")?;
    let lon: f64 = location
        .get_property("Longitude")
        .await
        .wrap_err("read GeoClue longitude")?;
    // Accuracy is optional in practice: GeoClue reports a negative value when
    // it has no estimate.
    let accuracy_m = location
        .get_property::<f64>("Accuracy")
        .await
        .ok()
        .filter(|a| *a >= 0.0);

    Ok(LocationFix::new(
        GeoPoint::new(lon, lat),
        accuracy_m,
        LocationSource::Platform,
    ))
}
