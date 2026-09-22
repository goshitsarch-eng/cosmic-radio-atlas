// SPDX-License-Identifier: MIT

//! Async background tasks shared by the app and the applet.
//!
//! Every blocking backend call (directory fetches, player IPC, saved-state
//! writes) runs on a `spawn_blocking` thread so the UI never stalls,
//! mirroring how the legacy UI shelled out to processes.

use radio_core::client::Client;
use radio_core::model::Station;
use radio_core::paths::Paths;
use radio_core::state::SavedState;
use radio_player::player::PlayScope;
use radio_player::{AudioOutput, Player, PlayerStatus};

/// Run a blocking backend closure off the UI executor.
pub async fn blocking<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|error| format!("Background task failed: {error}"))?
}

fn client_and_paths() -> Result<(Client, Paths), String> {
    let paths = Paths::resolve().map_err(|error| error.to_string())?;
    let client = Client::new().map_err(|error| error.to_string())?;
    Ok((client, paths))
}

fn player() -> Result<Player, String> {
    Player::new().map_err(|error| error.to_string())
}

// --- Directory ---------------------------------------------------------------

pub async fn fetch_world() -> Result<Vec<Station>, String> {
    blocking(|| {
        let (client, paths) = client_and_paths()?;
        client
            .fetch_world(&paths)
            .map_err(|error| error.to_string())
    })
    .await
}

pub async fn fetch_country(code: String) -> Result<Vec<Station>, String> {
    blocking(move || {
        let (client, paths) = client_and_paths()?;
        client
            .fetch_country(&paths, &code)
            .map_err(|error| error.to_string())
    })
    .await
}

pub async fn fetch_search(query: String) -> Result<Vec<Station>, String> {
    blocking(move || {
        let (client, paths) = client_and_paths()?;
        client
            .fetch_search(&paths, &query)
            .map_err(|error| error.to_string())
    })
    .await
}

pub async fn fetch_random(exclusions: String) -> Result<Vec<Station>, String> {
    blocking(move || {
        let (client, paths) = client_and_paths()?;
        client
            .fetch_random(&paths, &exclusions)
            .map_err(|error| error.to_string())
    })
    .await
}

pub async fn fetch_world_more() -> Result<Vec<Station>, String> {
    blocking(|| {
        let (client, paths) = client_and_paths()?;
        client
            .fetch_world_more(&paths)
            .map_err(|error| error.to_string())
    })
    .await
}

// --- Saved state ----------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct LocalSnapshot {
    pub favorites: Vec<Station>,
    pub recent: Vec<Station>,
    pub volume: i64,
}

impl From<SavedState> for LocalSnapshot {
    fn from(state: SavedState) -> Self {
        LocalSnapshot {
            favorites: state.favorites,
            recent: state.recent,
            volume: state.volume,
        }
    }
}

pub async fn load_local_state() -> Result<LocalSnapshot, String> {
    blocking(|| {
        let paths = Paths::resolve().map_err(|error| error.to_string())?;
        radio_core::state::load_state(&paths)
            .map(LocalSnapshot::from)
            .map_err(|error| error.to_string())
    })
    .await
}

pub async fn toggle_favorite(
    uuid: String,
    selection: Option<Vec<Station>>,
) -> Result<LocalSnapshot, String> {
    blocking(move || {
        let paths = Paths::resolve().map_err(|error| error.to_string())?;
        if let Some(rows) = selection {
            let bytes = serde_json::to_vec(&rows).map_err(|error| error.to_string())?;
            radio_core::paths::atomic_write(&paths.favorite_selection_file(), &bytes)
                .map_err(|error| error.to_string())?;
        }
        radio_core::state::toggle_favorite(&paths, &uuid)
            .map(LocalSnapshot::from)
            .map_err(|error| error.to_string())
    })
    .await
}

pub async fn record_played(uuid: String) -> Result<LocalSnapshot, String> {
    blocking(move || {
        let paths = Paths::resolve().map_err(|error| error.to_string())?;
        radio_core::state::record_played(&paths, &uuid)
            .map(LocalSnapshot::from)
            .map_err(|error| error.to_string())
    })
    .await
}

// --- Player ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerCommand {
    Toggle,
    Next,
    Previous,
    Mute,
    Stop,
}

pub async fn poll_player() -> Result<PlayerStatus, String> {
    blocking(|| player()?.status().map_err(|error| error.to_string())).await
}

pub async fn run_player_command(command: PlayerCommand) -> Result<PlayerStatus, String> {
    blocking(move || {
        let player = player()?;
        let result = match command {
            PlayerCommand::Toggle => player.toggle(),
            PlayerCommand::Next => player.next(),
            PlayerCommand::Previous => player.previous(),
            PlayerCommand::Mute => player.mute(),
            PlayerCommand::Stop => player.stop(),
        };
        result.map_err(|error| error.to_string())
    })
    .await
}

pub async fn play_station(
    uuid: String,
    scope: PlayScope,
    selection: Option<Vec<Station>>,
) -> Result<PlayerStatus, String> {
    blocking(move || {
        let player = player()?;
        if let Some(rows) = selection {
            let bytes = serde_json::to_vec(&rows).map_err(|error| error.to_string())?;
            radio_core::paths::atomic_write(&player.paths().play_selection_file(), &bytes)
                .map_err(|error| error.to_string())?;
        }
        player.play(&uuid, scope).map_err(|error| error.to_string())
    })
    .await
}

pub async fn set_player_volume(volume: i64) -> Result<PlayerStatus, String> {
    blocking(move || {
        player()?
            .set_volume(volume)
            .map_err(|error| error.to_string())
    })
    .await
}

pub async fn list_player_outputs() -> Result<Vec<AudioOutput>, String> {
    blocking(|| player()?.list_outputs().map_err(|error| error.to_string())).await
}

pub async fn set_player_output(output: String) -> Result<PlayerStatus, String> {
    blocking(move || {
        player()?
            .set_output(&output)
            .map_err(|error| error.to_string())
    })
    .await
}
