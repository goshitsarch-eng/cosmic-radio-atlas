//! Playback orchestration: mpv sessions, queues, volume, and outputs.
//!
//! Ports `radio-player` action for action: queue files are prepared before
//! the player lock is taken so a slow directory lookup never blocks `stop`,
//! a generation counter cancels stale `play` calls, and every IPC failure
//! refreshes the status file before reporting an error.

use std::os::unix::fs::FileTypeExt;
use std::time::{Duration, Instant};

use fs2::FileExt;
use radio_core::client::Client;
use radio_core::error::{Error, Result};
use radio_core::model::Station;
use radio_core::paths::{Paths, atomic_write};
use radio_core::state;
use serde::{Deserialize, Serialize};

use crate::ipc::{self, MAX_IPC_BYTES};
use crate::outputs::AudioOutput;
use crate::process;

const MAX_QUEUE_FILE_BYTES: u64 = 4_194_304;
const MAX_QUEUE_ROWS: usize = 500;
const CLICK_COUNT_URL: &str = "https://all.api.radio-browser.info/json/url";

/// Station identity subset published in the status file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StationSummary {
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub country: String,
    #[serde(default)]
    pub country_code: String,
    #[serde(default)]
    pub latitude: Option<f64>,
    #[serde(default)]
    pub longitude: Option<f64>,
}

/// Player status snapshot; serialized field names match `status.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerStatus {
    pub running: bool,
    pub paused: bool,
    pub muted: bool,
    pub title: String,
    pub playlist_position: i64,
    pub playlist_count: i64,
    pub volume: i64,
    pub output: String,
    pub loaded: bool,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub error_detail: String,
    pub station: StationSummary,
}

/// Where `play` loads its queue from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayScope {
    World,
    Favorites,
    Recent,
    Results,
    Selection,
}

impl PlayScope {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "world" => Some(PlayScope::World),
            "favorites" => Some(PlayScope::Favorites),
            "recent" => Some(PlayScope::Recent),
            "results" => Some(PlayScope::Results),
            "selection" => Some(PlayScope::Selection),
            _ => None,
        }
    }
}

pub struct Player {
    paths: Paths,
    client: Client,
    mpv_program: String,
}

impl Player {
    pub fn new() -> Result<Self> {
        Self::with_paths(Paths::resolve()?)
    }

    pub fn with_paths(paths: Paths) -> Result<Self> {
        Ok(Player {
            paths,
            client: Client::new()?,
            mpv_program: process::mpv_program(),
        })
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Override the mpv binary (used by tests with a fake player).
    pub fn set_mpv_program(&mut self, program: String) {
        self.mpv_program = program;
    }

    fn lock(&self) -> Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(self.paths.player_lock_file())?;
        file.lock_exclusive()?;
        Ok(file)
    }

    fn configured_volume(&self) -> i64 {
        state::load_state(&self.paths)
            .map(|state| state.volume)
            .unwrap_or(state::DEFAULT_VOLUME)
    }

    fn configured_output(&self) -> String {
        state::load_state(&self.paths)
            .map(|state| state.output)
            .unwrap_or_default()
    }

    // --- Status -----------------------------------------------------------

    /// Query mpv (or the stopped fallback) and publish `status.json`.
    pub fn status(&self) -> Result<PlayerStatus> {
        let _lock = self.lock()?;
        self.status_locked()
    }

    fn status_locked(&self) -> Result<PlayerStatus> {
        if !is_socket(&self.paths.mpv_socket()) {
            process::cleanup_dead_player(&self.paths);
            return self.write_status(self.stopped_status());
        }
        let messages = match ipc::query_status_properties(&self.paths.mpv_socket()) {
            Ok(messages) => messages,
            Err(_) => {
                process::cleanup_dead_player(&self.paths);
                return self.write_status(self.stopped_status());
            }
        };

        let failure = parse_failure(ipc::property(&messages, ipc::PROP_FAILURE));
        let mut position = failure.as_ref().map(|f| f.position).or_else(|| {
            ipc::property(&messages, ipc::PROP_POSITION).and_then(|value| value.as_i64())
        });
        if position.is_none() {
            // mpv reports -1 or null when idle; keep it numeric.
            position = Some(-1);
        }
        let position = position.unwrap_or(-1);

        let station = if position >= 0 {
            self.queue_station(position as usize).unwrap_or_default()
        } else {
            StationSummary::default()
        };

        let mut loaded = false;
        if failure.is_none() && !station.uuid.is_empty() {
            loaded = self.previous_loaded_uuid().as_deref() == Some(station.uuid.as_str());
        }

        let status = PlayerStatus {
            running: true,
            paused: failure.is_some()
                || ipc::property(&messages, ipc::PROP_PAUSE)
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
            muted: ipc::property(&messages, ipc::PROP_MUTE)
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            title: single_line(
                ipc::property(&messages, ipc::PROP_TITLE)
                    .and_then(|value| value.as_str())
                    .unwrap_or(""),
                512,
            ),
            playlist_position: position,
            playlist_count: ipc::property(&messages, ipc::PROP_COUNT)
                .and_then(|value| value.as_i64())
                .unwrap_or(0),
            volume: ipc::property(&messages, ipc::PROP_VOLUME)
                .and_then(|value| value.as_f64())
                .map(|volume| volume.round() as i64)
                .unwrap_or(state::DEFAULT_VOLUME),
            output: clean_output(
                ipc::property(&messages, ipc::PROP_DEVICE)
                    .and_then(|value| value.as_str())
                    .unwrap_or(""),
            ),
            loaded,
            error: failure
                .as_ref()
                .map(|f| f.message.clone())
                .unwrap_or_default(),
            error_detail: failure
                .as_ref()
                .map(|f| f.detail.clone())
                .unwrap_or_default(),
            station,
        };
        self.write_status(status)
    }

    fn stopped_status(&self) -> PlayerStatus {
        PlayerStatus {
            running: false,
            paused: false,
            muted: false,
            title: String::new(),
            playlist_position: -1,
            playlist_count: 0,
            volume: self.configured_volume(),
            output: self.configured_output(),
            loaded: false,
            error: String::new(),
            error_detail: String::new(),
            station: StationSummary::default(),
        }
    }

    fn write_status(&self, status: PlayerStatus) -> Result<PlayerStatus> {
        let mut bytes = serde_json::to_vec(&status).map_err(Error::Json)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_IPC_BYTES {
            return Err(Error::Unavailable(
                "Radio player returned an oversized status".to_string(),
            ));
        }
        atomic_write(self.paths.status_file().as_path(), &bytes)?;
        Ok(status)
    }

    fn queue_station(&self, position: usize) -> Option<StationSummary> {
        let rows = read_runtime_array(self.paths.queue_file().as_path())?;
        let row = rows.get(position)?;
        Some(StationSummary {
            uuid: truncate(row.uuid.as_str(), 64),
            name: truncate(row.name.as_str(), 160),
            country: truncate(row.country.as_str(), 100),
            country_code: truncate(row.country_code.as_str(), 2),
            latitude: row.latitude,
            longitude: row.longitude,
        })
    }

    fn previous_loaded_uuid(&self) -> Option<String> {
        let bytes = std::fs::read(self.paths.status_file()).ok()?;
        if bytes.len() > MAX_IPC_BYTES {
            return None;
        }
        let status: PlayerStatus = serde_json::from_slice(&bytes).ok()?;
        if status.loaded && !status.station.uuid.is_empty() {
            Some(status.station.uuid)
        } else {
            None
        }
    }

    // --- Transport ----------------------------------------------------------

    fn run_command(&self, payload: serde_json::Value) -> Result<PlayerStatus> {
        let _lock = self.lock()?;
        if ipc::send_command(&self.paths.mpv_socket(), payload).is_err() {
            let _ = self.status_locked();
            return Err(Error::Unavailable(
                "Radio player is unavailable".to_string(),
            ));
        }
        self.status_locked()
    }

    /// Play or pause; on a failed stream this retries the failed position.
    pub fn toggle(&self) -> Result<PlayerStatus> {
        self.run_command(serde_json::json!({"command":["script-message","radio-atlas-toggle"]}))
    }

    pub fn next(&self) -> Result<PlayerStatus> {
        self.run_command(serde_json::json!({"command":["playlist-next","force"]}))
    }

    pub fn previous(&self) -> Result<PlayerStatus> {
        self.run_command(serde_json::json!({"command":["playlist-prev","force"]}))
    }

    pub fn mute(&self) -> Result<PlayerStatus> {
        self.run_command(serde_json::json!({"command":["cycle","mute"]}))
    }

    /// Stop playback and tear down the mpv session.
    pub fn stop(&self) -> Result<PlayerStatus> {
        let _lock = self.lock()?;
        process::cancel_pending_plays(&self.paths)?;
        if !is_socket(&self.paths.mpv_socket()) {
            if !process::terminate_tracked_player(&self.paths) {
                return Err(Error::Unavailable(
                    "Radio player could not be stopped".to_string(),
                ));
            }
            return self.status_locked();
        }
        if ipc::send_command(
            &self.paths.mpv_socket(),
            serde_json::json!({"command":["quit"]}),
        )
        .is_err()
        {
            if !process::terminate_tracked_player(&self.paths) {
                return Err(Error::Unavailable(
                    "Radio player could not be stopped".to_string(),
                ));
            }
            return self.status_locked();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !is_socket(&self.paths.mpv_socket()) && !process::player_alive(&self.paths) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if (is_socket(&self.paths.mpv_socket()) || process::player_alive(&self.paths))
            && !process::terminate_tracked_player(&self.paths)
        {
            return Err(Error::Unavailable(
                "Radio player could not be stopped".to_string(),
            ));
        }
        self.status_locked()
    }

    // --- Volume and outputs ---------------------------------------------------

    pub fn set_volume(&self, volume: i64) -> Result<PlayerStatus> {
        if !(0..=100).contains(&volume) {
            return Err(Error::InvalidInput(
                "Volume must be an integer from 0 to 100".to_string(),
            ));
        }
        let _lock = self.lock()?;
        // Validate saved state before touching anything.
        state::load_state(&self.paths)?;
        if is_socket(&self.paths.mpv_socket()) {
            let command = serde_json::json!({"command":["set_property","volume",volume]});
            if ipc::send_command(&self.paths.mpv_socket(), command).is_err() {
                let _ = self.status_locked();
                if is_socket(&self.paths.mpv_socket()) {
                    return Err(Error::Unavailable(
                        "Radio volume could not be changed".to_string(),
                    ));
                }
            }
        }
        state::set_volume(&self.paths, volume)?;
        self.status_locked()
    }

    pub fn adjust_volume(&self, delta: i64) -> Result<PlayerStatus> {
        let current = self.status()?.volume;
        self.set_volume((current + delta).clamp(0, 100))
    }

    pub fn list_outputs(&self) -> Result<Vec<AudioOutput>> {
        crate::outputs::list_outputs()
    }

    /// Select an output sink (`""` restores the system default).
    pub fn set_output(&self, requested: &str) -> Result<PlayerStatus> {
        if !requested.is_empty() {
            if !state::is_valid_output_name(requested) {
                return Err(Error::InvalidInput("Unknown audio output".to_string()));
            }
            let available = crate::outputs::list_outputs()?;
            if !available.iter().any(|output| output.id == requested) {
                return Err(Error::Unavailable("Unknown audio output".to_string()));
            }
        }
        let _lock = self.lock()?;
        state::load_state(&self.paths)?;
        if is_socket(&self.paths.mpv_socket()) {
            let device = if requested.is_empty() {
                "auto".to_string()
            } else {
                format!("pipewire/{requested}")
            };
            let command = serde_json::json!({"command":["set_property","audio-device",device]});
            if ipc::send_command(&self.paths.mpv_socket(), command).is_err() {
                let _ = self.status_locked();
                if is_socket(&self.paths.mpv_socket()) {
                    return Err(Error::Unavailable(
                        "Audio output could not be changed".to_string(),
                    ));
                }
            }
        }
        state::set_output(&self.paths, requested)?;
        self.status_locked()
    }

    // --- Play -----------------------------------------------------------------

    /// Play a station, rotating its scope queue so playback starts there.
    pub fn play(&self, uuid: &str, scope: PlayScope) -> Result<PlayerStatus> {
        let starting_generation = process::player_generation(&self.paths);
        let mut source = self.source_stations(scope)?;
        if matches!(scope, PlayScope::Favorites | PlayScope::Recent) {
            self.refresh_saved_stations(&mut source);
        }
        if !source
            .iter()
            .any(|station| station.uuid == uuid && is_playable(station))
        {
            return Err(Error::Unavailable(
                "Station is unavailable or has an unsafe stream URL".to_string(),
            ));
        }
        let queue = rotate_queue(&source, uuid);
        let queue_bytes = serde_json::to_vec(&queue).map_err(Error::Json)?;
        let playlist_text = build_playlist(&queue);
        let temp_queue = temp_runtime_file(&self.paths, "queue")?;
        let temp_playlist = temp_runtime_file(&self.paths, "playlist")?;
        std::fs::write(&temp_queue, queue_bytes)?;
        std::fs::write(&temp_playlist, playlist_text)?;

        let _lock = self.lock()?;
        if process::player_generation(&self.paths) != starting_generation {
            let _ = std::fs::remove_file(&temp_queue);
            let _ = std::fs::remove_file(&temp_playlist);
            return Err(Error::Unavailable(
                "Radio playback was canceled".to_string(),
            ));
        }

        let load =
            serde_json::json!({"command":["loadlist", temp_playlist.to_string_lossy(), "replace"]});
        if ipc::send_command(&self.paths.mpv_socket(), load).is_ok() {
            std::fs::rename(&temp_queue, self.paths.queue_file())?;
            std::fs::rename(&temp_playlist, self.paths.playlist_file())?;
            let reload = serde_json::json!({"command":["script-message","radio-atlas-reload"]});
            if ipc::send_command(&self.paths.mpv_socket(), reload).is_err() {
                return Err(Error::Unavailable(
                    "Radio player could not refresh its station queue".to_string(),
                ));
            }
            let resume = serde_json::json!({"command":["set_property","pause",false]});
            if ipc::send_command(&self.paths.mpv_socket(), resume).is_err() {
                return Err(Error::Unavailable(
                    "Radio player could not resume playback".to_string(),
                ));
            }
        } else {
            let _ = self.status_locked();
            if is_socket(&self.paths.mpv_socket()) || process::player_alive(&self.paths) {
                let _ = std::fs::remove_file(&temp_queue);
                let _ = std::fs::remove_file(&temp_playlist);
                return Err(Error::Unavailable(
                    "Radio player could not load this station".to_string(),
                ));
            }
            std::fs::rename(&temp_queue, self.paths.queue_file())?;
            std::fs::rename(&temp_playlist, self.paths.playlist_file())?;
            self.start_session()?;
        }

        fire_click_count(uuid);
        self.status_locked()
    }

    fn source_stations(&self, scope: PlayScope) -> Result<Vec<Station>> {
        match scope {
            PlayScope::World => read_runtime_array(self.paths.world_file().as_path()),
            PlayScope::Results => read_runtime_array(self.paths.results_file().as_path()),
            PlayScope::Selection => read_runtime_array(self.paths.play_selection_file().as_path()),
            PlayScope::Favorites => state::load_state(&self.paths)
                .map(|state| state.favorites)
                .ok(),
            PlayScope::Recent => state::load_state(&self.paths)
                .map(|state| state.recent)
                .ok(),
        }
        .ok_or_else(|| Error::Unavailable("Station data is unavailable or invalid".to_string()))
    }

    /// Refresh saved stations' stream URLs; failures keep the stored rows.
    fn refresh_saved_stations(&self, source: &mut [Station]) {
        if source.is_empty() {
            return;
        }
        let uuids = source
            .iter()
            .map(|station| station.uuid.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let Ok(refreshed) = self.client.resolve_stations(&uuids) else {
            return;
        };
        let by_uuid: std::collections::HashMap<&str, &Station> = refreshed
            .iter()
            .map(|station| (station.uuid.as_str(), station))
            .collect();
        for row in source.iter_mut() {
            if let Some(fresh) = by_uuid.get(row.uuid.as_str()) {
                *row = (*fresh).clone();
            }
        }
    }

    fn start_session(&self) -> Result<()> {
        let script = ensure_status_script(&self.paths)?;
        let mut args = vec![
            "--no-video".to_string(),
            "--force-window=no".to_string(),
            "--audio-display=no".to_string(),
            "--idle=yes".to_string(),
            "--loop-playlist=inf".to_string(),
            "--cache-secs=20".to_string(),
            "--demuxer-max-bytes=8MiB".to_string(),
            "--demuxer-max-back-bytes=2MiB".to_string(),
            "--network-timeout=30".to_string(),
            "--load-unsafe-playlists=no".to_string(),
            format!("--volume={}", self.configured_volume()),
            "--volume-max=100".to_string(),
            format!("--input-ipc-server={}", self.paths.mpv_socket().display()),
            format!("--script={}", script.display()),
            format!("--playlist={}", self.paths.playlist_file().display()),
            "--no-terminal".to_string(),
            "--really-quiet".to_string(),
        ];
        let output = self.configured_output();
        if !output.is_empty() {
            args.push(format!("--audio-device=pipewire/{output}"));
        }
        let pid = process::spawn_player(&self.paths, &self.mpv_program, &args)?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if is_socket(&self.paths.mpv_socket()) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        process::terminate_tracked_player(&self.paths);
        let _ = pid;
        Err(Error::Unavailable(
            "Radio player could not start".to_string(),
        ))
    }
}

// --- Helpers ------------------------------------------------------------------

fn is_socket(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_socket())
        .unwrap_or(false)
}

fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn single_line(text: &str, limit: usize) -> String {
    text.chars()
        .map(|ch| match ch {
            '\r' | '\n' | '\t' => ' ',
            _ => ch,
        })
        .take(limit)
        .collect()
}

fn clean_output(device: &str) -> String {
    if device == "auto" {
        return String::new();
    }
    truncate(device.strip_prefix("pipewire/").unwrap_or(device), 160)
}

struct StreamFailure {
    position: i64,
    message: String,
    detail: String,
}

fn parse_failure(data: Option<&serde_json::Value>) -> Option<StreamFailure> {
    let object = data?.as_object()?;
    let position = object.get("position")?.as_i64()?;
    let message = object.get("message")?.as_str()?;
    let detail = object.get("detail")?.as_str()?;
    Some(StreamFailure {
        position,
        message: single_line(message, 200),
        detail: single_line(detail, 200),
    })
}

fn read_runtime_array(path: &std::path::Path) -> Option<Vec<Station>> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_QUEUE_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let rows: Vec<Station> = serde_json::from_slice(&bytes).ok()?;
    if rows.len() > MAX_QUEUE_ROWS {
        None
    } else {
        Some(rows)
    }
}

fn is_playable(station: &Station) -> bool {
    let url = station.url.as_str();
    (url.len() >= 7
        && (url[..7].eq_ignore_ascii_case("http://")
            || (url.len() >= 8 && url[..8].eq_ignore_ascii_case("https://"))))
        && !url.chars().any(|ch| ch == '\r' || ch == '\n')
}

/// Rotate the playable queue so `uuid` plays first.
fn rotate_queue(source: &[Station], uuid: &str) -> Vec<Station> {
    let rows: Vec<Station> = source.iter().filter(|s| is_playable(s)).cloned().collect();
    let Some(position) = rows.iter().position(|station| station.uuid == uuid) else {
        return Vec::new();
    };
    rows[position..]
        .iter()
        .chain(rows[..position].iter())
        .cloned()
        .collect()
}

fn build_playlist(queue: &[Station]) -> String {
    let mut text = String::from("#EXTM3U\n");
    for station in queue {
        text.push_str("#EXTINF:-1,");
        text.push_str(&single_line(&station.name, 160));
        text.push('\n');
        text.push_str(&station.url);
        text.push('\n');
    }
    text
}

fn temp_runtime_file(paths: &Paths, prefix: &str) -> Result<std::path::PathBuf> {
    let name = format!(
        ".{prefix}-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    Ok(paths.runtime_dir.join(name))
}

/// The mpv status script: installed helper copy, else the embedded one.
fn ensure_status_script(paths: &Paths) -> Result<std::path::PathBuf> {
    if let Some(dir) = process::helpers_dir() {
        let script = dir.join("radio-status.lua");
        if script.is_file() {
            return Ok(script);
        }
    }
    const EMBEDDED: &str = include_str!("../assets/radio-status.lua");
    let target = paths.runtime_dir.join("radio-status.lua");
    let write = match std::fs::read_to_string(&target) {
        Ok(existing) => existing != EMBEDDED,
        Err(_) => true,
    };
    if write {
        atomic_write(target.as_path(), EMBEDDED.as_bytes())?;
    }
    Ok(target)
}

/// Tell Radio Browser a stream started; fire-and-forget like the original.
fn fire_click_count(uuid: &str) {
    if uuid.len() < 20 || !uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return;
    }
    let url = format!("{CLICK_COUNT_URL}/{uuid}");
    let user_agent = format!(
        "Cosmic Radio Atlas/{} (https://github.com/goshitsarch-eng/cosmic-radio-atlas)",
        env!("CARGO_PKG_VERSION")
    );
    let _ = std::process::Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--max-time",
            "4",
            "--proto",
            "=https",
            "--user-agent",
            &user_agent,
            &url,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use radio_core::paths::Paths;
    use std::os::unix::fs::PermissionsExt;

    fn station(uuid: &str) -> Station {
        Station {
            uuid: uuid.to_string(),
            name: uuid.to_string(),
            url: "https://example.com/x".to_string(),
            ..Station::default()
        }
    }

    #[test]
    fn rotates_queue_to_station() {
        let source = vec![station("a"), station("b"), station("c")];
        let queue = rotate_queue(&source, "b");
        assert_eq!(queue[0].uuid, "b");
        assert_eq!(queue[1].uuid, "c");
        assert_eq!(queue[2].uuid, "a");
    }

    #[test]
    fn drops_unplayable_rows_from_queue() {
        let mut bad = station("bad");
        bad.url = "ftp://example.com/bad".to_string();
        let source = vec![bad, station("good")];
        let queue = rotate_queue(&source, "good");
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn play_and_stop_lifecycle_with_fake_mpv() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::resolve_in(
            root.path(),
            &root.path().join("cache"),
            &root.path().join("data"),
        )
        .unwrap();
        // Fake mpv: bind the IPC socket from --input-ipc-server, then idle.
        let fake = root.path().join("fake-mpv");
        std::fs::write(
            &fake,
            "#!/bin/sh\nfor arg in \"$@\"; do\n  case \"$arg\" in --input-ipc-server=*) sock=\"${arg#*=}\";; esac\ndone\nexec python3 -c \"import socket,time,sys; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen(1); time.sleep(120)\" \"$sock\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let fixture = r#"[{"uuid":"abcdefab-1234-1234-1234-abcdefabcdef","name":"Fake FM","url":"https://example.com/fake.mp3","countryCode":"US"}]"#;
        std::fs::write(paths.results_file(), fixture).unwrap();

        let mut player = Player::with_paths(paths.clone()).unwrap();
        player.set_mpv_program(fake.to_string_lossy().into_owned());
        let status = player
            .play("abcdefab-1234-1234-1234-abcdefabcdef", PlayScope::Results)
            .unwrap();
        // The fake never answers IPC, so the status falls back to stopped.
        assert!(!status.running);
        assert!(paths.queue_file().is_file());
        assert!(paths.playlist_file().is_file());
        let queue = std::fs::read_to_string(paths.queue_file()).unwrap();
        assert!(queue.contains("abcdefab-1234-1234-1234-abcdefabcdef"));

        let status = player.stop().unwrap();
        assert!(!status.running);
        assert!(!paths.mpv_socket().exists());
        assert!(!paths.player_pid_file().exists());
    }

    #[test]
    fn reports_stopped_status_without_mpv() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::resolve_in(
            root.path(),
            &root.path().join("cache"),
            &root.path().join("data"),
        )
        .unwrap();
        let player = Player::with_paths(paths).unwrap();
        let status = player.status().unwrap();
        assert!(!status.running);
        assert_eq!(status.playlist_position, -1);
        assert!(player.paths().status_file().is_file());

        let status = player.set_volume(23).unwrap();
        assert_eq!(status.volume, 23);
        assert!(player.set_volume(101).is_err());
        assert!(player.stop().is_ok());
    }
}
