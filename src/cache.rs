use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Context, Result};
use directories::ProjectDirs;

#[derive(Debug, Clone)]
pub struct FrontDirs {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub maps_dir: PathBuf,
    pub radar_dir: PathBuf,
    pub log_path: PathBuf,
}

impl FrontDirs {
    pub fn new() -> Result<Self> {
        let project = ProjectDirs::from("", "", "front")
            .ok_or_else(|| color_eyre::eyre::eyre!("could not resolve XDG project directories"))?;
        let config_dir = project.config_dir().to_path_buf();
        let cache_dir = project.cache_dir().to_path_buf();
        let maps_dir = cache_dir.join("maps");
        let radar_dir = cache_dir.join("radar");
        let log_path = cache_dir.join("front.log");
        fs::create_dir_all(&config_dir).wrap_err("create config directory")?;
        fs::create_dir_all(&maps_dir).wrap_err("create map cache directory")?;
        fs::create_dir_all(&radar_dir).wrap_err("create radar cache directory")?;
        Ok(Self {
            config_dir,
            cache_dir,
            maps_dir,
            radar_dir,
            log_path,
        })
    }

    pub fn clear_cache(&self) -> Result<()> {
        if self.cache_dir.exists() {
            fs::remove_dir_all(&self.cache_dir).wrap_err("clear cache directory")?;
        }
        fs::create_dir_all(&self.maps_dir).wrap_err("recreate map cache directory")?;
        fs::create_dir_all(&self.radar_dir).wrap_err("recreate radar cache directory")?;
        Ok(())
    }

    /// Remove cached radar GeoTIFFs older than `max_age`.  Radar frames
    /// arrive every 5 minutes and are only browsable for the last hour,
    /// so without pruning the cache grows by hundreds of MB per day.
    pub fn prune_radar_cache(&self, max_age: std::time::Duration) {
        let now = std::time::SystemTime::now();
        let mut stack = vec![self.radar_dir.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let stale = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|mtime| now.duration_since(mtime).ok())
                    .is_some_and(|age| age > max_age);
                if stale {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }

    pub fn clear_map_cache(&self) -> Result<()> {
        if self.maps_dir.exists() {
            fs::remove_dir_all(&self.maps_dir).wrap_err("clear map cache directory")?;
        }
        fs::create_dir_all(&self.maps_dir).wrap_err("recreate map cache directory")?;
        Ok(())
    }
}

pub fn read_if_exists(path: &Path) -> Result<Option<Vec<u8>>> {
    if path.exists() {
        Ok(Some(
            fs::read(path).wrap_err_with(|| format!("read {}", path.display()))?,
        ))
    } else {
        Ok(None)
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).wrap_err_with(|| format!("create {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).wrap_err_with(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .wrap_err_with(|| format!("rename {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Append a timestamped line to the debug log file.
/// Safe to call while the terminal is in raw mode (writes to file, not stderr).
pub fn write_log(path: &Path, msg: impl std::fmt::Display) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let ts = chrono::Local::now().format("%H:%M:%S%.3f");
        let _ = writeln!(file, "[{ts}] {msg}");
        let _ = file.flush();
    }
}
