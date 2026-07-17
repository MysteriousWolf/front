use clap::Parser;
use color_eyre::eyre::Result;

use front::app::App;
use front::cli::Cli;

/// Stop glibc from hoarding freed radar grids.
///
/// Grids are ~16.7 MB and are allocated and freed constantly as frames stream
/// in.  glibc raises its mmap threshold dynamically (up to 32 MB) when it sees
/// large blocks freed, after which allocations that size come from the heap
/// instead of mmap — and heap memory is never returned to the OS.  Measured on
/// a 3 h timeline that is the difference between 742 MB and 149 MB resident,
/// none of it memory the process is actually using.  Pinning the threshold
/// keeps grids on mmap, where free() releases them; capping arenas stops each
/// worker thread from holding its own pool.
#[cfg(target_env = "gnu")]
fn tune_allocator() {
    // SAFETY: plain libc calls with constant arguments, made before any
    // allocation-heavy work or extra threads exist.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 128 * 1024);
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
}

#[cfg(not(target_env = "gnu"))]
fn tune_allocator() {}

// The runtime is built by hand rather than via `#[tokio::main]` so
// `tune_allocator` runs first: `M_ARENA_MAX` only constrains arenas not yet
// created, and the macro spawns the worker threads before the body executes.
fn main() -> Result<()> {
    tune_allocator();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    let app = App::boot(&cli).await?;
    front::ui::run(app).await
}
