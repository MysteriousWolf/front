use clap::Parser;
use color_eyre::eyre::Result;

use front::app::App;
use front::cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    let app = App::boot(&cli).await?;
    front::ui::run(app).await
}
