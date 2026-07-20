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

/// The `address` object from a reverse lookup with `addressdetails=1`.
///
/// Every field is optional and which one is populated depends entirely on how
/// the place is tagged in OSM: a capital is a `city`, a hamlet is a `hamlet`,
/// and a point in open countryside may have none of them.  We only want the
/// settlement, so [`Self::settlement`] walks them smallest-first.
#[derive(Debug, Default, Deserialize)]
struct NominatimAddress {
    #[serde(default)]
    city: Option<String>,
    #[serde(default)]
    town: Option<String>,
    #[serde(default)]
    village: Option<String>,
    #[serde(default)]
    hamlet: Option<String>,
    #[serde(default)]
    suburb: Option<String>,
    #[serde(default)]
    municipality: Option<String>,
    #[serde(default)]
    county: Option<String>,
}

impl NominatimAddress {
    /// The most specific settlement name available.
    ///
    /// Smallest populated place first: a response for Trzin carries both
    /// `village: Trzin` and `municipality: Trzin`, but one near a city can
    /// carry `city` too — and answering "Ljubljana" when you are standing in
    /// Trzin is exactly the regional-rollup this ordering exists to avoid.
    ///
    /// `suburb` is deliberately *below* `city`: inside Ljubljana the response
    /// carries `suburb: Četrtna skupnost Šiška`, and the answer a person wants
    /// there is "Ljubljana", not the administrative quarter. `county` stays
    /// last, for points with no settlement at all.
    fn settlement(&self) -> Option<&str> {
        [
            &self.hamlet,
            &self.village,
            &self.town,
            &self.municipality,
            &self.city,
            &self.suburb,
            &self.county,
        ]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .find(|s| !s.trim().is_empty())
    }
}

/// A reverse-geocoding response.  `address` is absent on an error payload.
#[derive(Debug, Deserialize)]
struct ReverseResult {
    #[serde(default)]
    address: NominatimAddress,
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

    /// Name the settlement containing `point`, for labelling a map pin.
    ///
    /// `Ok(None)` means the lookup succeeded but the point has no settlement
    /// worth naming — mid-ocean, or open country with nothing tagged nearby.
    /// The caller then leaves the pin unlabelled rather than inventing a name.
    ///
    /// `zoom=13` is the village/suburb level.  The default (18) resolves to a
    /// building, far longer than a terminal row can spare; but 10 (city) is too
    /// coarse in the other direction — it returns only `municipality`, dropping
    /// the `village` field that names a small town precisely.
    pub async fn reverse(&self, point: GeoPoint, log_path: &Path) -> Result<Option<String>> {
        self.throttle().await;

        let resp = self
            .client
            .get(self.reverse_endpoint())
            .query(&[
                ("lat", point.lat.to_string().as_str()),
                ("lon", point.lon.to_string().as_str()),
                ("format", "jsonv2"),
                ("zoom", "13"),
                ("addressdetails", "1"),
            ])
            .send()
            .await
            .map_err(|e| eyre!("reverse request: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            write_log(log_path, format!("geocode: reverse HTTP {status}"));
            return Err(eyre!("reverse failed: HTTP {status}"));
        }
        let body = resp.text().await.map_err(|e| eyre!("read response: {e}"))?;
        let name = parse_reverse(&body)?;
        write_log(
            log_path,
            match &name {
                Some(n) => format!("geocode: reverse {:.4},{:.4} -> {n}", point.lat, point.lon),
                None => format!(
                    "geocode: reverse {:.4},{:.4} -> unnamed",
                    point.lat, point.lon
                ),
            },
        );
        Ok(name)
    }

    /// Nominatim exposes `/reverse` alongside `/search`, so derive one from the
    /// other.  Keeps `config.toml` to a single endpoint key and means a custom
    /// instance only has to be configured once.
    fn reverse_endpoint(&self) -> String {
        match self.endpoint.rsplit_once("/search") {
            Some((base, rest)) => format!("{base}/reverse{rest}"),
            None => self.endpoint.clone(),
        }
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

/// Extract the settlement name from a Nominatim `reverse` response.
///
/// Falls back to the first comma-separated component of `display_name` when
/// the address object has no settlement field: that component is the place
/// name itself, which is still a better label than nothing.
fn parse_reverse(body: &str) -> Result<Option<String>> {
    let result: ReverseResult =
        serde_json::from_str(body).map_err(|e| eyre!("parse reverse response: {e}"))?;
    if let Some(name) = result.address.settlement() {
        return Ok(Some(name.to_string()));
    }
    let head = result
        .display_name
        .split(',')
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    Ok(head.map(str::to_string))
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

    // ── reverse geocoding ──────────────────────────────────────────────

    #[test]
    fn reverse_prefers_the_city_field() {
        let body = r#"{"display_name":"Some Street, Ljubljana, Slovenija",
            "address":{"road":"Some Street","city":"Ljubljana","country":"Slovenija"}}"#;
        assert_eq!(parse_reverse(body).unwrap().as_deref(), Some("Ljubljana"));
    }

    /// Smaller places are tagged `town`/`village`/`hamlet` rather than `city`;
    /// the label must still name them instead of falling through to the county.
    #[test]
    fn reverse_falls_through_to_smaller_settlement_types() {
        for (field, want) in [("town", "Bled"), ("village", "Bled"), ("hamlet", "Bled")] {
            let body = format!(
                r#"{{"display_name":"x","address":{{"{field}":"Bled","county":"Radovljica"}}}}"#
            );
            assert_eq!(parse_reverse(&body).unwrap().as_deref(), Some(want));
        }
    }

    /// The regression this ordering exists for: standing in Trzin, a village
    /// next to Ljubljana, must answer "Trzin" and not roll up to the city.
    #[test]
    fn reverse_prefers_the_village_over_a_neighbouring_city() {
        let body = r#"{"display_name":"Trzin, Upravna Enota Domžale, 1236, Slovenija",
            "address":{"village":"Trzin","municipality":"Trzin","city":"Ljubljana"}}"#;
        assert_eq!(parse_reverse(body).unwrap().as_deref(), Some("Trzin"));
    }

    /// A real zoom=13 response for Trzin, which carries no `city` at all.
    #[test]
    fn reverse_names_trzin_from_its_real_response() {
        let body = r#"{"display_name":"Trzin, Upravna Enota Domžale, 1236, Slovenija",
            "address":{"village":"Trzin","municipality":"Trzin","postcode":"1236",
            "country":"Slovenija","country_code":"si"}}"#;
        assert_eq!(parse_reverse(body).unwrap().as_deref(), Some("Trzin"));
    }

    /// A larger settlement wins over a suburb inside it: "Ljubljana" is the
    /// answer to "where am I", not the administrative quarter.  This is the
    /// real zoom=13 response for central Ljubljana.
    #[test]
    fn reverse_names_ljubljana_not_its_quarter() {
        let body = r#"{"display_name":"x","address":{"suburb":"Četrtna skupnost Center",
            "city":"Ljubljana","municipality":"Ljubljana"}}"#;
        assert_eq!(parse_reverse(body).unwrap().as_deref(), Some("Ljubljana"));
    }

    /// A larger settlement wins over a suburb inside it: "Ljubljana" is the
    /// answer to "where am I", not the neighbourhood name.
    #[test]
    fn reverse_prefers_the_city_over_its_suburb() {
        let body = r#"{"display_name":"x","address":{"suburb":"Å iÅ¡ka","city":"Ljubljana"}}"#;
        assert_eq!(parse_reverse(body).unwrap().as_deref(), Some("Ljubljana"));
    }

    /// Open countryside has no settlement fields at all; the first component of
    /// display_name is still a usable label.
    #[test]
    fn reverse_falls_back_to_the_display_name_head() {
        let body = r#"{"display_name":"Triglav, Slovenija","address":{}}"#;
        assert_eq!(parse_reverse(body).unwrap().as_deref(), Some("Triglav"));
    }

    #[test]
    fn reverse_with_nothing_usable_is_none_not_an_error() {
        let body = r#"{"display_name":"","address":{}}"#;
        assert_eq!(parse_reverse(body).unwrap(), None);
    }

    /// Blank fields must not win over a populated one further down the list.
    #[test]
    fn reverse_skips_empty_settlement_fields() {
        let body = r#"{"display_name":"x","address":{"city":"  ","town":"Kranj"}}"#;
        assert_eq!(parse_reverse(body).unwrap().as_deref(), Some("Kranj"));
    }

    #[test]
    fn reverse_rejects_non_json_body() {
        assert!(parse_reverse("<html>429 Too Many Requests</html>").is_err());
    }

    /// The reverse endpoint is derived from the configured search endpoint so
    /// a custom Nominatim instance only has to be set once.
    #[test]
    fn reverse_endpoint_is_derived_from_the_search_endpoint() {
        let p =
            GeocodeProvider::new("https://nominatim.openstreetmap.org/search".to_string()).unwrap();
        assert_eq!(
            p.reverse_endpoint(),
            "https://nominatim.openstreetmap.org/reverse"
        );
    }

    #[test]
    fn user_agent_identifies_the_app() {
        assert!(USER_AGENT.starts_with("front/"));
        assert!(USER_AGENT.contains("github.com"));
    }
}
