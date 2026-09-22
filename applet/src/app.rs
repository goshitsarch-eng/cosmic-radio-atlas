// SPDX-License-Identifier: MIT

//! Panel applet for Cosmic Radio Atlas: a status icon with a popup for
//! transport controls, volume, random tuning, and opening the full app.
//!
//! Ports the legacy `BarWidget.qml`: the icon reflects playback state and
//! every control talks to the same mpv backend the app uses, so the panel
//! and the window never disagree.

use std::collections::HashSet;
use std::time::Duration;

use cosmic::cosmic_config::{self, CosmicConfigEntry};
use cosmic::iced::platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup};
use cosmic::iced::{Length, Limits, Subscription, window::Id};
use cosmic::prelude::*;
use cosmic::widget;
use radio_core::model::Station;
use radio_player::player::{PlayScope, PlayerStatus};
use radio_tasks::{self as tasks, LocalSnapshot, PlayerCommand};

use crate::config::Config;

const POLL_INTERVAL: Duration = Duration::from_secs(1);

fn cmd(future: impl Future<Output = Message> + Send + 'static) -> Task<cosmic::Action<Message>> {
    Task::perform(future, cosmic::Action::App)
}

/// The application model stores app-specific state used to describe its interface and
/// drive its logic.
pub struct AppModel {
    /// Application state which is managed by the COSMIC runtime.
    core: cosmic::Core,
    /// The popup id.
    popup: Option<Id>,
    /// Configuration data that persists between application runs.
    config: Config,
    /// Latest player snapshot.
    player: PlayerStatus,
    /// A status poll is in flight.
    poll_in_flight: bool,
    /// A transport command is in flight.
    busy: bool,
    /// Slider position, echoed from the player when idle.
    volume_display: i64,
    /// Favorite station ids, for the popup toggle.
    favorite_uuids: HashSet<String>,
    /// Last error shown in the popup.
    error: String,
    /// Random-tune generation; stale fetches are dropped after Stop.
    random_token: u64,
}

/// Messages emitted by the application and its widgets.
#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    PopupClosed(Id),
    UpdateConfig(Config),
    PollTick,
    PlayerPolled(Result<PlayerStatus, String>),
    Transport(PlayerCommand),
    TransportDone(Result<PlayerStatus, String>),
    VolumeChanged(f32),
    VolumeCommit,
    VolumeDone(Result<PlayerStatus, String>),
    TuneRandom,
    RandomFetched(u64, Result<Vec<Station>, String>),
    PlayDone(Result<PlayerStatus, String>),
    ToggleFavorite,
    FavoriteDone(Result<LocalSnapshot, String>),
    OpenApp,
}

/// Create a COSMIC application from the app model
impl cosmic::Application for AppModel {
    /// The async executor that will be used to run your application's commands.
    type Executor = cosmic::executor::Default;

    /// Data that your application receives to its init method.
    type Flags = ();

    /// Messages which the application and its widgets will emit.
    type Message = Message;

    /// Unique identifier in RDNN (reverse domain name notation) format.
    const APP_ID: &'static str = "io.github.goshitsarcheng.RadioAtlasApplet";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    /// Initializes the application with any given flags and startup commands.
    fn init(
        core: cosmic::Core,
        _flags: Self::Flags,
    ) -> (Self, Task<cosmic::Action<Self::Message>>) {
        let app = AppModel {
            core,
            popup: None,
            config: cosmic_config::Config::new(Self::APP_ID, Config::VERSION)
                .map(|context| match Config::get_entry(&context) {
                    Ok(config) => config,
                    Err((_errors, config)) => config,
                })
                .unwrap_or_default(),
            player: PlayerStatus::default(),
            poll_in_flight: true,
            busy: false,
            volume_display: 70,
            favorite_uuids: HashSet::new(),
            error: String::new(),
            random_token: 0,
        };
        let poll = cmd(async { Message::PlayerPolled(tasks::poll_player().await) });
        let local = cmd(async { Message::FavoriteDone(tasks::load_local_state().await) });
        (app, Task::batch([poll, local]))
    }

    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    /// The applet's button in the panel.
    fn view(&self) -> Element<'_, Self::Message> {
        let icon = if self.player.running && !self.player.paused {
            "media-playback-start-symbolic"
        } else {
            "audio-speakers-symbolic"
        };
        self.core
            .applet
            .icon_button(icon)
            .on_press(Message::TogglePopup)
            .into()
    }

    /// The applet's popup window.
    fn view_window(&self, _id: Id) -> Element<'_, Self::Message> {
        let name = if self.player.running {
            let summary = self.player.station.name.trim();
            if summary.is_empty() {
                if self.player.title.trim().is_empty() {
                    "Unknown station".to_string()
                } else {
                    self.player.title.trim().to_string()
                }
            } else {
                summary.to_string()
            }
        } else {
            "Nothing playing".to_string()
        };
        let status = if !self.error.is_empty() {
            self.error.clone()
        } else if !self.player.error.is_empty() {
            format!("{}. Open the app to retry.", self.player.error)
        } else if !self.player.running {
            "Open Radio Atlas to browse stations".to_string()
        } else if self.player.paused {
            "Paused".to_string()
        } else {
            "Live".to_string()
        };
        let playing = self.player.running && !self.player.paused;
        let transport = widget::row::with_capacity(4)
            .push(transport_button(
                "media-skip-backward-symbolic",
                Message::Transport(PlayerCommand::Previous),
                self.player.running && !self.busy,
            ))
            .push(transport_button(
                if playing {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                },
                Message::Transport(PlayerCommand::Toggle),
                self.player.running && !self.busy,
            ))
            .push(transport_button(
                "media-skip-forward-symbolic",
                Message::Transport(PlayerCommand::Next),
                self.player.running && !self.busy,
            ))
            .push(transport_button(
                "media-playback-stop-symbolic",
                Message::Transport(PlayerCommand::Stop),
                self.player.running && !self.busy,
            ))
            .spacing(4);
        let mut column = widget::column::with_capacity(6)
            .push(widget::text::body(name))
            .push(widget::text::caption(status))
            .push(transport)
            .spacing(8);
        if !self.config.hide_volume {
            column = column.push(
                widget::row::with_capacity(3)
                    .push(transport_button(
                        if self.player.muted {
                            "audio-volume-muted-symbolic"
                        } else {
                            "audio-volume-high-symbolic"
                        },
                        Message::Transport(PlayerCommand::Mute),
                        self.player.running && !self.busy,
                    ))
                    .push(
                        widget::Slider::new(
                            0.0..=100.0,
                            self.volume_display as f32,
                            Message::VolumeChanged,
                        )
                        .step(1.0_f32)
                        .on_release(Message::VolumeCommit)
                        .width(Length::Fill),
                    )
                    .push(widget::text::caption(format!("{}%", self.volume_display)))
                    .spacing(8)
                    .align_y(cosmic::iced::Alignment::Center),
            );
        }
        let playing_uuid = self.player.station.uuid.clone();
        let favorite = !playing_uuid.is_empty() && self.favorite_uuids.contains(&playing_uuid);
        column = column.push(
            widget::row::with_capacity(3)
                .push(
                    widget::button::standard("Random")
                        .on_press(Message::TuneRandom)
                        .width(Length::Fill),
                )
                .push(favorite_button(
                    favorite,
                    self.player.running && !playing_uuid.is_empty(),
                ))
                .spacing(8),
        );
        column = column.push(
            widget::button::standard("Open Radio Atlas")
                .on_press(Message::OpenApp)
                .width(Length::Fill),
        );
        self.core.applet.popup_container(column.width(320)).into()
    }

    /// Register subscriptions for this application.
    fn subscription(&self) -> Subscription<Self::Message> {
        Subscription::batch(vec![
            self.core()
                .watch_config::<Config>(Self::APP_ID)
                .map(|update| Message::UpdateConfig(update.config)),
            cosmic::iced::time::every(POLL_INTERVAL).map(|_| Message::PollTick),
        ])
    }

    /// Handles messages emitted by the application and its widgets.
    fn update(&mut self, message: Self::Message) -> Task<cosmic::Action<Self::Message>> {
        match message {
            Message::UpdateConfig(config) => {
                self.config = config;
            }
            Message::TogglePopup => {
                return if let Some(p) = self.popup.take() {
                    destroy_popup(p)
                } else {
                    let new_id = Id::unique();
                    self.popup.replace(new_id);
                    let mut popup_settings = self.core.applet.get_popup_settings(
                        self.core.main_window_id().unwrap(),
                        new_id,
                        None,
                        None,
                        None,
                    );
                    popup_settings.positioner.size_limits = Limits::NONE
                        .max_width(372.0)
                        .min_width(300.0)
                        .min_height(200.0)
                        .max_height(1080.0);
                    get_popup(popup_settings)
                };
            }
            Message::PopupClosed(id) => {
                if self.popup.as_ref() == Some(&id) {
                    self.popup = None;
                }
            }
            Message::PollTick => {
                if !self.poll_in_flight {
                    self.poll_in_flight = true;
                    return cmd(async { Message::PlayerPolled(tasks::poll_player().await) });
                }
            }
            Message::PlayerPolled(result) => {
                self.poll_in_flight = false;
                if let Ok(status) = result {
                    self.error.clear();
                    self.player = status;
                    self.volume_display = self.player.volume;
                }
            }
            Message::Transport(command) => {
                if self.busy || (command != PlayerCommand::Stop && !self.player.running) {
                    return Task::none();
                }
                if command == PlayerCommand::Stop {
                    self.random_token += 1;
                }
                self.busy = true;
                self.error.clear();
                return cmd(async move {
                    Message::TransportDone(tasks::run_player_command(command).await)
                });
            }
            Message::TransportDone(result) => {
                self.busy = false;
                match result {
                    Ok(status) => {
                        self.player = status;
                        self.volume_display = self.player.volume;
                    }
                    Err(_) => self.error = "Player action failed".to_string(),
                }
            }
            Message::VolumeChanged(value) => {
                self.volume_display = (value.round() as i64).clamp(0, 100);
            }
            Message::VolumeCommit => {
                let volume = self.volume_display;
                return cmd(
                    async move { Message::VolumeDone(tasks::set_player_volume(volume).await) },
                );
            }
            Message::VolumeDone(result) => match result {
                Ok(status) => {
                    self.error.clear();
                    self.player = status;
                    self.volume_display = self.player.volume;
                }
                Err(_) => {
                    self.volume_display = self.player.volume;
                    self.error = "Could not change volume".to_string();
                }
            },
            Message::TuneRandom => {
                if self.busy {
                    return Task::none();
                }
                self.busy = true;
                self.error.clear();
                self.random_token += 1;
                let token = self.random_token;
                let exclusions = self.player.station.uuid.clone();
                return cmd(async move {
                    Message::RandomFetched(token, tasks::fetch_random(exclusions).await)
                });
            }
            Message::RandomFetched(token, result) => {
                if token != self.random_token {
                    self.busy = false;
                    return Task::none();
                }
                match result {
                    Ok(stations) => {
                        if let Some(first) = stations.first() {
                            let uuid = first.uuid.clone();
                            return cmd(async move {
                                Message::PlayDone(
                                    tasks::play_station(uuid, PlayScope::Results, None).await,
                                )
                            });
                        }
                        self.busy = false;
                        self.error = "No working stations found.".to_string();
                    }
                    Err(_) => {
                        self.busy = false;
                        self.error = "Radio Browser is unavailable. Try again shortly.".to_string();
                    }
                }
            }
            Message::PlayDone(result) => {
                self.busy = false;
                match result {
                    Ok(status) => {
                        self.player = status;
                        self.volume_display = self.player.volume;
                    }
                    Err(_) => self.error = "Could not play this station".to_string(),
                }
            }
            Message::ToggleFavorite => {
                let uuid = self.player.station.uuid.clone();
                if uuid.is_empty() {
                    return Task::none();
                }
                return cmd(async move {
                    Message::FavoriteDone(tasks::toggle_favorite(uuid, None).await)
                });
            }
            Message::FavoriteDone(result) => match result {
                Ok(snapshot) => {
                    self.favorite_uuids = snapshot
                        .favorites
                        .iter()
                        .map(|station| station.uuid.clone())
                        .collect();
                }
                Err(_) => self.error = "Favorite could not be updated".to_string(),
            },
            Message::OpenApp => {
                if let Err(error) = std::process::Command::new("cosmic-radio-atlas")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                {
                    self.error = format!("Could not open Radio Atlas: {error}");
                }
            }
        }
        Task::none()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

fn transport_button(
    icon: &'static str,
    message: Message,
    enabled: bool,
) -> widget::Button<'static, Message> {
    let button = widget::button::custom(widget::icon::from_name(icon).size(16)).padding(8);
    if enabled {
        button.on_press(message)
    } else {
        button
    }
}

fn favorite_button(favorite: bool, enabled: bool) -> widget::Button<'static, Message> {
    let button =
        widget::button::custom(widget::icon::from_name("emblem-favorite-symbolic").size(16))
            .class(if favorite {
                cosmic::theme::Button::Suggested
            } else {
                cosmic::theme::Button::Text
            })
            .padding(8);
    if enabled {
        button.on_press(Message::ToggleFavorite)
    } else {
        button
    }
}
