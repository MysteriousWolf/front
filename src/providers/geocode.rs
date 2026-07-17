//! Place-name search via OpenStreetMap Nominatim.
//!
//! Backs the `/` prompt: type a place, get a point to pin on the map.
//!
//! Nominatim is a free, donated service with a strict usage policy
//! (<https://operations.osmfoundation.org/policies/nominatim/>): at most one
//! request per second and a `User-Agent` that identifies the application.
//! Both are enforced here rather than left to the caller — searches are
//! user-typed and rare, so the limit is never felt in practice.

use std::path::Path;
use std::time::{Duration, Instant};

use color_eyre::eyre::{eyre, Result};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::cache::write_log;
use crate::geo::GeoPoint;

/// Nominatim's usage policy allows at most 1 request per second.
const MIN_REQUEST_INTERVAL: Duration = Duration::from_secs(1);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Nominatim requires a User-Agent identifying the application.  A generic or
/// absent one gets the request blocked.
const USER_AGENT: &str = concat!(
    "front/",
    env!("CARGO_PKG_VERSION"),
    " (terminal weather radar; +https://github.com/MysteriousWolf/front)"
);

/// One geocoding result.
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    pub point: GeoPoint,
    /// Human-readable name, e.g. "Ljubljana, Upravna Enota Ljubljana, ...".
    pub display_name: String,
}

/// Nominatim returns lat/lon as *strings*, not numbers.
#[derive(Debug, Deserialize)]
struct NominatimResult {
    lat: String,
    lon: String,
    #[serde(default)]
    display_name: String,
}

#[derive(Debug)]
pub struct GeocodeProvider {
    client: reqwest::Client,
    endpoint: String,
    /// Timestamp of the last request, to honour the 1 req/s policy.
    last_request: Mutex<Option<Instant>>,
}

impl GeocodeProvider {
    pub fn new(endpoint: String) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .map_err(|e| eyre!("build geocoding HTTP client: {e}"))?;
        Ok(Self {
            client,
            endpoint,
            last_request: Mutex::new(None),
        })
    }

    /// Look up `query`, returning the best match.
    ///
    /// `Ok(None)` means the search ran but matched nothing — a normal outcome
    /// for a typo, distinct from the network/service errors in `Err`.
    pub async fn search(&self, query: &str, log_path: &Path) -> Result<Option<Place>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(None);
        }
        self.throttle().await;

        let resp = self
            .client
            .get(&self.endpoint)
            .query(&[
                ("q", query),
                ("format", "jsonv2"),
                ("limit", "1"),
                ("addressdetails", "0"),
            ])
            .send()
            .await
            .map_err(|e| eyre!("search request: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            write_log(log_path, format!("geocode: HTTP {status} for {query:?}"));
            return Err(eyre!("search failed: HTTP {status}"));
        }
        let body = resp.text().await.map_err(|e| eyre!("read response: {e}"))?;
        let place = parse(&body)?;
        write_log(
            log_path,
            match &place {
                Some(p) => format!("geocode: {query:?} -> {}", p.display_name),
                None => format!("geocode: {query:?} -> no match"),
            },
        );
        Ok(place)
    }

    /// Block until at least `MIN_REQUEST_INTERVAL` has passed since the last
    /// request, so bursts of searches cannot breach the usage policy.
    async fn throttle(&self) {
        let mut last = self.last_request.lock().await;
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed < MIN_REQUEST_INTERVAL {
                tokio::time::sleep(MIN_REQUEST_INTERVAL - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }
}

/// Parse a Nominatim `jsonv2` search response into the first usable place.
fn parse(body: &str) -> Result<Option<Place>> {
    let results: Vec<NominatimResult> =
        serde_json::from_str(body).map_err(|e| eyre!("parse search response: {e}"))?;
    let Some(first) = results.into_iter().next() else {
        return Ok(None);
    };
    let lat: f64 = first
        .lat
        .parse()
        .map_err(|_| eyre!("bad latitude {:?}", first.lat))?;
    let lon: f64 = first
        .lon
        .parse()
        .map_err(|_| eyre!("bad longitude {:?}", first.lon))?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err(eyre!("search result out of range: {lat},{lon}"));
    }
    Ok(Some(Place {
        point: GeoPoint::new(lon, lat),
        display_name: first.display_name,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A trimmed real response for "Ljubljana".
    const LJUBLJANA: &str = r#"[{"lat":"46.0500268","lon":"14.5069289",
        "display_name":"Ljubljana, Upravna Enota Ljubljana, 1000, Slovenija","type":"city"}]"#;

    #[test]
    fn parses_string_coordinates() {
        let place = parse(LJUBLJANA).unwrap().unwrap();
        assert!((place.point.lat - 46.0500268).abs() < 1e-9);
        assert!((place.point.lon - 14.5069289).abs() < 1e-9);
        assert!(place.display_name.starts_with("Ljubljana"));
    }

    #[test]
    fn empty_result_list_is_no_match_not_an_error() {
        assert_eq!(parse("[]").unwrap(), None);
    }

    #[test]
    fn takes_the_first_result() {
        let body = r#"[{"lat":"1.0","lon":"2.0","display_name":"first"},
                       {"lat":"3.0","lon":"4.0","display_name":"second"}]"#;
        assert_eq!(parse(body).unwrap().unwrap().display_name, "first");
    }

    #[test]
    fn missing_display_name_defaults_rather_than_failing() {
        let place = parse(r#"[{"lat":"1.0","lon":"2.0"}]"#).unwrap().unwrap();
        assert_eq!(place.display_name, "");
    }

    #[test]
    fn rejects_unparsable_coordinates() {
        assert!(parse(r#"[{"lat":"abc","lon":"2.0","display_name":"x"}]"#).is_err());
    }

    #[test]
    fn rejects_out_of_range_coordinates() {
        assert!(parse(r#"[{"lat":"460.0","lon":"2.0","display_name":"x"}]"#).is_err());
    }

    #[test]
    fn rejects_non_json_body() {
        assert!(parse("<html>429 Too Many Requests</html>").is_err());
    }

    #[test]
    fn user_agent_identifies_the_app() {
        assert!(USER_AGENT.starts_with("front/"));
        assert!(USER_AGENT.contains("github.com"));
    }
}
