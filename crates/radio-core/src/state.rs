//! Saved favorites, listening history, volume, and audio output.
//!
//! Ports `radio-state`: the JSON document is validated before every read and
//! every write, and an invalid file is never overwritten, so a corrupt
//! `state.json` fails loudly instead of destroying user data.

use fs2::FileExt;

use crate::error::{Error, Result};
use crate::model::Station;
use crate::paths::{Paths, atomic_write};

pub const MAX_FAVORITES: usize = 500;
pub const MAX_RECENT: usize = 30;
pub const MAX_JSON_BYTES: u64 = 4_194_304;
pub const DEFAULT_VOLUME: i64 = 70;

#[derive(Debug, Clone)]
pub struct SavedState {
    pub favorites: Vec<Station>,
    pub recent: Vec<Station>,
    pub volume: i64,
    pub output: String,
}

impl Default for SavedState {
    fn default() -> Self {
        SavedState {
            favorites: Vec::new(),
            recent: Vec::new(),
            volume: DEFAULT_VOLUME,
            output: String::new(),
        }
    }
}

pub fn is_valid_output_name(output: &str) -> bool {
    output.len() <= 160
        && output
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'+' | b'-'))
}

fn is_valid_uuid(uuid: &str) -> bool {
    uuid.len() >= 20 && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

#[derive(Debug, serde::Deserialize)]
struct RawState {
    #[serde(default)]
    favorites: Option<Vec<Station>>,
    #[serde(default)]
    recent: Option<Vec<Station>>,
    #[serde(default)]
    volume: Option<serde_json::Value>,
    #[serde(default)]
    output: Option<serde_json::Value>,
}

fn within_limit(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.len() <= MAX_JSON_BYTES)
        .unwrap_or(false)
}

/// Read and validate the saved document.
///
/// A missing file is created with defaults. An invalid file is an error and
/// is never overwritten.
pub fn load_state(paths: &Paths) -> Result<SavedState> {
    let _lock = lock_state(paths)?;
    load_state_locked(paths)
}

fn load_state_locked(paths: &Paths) -> Result<SavedState> {
    if !paths.state_file.exists() {
        let default = SavedState::default();
        write_validated(paths, &default)?;
        return Ok(default);
    }
    parse_state_file(&paths.state_file)
}

fn parse_state_file(path: &std::path::Path) -> Result<SavedState> {
    if !within_limit(path) {
        return Err(refusing(path));
    }
    let bytes = std::fs::read(path)?;
    // `from_slice` rejects trailing bytes, enforcing the single-document rule.
    let raw: RawState = serde_json::from_slice(&bytes).map_err(|_| refusing(path))?;
    let favorites = raw.favorites.ok_or_else(|| refusing(path))?;
    let recent = raw.recent.ok_or_else(|| refusing(path))?;
    if favorites.len() > MAX_FAVORITES || recent.len() > MAX_RECENT {
        return Err(refusing(path));
    }
    let volume = match raw.volume {
        None => DEFAULT_VOLUME,
        Some(value) => match value {
            serde_json::Value::Number(number) => number
                .as_f64()
                .filter(|v| (0.0..=100.0).contains(v))
                .map(|v| v.round() as i64)
                .ok_or_else(|| refusing(path))?,
            _ => return Err(refusing(path)),
        },
    };
    let output = match raw.output {
        None => String::new(),
        Some(serde_json::Value::String(text)) if is_valid_output_name(&text) => text,
        _ => return Err(refusing(path)),
    };
    Ok(SavedState {
        favorites,
        recent,
        volume,
        output,
    })
}

fn refusing(path: &std::path::Path) -> Error {
    Error::InvalidState(format!(
        "Radio Atlas saved state is invalid; refusing to overwrite {}",
        path.display()
    ))
}

fn write_validated(paths: &Paths, state: &SavedState) -> Result<()> {
    if state.favorites.len() > MAX_FAVORITES
        || state.recent.len() > MAX_RECENT
        || !(0..=100).contains(&state.volume)
        || !is_valid_output_name(&state.output)
    {
        return Err(Error::InvalidState(
            "Radio Atlas could not save this station safely".to_string(),
        ));
    }
    let document = serde_json::json!({
        "favorites": state.favorites,
        "recent": state.recent,
        "volume": state.volume,
        "output": state.output,
    });
    let bytes = serde_json::to_vec(&document).map_err(Error::Json)?;
    atomic_write(&paths.state_file, &bytes)?;
    // Re-validate what was written, mirroring the shell's post-write check.
    parse_state_file(&paths.state_file)?;
    Ok(())
}

fn lock_state(paths: &Paths) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&paths.state_lock)?;
    file.lock_exclusive()?;
    Ok(file)
}

/// Reset runtime station files that are missing, oversized, or malformed.
fn ensure_runtime_array(path: &std::path::Path) {
    let valid = within_limit(path)
        && std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<serde_json::Value>>(&bytes).ok())
            .map(|rows| rows.len() <= MAX_FAVORITES)
            .unwrap_or(false);
    if !valid {
        let _ = atomic_write(path, b"[]");
    }
}

fn ensure_runtime_data(paths: &Paths) {
    ensure_runtime_array(&paths.results_file());
    ensure_runtime_array(&paths.world_file());
    ensure_runtime_array(&paths.queue_file());
    ensure_runtime_array(&paths.favorite_selection_file());
}

fn find_in_file(path: &std::path::Path, uuid: &str) -> Option<Station> {
    let bytes = std::fs::read(path).ok()?;
    let rows: Vec<Station> = serde_json::from_slice(&bytes).ok()?;
    rows.into_iter().find(|station| station.uuid == uuid)
}

fn find_station(paths: &Paths, state: &SavedState, uuid: &str) -> Option<Station> {
    for path in [
        paths.favorite_selection_file(),
        paths.queue_file(),
        paths.results_file(),
        paths.world_file(),
    ] {
        if let Some(station) = find_in_file(&path, uuid) {
            return Some(station);
        }
    }
    state
        .favorites
        .iter()
        .chain(state.recent.iter())
        .find(|station| station.uuid == uuid)
        .cloned()
}

/// Toggle a station's favorite status.
pub fn toggle_favorite(paths: &Paths, uuid: &str) -> Result<SavedState> {
    if !is_valid_uuid(uuid) {
        return Err(Error::InvalidInput("Invalid station id".to_string()));
    }
    let _lock = lock_state(paths)?;
    ensure_runtime_data(paths);
    let mut state = load_state_locked(paths)?;
    let station = find_station(paths, &state, uuid)
        .ok_or_else(|| Error::Unavailable("Station is not in the current data set".to_string()))?;
    if state.favorites.iter().any(|row| row.uuid == station.uuid) {
        state.favorites.retain(|row| row.uuid != station.uuid);
    } else {
        state.favorites.retain(|row| row.uuid != station.uuid);
        state.favorites.insert(0, station);
        state.favorites.truncate(MAX_FAVORITES);
    }
    write_validated(paths, &state)?;
    Ok(state)
}

/// Record a station at the head of the listening history.
pub fn record_played(paths: &Paths, uuid: &str) -> Result<SavedState> {
    if !is_valid_uuid(uuid) {
        return Err(Error::InvalidInput("Invalid station id".to_string()));
    }
    let _lock = lock_state(paths)?;
    ensure_runtime_data(paths);
    let mut state = load_state_locked(paths)?;
    let station = find_station(paths, &state, uuid)
        .ok_or_else(|| Error::Unavailable("Station is not in the current data set".to_string()))?;
    state.recent.retain(|row| row.uuid != station.uuid);
    state.recent.insert(0, station);
    state.recent.truncate(MAX_RECENT);
    write_validated(paths, &state)?;
    Ok(state)
}

/// Persist the radio volume without touching playback.
pub fn set_volume(paths: &Paths, volume: i64) -> Result<SavedState> {
    if !(0..=100).contains(&volume) {
        return Err(Error::InvalidInput(
            "Volume must be an integer from 0 to 100".to_string(),
        ));
    }
    let _lock = lock_state(paths)?;
    let mut state = load_state_locked(paths)?;
    state.volume = volume;
    write_validated(paths, &state)?;
    Ok(state)
}

/// Persist the chosen audio output sink name (`""` means system default).
pub fn set_output(paths: &Paths, output: &str) -> Result<SavedState> {
    if !is_valid_output_name(output) {
        return Err(Error::InvalidInput("Invalid audio output".to_string()));
    }
    let _lock = lock_state(paths)?;
    let mut state = load_state_locked(paths)?;
    state.output = output.to_string();
    write_validated(paths, &state)?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::test_paths_in;

    fn test_paths() -> (tempfile::TempDir, Paths) {
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths_in(root.path());
        (root, paths)
    }

    fn seed_results(paths: &Paths) {
        let fixture = r#"[{"uuid":"12345678-1234-1234-1234-123456789abc","name":"Test Radio","url":"https://example.com/radio.mp3","countryCode":"US","latitude":40,"longitude":-74}]"#;
        std::fs::write(paths.results_file(), fixture).unwrap();
    }

    #[test]
    fn round_trips_favorites_history_volume_and_output() {
        let (_root, paths) = test_paths();
        let state = load_state(&paths).unwrap();
        assert_eq!(state.volume, 70);

        seed_results(&paths);
        let uuid = "12345678-1234-1234-1234-123456789abc";
        let state = toggle_favorite(&paths, uuid).unwrap();
        assert_eq!(state.favorites.len(), 1);
        let state = record_played(&paths, uuid).unwrap();
        assert_eq!(state.recent.len(), 1);
        let state = set_volume(&paths, 42).unwrap();
        assert_eq!(state.volume, 42);
        let state = set_output(&paths, "alsa_output.test").unwrap();
        assert_eq!(state.output, "alsa_output.test");
        let state = toggle_favorite(&paths, uuid).unwrap();
        assert!(state.favorites.is_empty());
    }

    #[test]
    fn refuses_to_overwrite_invalid_state() {
        let (_root, paths) = test_paths();
        std::fs::write(&paths.state_file, b"{\"favorites\":\"bad\"}").unwrap();
        assert!(load_state(&paths).is_err());
        assert!(set_volume(&paths, 10).is_err());
        assert_eq!(
            std::fs::read(&paths.state_file).unwrap(),
            b"{\"favorites\":\"bad\"}"
        );
    }

    #[test]
    fn rejects_bad_inputs() {
        let (_root, paths) = test_paths();
        assert!(toggle_favorite(&paths, "short").is_err());
        assert!(set_volume(&paths, 101).is_err());
        assert!(set_output(&paths, "bad sink!").is_err());
    }
}
