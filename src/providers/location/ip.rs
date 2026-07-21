//! IP-geolocation backend — the universal fallback.
//!
//! Works on every platform with no OS integration and no permission prompt,
//! at the cost of accuracy (city-level at best) and privacy: it discloses this
//! machine's IP address to a third-party service.  For that reason it is
//! opt-out via `location.ip_fallback` in config.toml, and it always reports a
//! deliberately pessimistic accuracy so any real platform fix outranks it.

use std::path::Path;
use std::time::Duration;

use color_eyre::eyre::{eyre, Result};
use serde::Deserialize;
use tokio::sync::mpsc::UnboundedSender;

use super::{LocationFix, LocationSource};
use crate::cache::write_log;
use crate::geo::GeoPoint;
use crate::retry::RetryPolicy;

/// Assumed radius for a city-level IP lookup.  Services rarely return a real
/// accuracy figure, and when they do it is optimistic; 25 km keeps the fix
/// ranked below anything the OS produces.
const ASSUMED_ACCURACY_M: f64 = 25_000.0;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How often to re-check after a success.  IP location only changes when the
/// network changes, so this is a slow background correction, not a tracker.
const REFRESH_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Backoff after a failed lookup.  These services rate-limit shared/CGNAT
/// addresses aggressively, and a 429 on the very first attempt is common —
/// waiting a full `REFRESH_INTERVAL` would leave the user with no location at
/// all for half an hour.  Retry sooner, backing off to the normal interval.
const RETRY_INTERVAL: Duration = Duration::from_secs(60);
const MAX_RETRY_INTERVAL: Duration = REFRESH_INTERVAL;

#[derive(Debug, Deserialize)]
struct IpApiResponse {
    #[serde(alias = "latitude")]
    lat: Option<f64>,
    #[serde(alias = "longitude")]
    lon: Option<f64>,
}

pub async fn run(tx: UnboundedSender<LocationFix>, endpoint: String, log_path: &Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| eyre!("build IP geolocation HTTP client: {e}"))?;

    const POLICY: RetryPolicy = RetryPolicy::new(RETRY_INTERVAL, MAX_RETRY_INTERVAL, None);
    let mut failures: u32 = 0;
    loop {
        let delay = match fetch(&client, &endpoint).await {
            Ok(fix) => {
                if tx.send(fix).is_err() {
                    return Ok(());
                }
                failures = 0;
                REFRESH_INTERVAL
            }
            // A failed IP lookup is entirely routine (offline, rate limited,
            // service down) and never fatal — the platform backend may still
            // be delivering. Log it and try again later.
            Err(e) => {
                write_log(log_path, format!("location/ip: lookup failed: {e}"));
                let delay = POLICY.delay_for(failures);
                failures += 1;
                delay
            }
        };
        tokio::time::sleep(delay).await;
    }
}

async fn fetch(client: &reqwest::Client, endpoint: &str) -> Result<LocationFix> {
    let resp = client
        .get(endpoint)
        .send()
        .await
        .map_err(|e| eyre!("request: {e}"))?;
    if !resp.status().is_success() {
        return Err(eyre!("HTTP {}", resp.status()));
    }
    let body = resp.text().await.map_err(|e| eyre!("body: {e}"))?;
    parse(&body)
}

/// Parse a lat/lon out of an IP-geolocation JSON response.
///
/// Kept separate from the HTTP call so the field-name handling is testable —
/// services disagree on whether the keys are `lat`/`lon` or
/// `latitude`/`longitude`.
fn parse(body: &str) -> Result<LocationFix> {
    let parsed: IpApiResponse =
        serde_json::from_str(body).map_err(|e| eyre!("parse response: {e}"))?;
    let (Some(lat), Some(lon)) = (parsed.lat, parsed.lon) else {
        return Err(eyre!("response has no coordinates"));
    };
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err(eyre!("response coordinates out of range: {lat},{lon}"));
    }
    Ok(LocationFix::new(
        GeoPoint::new(lon, lat),
        Some(ASSUMED_ACCURACY_M),
        LocationSource::Ip,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lat_lon_keys() {
        let fix = parse(r#"{"lat":46.05,"lon":14.51}"#).unwrap();
        assert_eq!(fix.point.lat, 46.05);
        assert_eq!(fix.point.lon, 14.51);
        assert_eq!(fix.source, LocationSource::Ip);
    }

    #[test]
    fn parses_latitude_longitude_aliases() {
        let fix = parse(r#"{"latitude":46.05,"longitude":14.51}"#).unwrap();
        assert_eq!(fix.point.lat, 46.05);
        assert_eq!(fix.point.lon, 14.51);
    }

    #[test]
    fn reports_pessimistic_accuracy_so_platform_fixes_win() {
        let fix = parse(r#"{"lat":46.05,"lon":14.51}"#).unwrap();
        assert_eq!(fix.accuracy_m, Some(ASSUMED_ACCURACY_M));
    }

    #[test]
    fn rejects_response_without_coordinates() {
        assert!(parse(r#"{"error":"quota exceeded"}"#).is_err());
    }

    #[test]
    fn rejects_out_of_range_coordinates() {
        assert!(parse(r#"{"lat":460.0,"lon":14.51}"#).is_err());
    }

    #[test]
    fn rejects_non_json_body() {
        assert!(parse("<html>429 Too Many Requests</html>").is_err());
    }

    /// `run`'s backoff loop has no seam to drive without real sleeps, so this
    /// asserts the policy call it now delegates to (`POLICY.delay_for(failures)`,
    /// `failures` starting at 0) against today's hand-rolled sequence:
    /// 60, 120, 240, 480, 960, then clamps to 1800 (30 min).
    #[test]
    fn retry_sequence_matches_today() {
        let policy = RetryPolicy::new(RETRY_INTERVAL, MAX_RETRY_INTERVAL, None);
        let expected = [60u64, 120, 240, 480, 960, 1800, 1800];
        for (failures, secs) in expected.into_iter().enumerate() {
            assert_eq!(
                policy.delay_for(failures as u32),
                Duration::from_secs(secs),
                "failures {failures}"
            );
        }
    }
}
