use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "front",
    about = "Fancy Radar ObservatioN Tool - a terminal weather radar map"
)]
pub struct Cli {
    #[arg(long)]
    pub lat: Option<f64>,

    #[arg(long)]
    pub lon: Option<f64>,

    #[arg(long)]
    pub zoom: Option<f64>,

    /// Disable every location source: no OS lookup, no IP fallback.
    #[arg(long)]
    pub no_location: bool,

    #[arg(long)]
    pub clear_cache: bool,
}
