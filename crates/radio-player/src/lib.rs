//! mpv-backed radio playback with IPC control.

pub mod ipc;
pub mod outputs;
pub mod player;
pub mod process;

pub use outputs::AudioOutput;
pub use player::{PlayScope, Player, PlayerStatus, StationSummary};
