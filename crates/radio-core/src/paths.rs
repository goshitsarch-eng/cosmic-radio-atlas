//! XDG directory layout.
//!
//! Runtime and cache files live under `XDG_RUNTIME_DIR` / `XDG_CACHE_HOME`
//! in a `cosmic-radio-atlas` directory. Saved favorites, history, volume,
//! and output stay in `radio-atlas/state.json` under `XDG_DATA_HOME`, the
//! same file the legacy release used, so reinstalling keeps user data.

use std::path::PathBuf;

use crate::error::{Error, Result};

pub const RUNTIME_SUBDIR: &str = "cosmic-radio-atlas";
pub const STATE_SUBDIR: &str = "radio-atlas";

#[derive(Debug, Clone)]
pub struct Paths {
    pub runtime_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub country_cache_dir: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
    pub state_file: PathBuf,
    pub state_lock: PathBuf,
}

impl Paths {
    /// Resolve directories from the environment, creating them mode 0700.
    pub fn resolve() -> Result<Self> {
        let runtime_root = std::env::var_os("XDG_RUNTIME_DIR").ok_or(Error::MissingRuntimeDir)?;
        let cache_root = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(home_dir_subdir)
            .ok_or_else(|| Error::InvalidInput("Cannot resolve a cache directory".to_string()))?;
        let data_root = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(home_data_subdir)
            .ok_or_else(|| Error::InvalidInput("Cannot resolve a data directory".to_string()))?;
        Self::resolve_in(
            PathBuf::from(runtime_root).as_path(),
            cache_root.as_path(),
            data_root.as_path(),
        )
    }

    /// Resolve directories from explicit roots (used by tests).
    pub fn resolve_in(
        runtime_root: &std::path::Path,
        cache_root: &std::path::Path,
        data_root: &std::path::Path,
    ) -> Result<Self> {
        let paths = Paths {
            runtime_dir: runtime_root.join(RUNTIME_SUBDIR),
            cache_dir: cache_root.join(RUNTIME_SUBDIR),
            country_cache_dir: cache_root.join(RUNTIME_SUBDIR).join("countries"),
            data_dir: data_root.to_path_buf(),
            state_dir: PathBuf::new(),
            state_file: PathBuf::new(),
            state_lock: PathBuf::new(),
        };
        let state_dir = paths.data_dir.join(STATE_SUBDIR);
        let paths = Paths {
            state_file: state_dir.join("state.json"),
            state_lock: state_dir.join("state.lock"),
            state_dir,
            ..paths
        };
        paths.ensure_dirs()?;
        Ok(paths)
    }

    fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.runtime_dir,
            &self.cache_dir,
            &self.country_cache_dir,
            &self.state_dir,
        ] {
            std::fs::create_dir_all(dir)?;
            set_private_dir(dir)?;
        }
        Ok(())
    }

    pub fn world_file(&self) -> PathBuf {
        self.runtime_dir.join("world.json")
    }

    pub fn world_cache_file(&self) -> PathBuf {
        self.cache_dir.join("world.json")
    }

    pub fn world_lock_file(&self) -> PathBuf {
        self.cache_dir.join("world.lock")
    }

    pub fn results_file(&self) -> PathBuf {
        self.runtime_dir.join("results.json")
    }

    pub fn queue_file(&self) -> PathBuf {
        self.runtime_dir.join("playlist.json")
    }

    pub fn playlist_file(&self) -> PathBuf {
        self.runtime_dir.join("playlist.m3u")
    }

    pub fn play_selection_file(&self) -> PathBuf {
        self.runtime_dir.join("play-selection.json")
    }

    pub fn favorite_selection_file(&self) -> PathBuf {
        self.runtime_dir.join("favorite-selection.json")
    }

    pub fn status_file(&self) -> PathBuf {
        self.runtime_dir.join("status.json")
    }

    pub fn mpv_socket(&self) -> PathBuf {
        self.runtime_dir.join("mpv.sock")
    }

    pub fn proxy_socket(&self) -> PathBuf {
        self.runtime_dir.join("proxy.sock")
    }

    pub fn player_pid_file(&self) -> PathBuf {
        self.runtime_dir.join("player.pid")
    }

    pub fn player_lock_file(&self) -> PathBuf {
        self.runtime_dir.join("player.lock")
    }

    pub fn generation_file(&self) -> PathBuf {
        self.runtime_dir.join("player.generation")
    }

    pub fn mpv_log_file(&self) -> PathBuf {
        self.runtime_dir.join("mpv.log")
    }

    pub fn country_cache_file(&self, code: &str) -> PathBuf {
        self.country_cache_dir.join(format!("{code}.json"))
    }

    pub fn country_lock_file(&self, code: &str) -> PathBuf {
        self.country_cache_dir.join(format!("{code}.lock"))
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn home_dir_subdir() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".cache"))
}

fn home_data_subdir() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".local").join("share"))
}

#[cfg(unix)]
fn set_private_dir(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_dir(_dir: &std::path::Path) -> Result<()> {
    Ok(())
}

/// Atomically replace `target` with `contents`.
pub fn atomic_write(target: &std::path::Path, contents: &[u8]) -> Result<()> {
    let parent = target.parent().ok_or_else(|| {
        Error::InvalidInput("Refusing to write a file without a directory".to_string())
    })?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = parent.join(".radio-atlas-write");
    temporary.set_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&temporary, contents)?;
    std::fs::rename(&temporary, target)?;
    Ok(())
}

/// Atomically copy one file over another.
pub fn atomic_copy(source: &std::path::Path, target: &std::path::Path) -> Result<()> {
    let contents = std::fs::read(source)?;
    atomic_write(target, &contents)
}

#[cfg(test)]
pub(crate) fn test_paths_in(root: &std::path::Path) -> Paths {
    Paths::resolve_in(root, &root.join("cache"), &root.join("data")).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_temp_dirs() {
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths_in(root.path());
        assert!(paths.runtime_dir.is_dir());
        assert!(paths.country_cache_dir.is_dir());
        assert!(paths.state_dir.is_dir());
    }
}
