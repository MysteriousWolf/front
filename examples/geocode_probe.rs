//! Manual probe for the `/` place search: runs real Nominatim queries and
//! prints what the prompt would pin.  Also exercises the rate limiter.
//!
//! Run with: `cargo run --example geocode_probe -- "Ljubljana" "Mount Fuji"`

use std::time::Instant;

use front::config::GeocodeConfig;
use front::providers::geocode::GeocodeProvider;

#[tokio::main]
async fn main() {
    let log = std::env::temp_dir().join("front-geocode-probe.log");
    let config = GeocodeConfig::default();
    println!("endpoint: {}\n", config.endpoint);

    let queries: Vec<String> = std::env::args().skip(1).collect();
    let queries = if queries.is_empty() {
        ["Ljubljana", "Bezigrad, Ljubljana", "Mount Fuji", "zzzzqqqq"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        queries
    };

    let provider = GeocodeProvider::new(config.endpoint).expect("build provider");
    let started = Instant::now();
    for q in &queries {
        let t = Instant::now();
        match provider.search(q, &log).await {
            Ok(Some(p)) => println!(
                "{:>24} -> lat={:9.4} lon={:9.4}  {}  ({:?})",
                q,
                p.point.lat,
                p.point.lon,
                p.display_name,
                t.elapsed()
            ),
            Ok(None) => println!("{q:>24} -> no match  ({:?})", t.elapsed()),
            Err(e) => println!("{q:>24} -> error: {e}"),
        }
    }
    println!(
        "\n{} queries in {:?} (policy: >= 1s apart)",
        queries.len(),
        started.elapsed()
    );
}
