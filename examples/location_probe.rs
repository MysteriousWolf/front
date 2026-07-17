//! Manual probe for the location backends: prints every fix as it arrives and
//! shows which one the arbiter picks.  Useful for checking a platform backend
//! on a machine where the TUI is not convenient to drive.
//!
//! Run with: `cargo run --example location_probe`

use std::time::Duration;

use front::config::LocationConfig;
use front::providers::location::{spawn, LocationArbiter};

#[tokio::main]
async fn main() {
    let log = std::env::temp_dir().join("front-location-probe.log");
    let config = LocationConfig::default();
    println!("ip_fallback={} endpoint={}", config.ip_fallback, config.ip_endpoint);
    println!("waiting 15s for fixes...\n");

    let mut stream = spawn(&config, &log);
    let mut arbiter = LocationArbiter::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);

    loop {
        match tokio::time::timeout_at(deadline, stream.rx.recv()).await {
            Ok(Some(fix)) => {
                let won = arbiter.offer(fix);
                println!(
                    "fix {:>28}  lat={:.4} lon={:.4}  -> {}",
                    fix.label(),
                    fix.point.lat,
                    fix.point.lon,
                    if won { "ACCEPTED" } else { "rejected" }
                );
            }
            Ok(None) => {
                println!("\nall backends exited");
                break;
            }
            Err(_) => break,
        }
    }

    match arbiter.current() {
        Some(fix) => println!(
            "\nfinal: {} at lat={:.4} lon={:.4}",
            fix.label(),
            fix.point.lat,
            fix.point.lon
        ),
        None => println!("\nfinal: no fix acquired"),
    }
    println!("log: {}", log.display());
}
