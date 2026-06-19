use std::path::Path;
use std::time::Duration;

use color_eyre::eyre::{Context, Result};
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, Proxy};

use crate::cache::write_log;
use crate::geo::GeoPoint;
use crate::layers::LocationFix;

pub async fn locate(log_path: &Path) -> Result<Option<LocationFix>> {
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

    let client_path: OwnedObjectPath = manager
        .call("GetClient", &())
        .await
        .wrap_err("request GeoClue client")?;
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
    if let Err(e) = client.set_property("RequestedAccuracyLevel", &4_u32).await {
        write_log(
            log_path,
            format!("geoclue: failed to set RequestedAccuracyLevel: {e}"),
        );
    }
    client
        .call::<_, _, ()>("Start", &())
        .await
        .wrap_err("start GeoClue client")?;
    tokio::time::sleep(Duration::from_millis(350)).await;

    let location_path: OwnedObjectPath = client
        .get_property("Location")
        .await
        .wrap_err("read GeoClue location path")?;
    let location = Proxy::new(
        &connection,
        "org.freedesktop.GeoClue2",
        location_path.as_str(),
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
    Ok(Some(LocationFix {
        point: GeoPoint::new(lon, lat),
        label: "GeoClue".to_string(),
    }))
}
