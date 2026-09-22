// SPDX-License-Identifier: MIT

//! Cosmic Radio Atlas: live radio on a rotatable globe.
//!
//! Ports the legacy `RadioAtlas.qml` state machine: directory fetches with a
//! pending slot, world-cache expansion, selection and playback queues, and
//! saved favorites/history. Blocking backend calls run on worker threads and
//! report back as messages, the way the QML UI shelled out to processes.

use std::time::{Duration, Instant};

use cosmic::app::context_drawer;
use cosmic::cosmic_config::{self, CosmicConfigEntry};
use cosmic::iced::{Length, Subscription, keyboard};
use cosmic::prelude::*;
use cosmic::widget::{self, about::About};
use radio_core::countries::embedded_countries;
use radio_core::geo::{self};
use radio_core::model::Station;
use radio_player::AudioOutput;
use radio_player::player::{PlayScope, PlayerStatus};

use crate::config::Config;
use crate::fl;
use crate::globe::{self, GlobeProgram, GlobeState};
use radio_tasks::{self as tasks, LocalSnapshot, PlayerCommand};

const APP_ICON: &[u8] = include_bytes!("../resources/icons/hicolor/scalable/apps/icon.svg");
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const WORLD_STATION_LIMIT: usize = 5000;
const SEARCH_PREVIEW_LIMIT: usize = 150;
const COUNTRY_PREVIEW_LIMIT: usize = 150;
const MAX_VISIBLE_ROWS: usize = 300;
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const KINETIC_INTERVAL: Duration = Duration::from_millis(16);
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);

/// Wrap a message-producing future as a cosmic task.
fn cmd(future: impl Future<Output = Message> + Send + 'static) -> Task<cosmic::Action<Message>> {
    Task::perform(future, cosmic::Action::App)
}

async fn after(duration: Duration) {
    tokio::time::sleep(duration).await;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    World,
    Country { code: String, name: String },
    Search { query: String },
    Random,
    Favorites,
    Recent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchKind {
    World,
    Country { code: String },
    Search { query: String },
    Random,
}

#[derive(Debug, Clone)]
pub struct PendingPlay {
    uuid: String,
    scope: PlayScope,
    selection: Option<Vec<Station>>,
}

/// Messages emitted by the application and its widgets.
#[derive(Debug, Clone)]
pub enum Message {
    SearchInput(String),
    SearchSubmit,
    SearchDebounced(u64, String),
    TuneRandom,
    ToggleHelp,
    ToggleContextPage(ContextPage),
    ToggleOutputMenu,
    LaunchUrl(String),
    UpdateConfig(Config),
    ShowWorld,
    ShowFavorites,
    ShowRecent,
    FetchCompleted(FetchKind, Result<Vec<Station>, String>),
    WorldExpandDue,
    WorldExpanded(Result<Vec<Station>, String>),
    LocalState(Result<LocalSnapshot, String>),
    PlaySelected,
    StationRowPressed(String),
    MapStationTapped(String),
    MapCountryTapped(String),
    PlayerAction(PlayerCommand),
    PlayerResult(PlayerCommand, Result<PlayerStatus, String>),
    PlayCompleted(Result<PlayerStatus, String>),
    PollTick,
    PlayerPolled(Result<PlayerStatus, String>),
    ToggleFavorite(String),
    VolumeChanged(f32),
    VolumeCommit,
    VolumeCommitted(Result<PlayerStatus, String>),
    OutputsLoaded(Result<Vec<AudioOutput>, String>),
    SelectOutput(String),
    OutputSelected(Result<PlayerStatus, String>),
    GlobePressed,
    GlobeDragged(f64, f64),
    GlobeFlick(f64, f64),
    GlobeZoom(f64),
    GlobeHover(Option<String>),
    KineticTick(Instant),
    Keyboard(keyboard::Event),
}

/// The application model stores app-specific state used to describe its interface and
/// drive its logic.
pub struct AppModel {
    /// Application state which is managed by the COSMIC runtime.
    core: cosmic::Core,
    /// Display a context drawer with the designated page if defined.
    context_page: ContextPage,
    /// The about page for this app.
    about: About,
    /// Configuration data that persists between application runs.
    config: Config,

    world_stations: Vec<Station>,
    results: Vec<Station>,
    favorites: Vec<Station>,
    recent: Vec<Station>,
    geo_stations: Vec<Station>,
    mode: Mode,
    active_country_code: String,
    active_country_name: String,
    selected_index: Option<usize>,
    keyboard_selection: bool,

    search_text: String,
    search_token: u64,
    search_id: widget::Id,
    fetching: Option<FetchKind>,
    pending_fetch: Option<FetchKind>,
    fetch_error: String,
    local_error: String,
    expansion_misses: u32,
    expansion_scheduled: bool,

    player: PlayerStatus,
    player_error: String,
    recorded_uuid: String,
    last_random_uuid: String,
    poll_in_flight: bool,
    player_busy: bool,
    play_in_flight: bool,
    play_cancel_requested: bool,
    pending_play: Option<PendingPlay>,
    pending_favorites: Vec<(String, Option<Vec<Station>>)>,
    favorite_busy: bool,

    volume_display: i64,
    outputs: Vec<AudioOutput>,
    outputs_error: String,
    outputs_loading: bool,
    output_menu: bool,

    globe: GlobeState,
    help_visible: bool,
}

impl AppModel {
    fn display_stations(&self) -> &[Station] {
        match self.mode {
            Mode::Favorites => &self.favorites,
            Mode::Recent => &self.recent,
            _ => &self.results,
        }
    }

    fn remote_mode(&self) -> bool {
        !matches!(self.mode, Mode::Favorites | Mode::Recent)
    }

    fn selected_station(&self) -> Option<&Station> {
        self.selected_index
            .and_then(|index| self.display_stations().get(index))
    }

    fn playing_uuid(&self) -> &str {
        if self.player.running {
            self.player.station.uuid.as_str()
        } else {
            ""
        }
    }

    fn set_selection(&mut self, index: Option<usize>, keyboard: bool) {
        self.keyboard_selection = keyboard;
        let len = self.display_stations().len();
        self.selected_index = match (index, len) {
            (Some(i), n) if n > 0 => Some(i.min(n - 1)),
            _ => None,
        };
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.display_stations().len();
        if len == 0 {
            return;
        }
        let next = match self.selected_index {
            Some(i) => (i as i32 + delta).rem_euclid(len as i32) as usize,
            None if delta < 0 => len - 1,
            None => 0,
        };
        self.set_selection(Some(next), true);
    }

    fn set_station_list(&mut self, mode: Mode, stations: Vec<Station>) {
        let local = matches!(mode, Mode::Favorites | Mode::Recent);
        self.mode = mode;
        if !local {
            self.results = stations;
        }
        let len = self.display_stations().len();
        self.selected_index = if len > 0 { Some(0) } else { None };
        self.keyboard_selection = false;
        self.refresh_geo();
    }

    fn refresh_geo(&mut self) {
        let countries = embedded_countries();
        let geo = {
            let display: &[Station] = match self.mode {
                Mode::Favorites => &self.favorites,
                Mode::Recent => &self.recent,
                _ => &self.results,
            };
            geo::merge_geo_stations(
                &self.world_stations,
                display,
                countries,
                &mut self.globe.estimates,
            )
        };
        self.geo_stations = geo;
        for watched in [&mut self.globe.landing_uuid, &mut self.globe.hover_uuid] {
            if let Some(uuid) = watched.as_deref() {
                if !self.geo_stations.iter().any(|station| station.uuid == uuid) {
                    *watched = None;
                }
            }
        }
    }

    fn playlist_scope(&self) -> PlayScope {
        match self.mode {
            Mode::World => PlayScope::World,
            Mode::Favorites => PlayScope::Favorites,
            Mode::Recent => PlayScope::Recent,
            _ => PlayScope::Results,
        }
    }

    fn highlight_station_country(&mut self, station: &Station, focus_globe: bool) {
        self.active_country_code = station.country_code.to_ascii_uppercase();
        self.active_country_name = if station.country.is_empty() {
            self.active_country_code.clone()
        } else {
            station.country.clone()
        };
        if !focus_globe {
            return;
        }
        match (station.latitude, station.longitude) {
            (Some(lat), Some(lon)) => self.globe.focus_coordinate(lat, lon),
            _ if !self.active_country_code.is_empty() => {
                if let Some(center) =
                    geo::country_centre(embedded_countries(), &self.active_country_code)
                {
                    self.globe
                        .focus_coordinate(center.latitude, center.longitude);
                }
            }
            _ => {}
        }
    }

    fn restore_playing_country(&mut self, focus_globe: bool) {
        if self.player.running && !self.player.station.uuid.is_empty() {
            let summary = self.player.station.clone();
            let station = Station {
                uuid: summary.uuid,
                name: summary.name,
                country: summary.country,
                country_code: summary.country_code,
                latitude: summary.latitude,
                longitude: summary.longitude,
                ..Station::default()
            };
            self.highlight_station_country(&station, focus_globe);
        } else {
            self.active_country_code.clear();
            self.active_country_name.clear();
        }
    }

    // --- Fetching ------------------------------------------------------------

    fn mode_matches(&self, kind: &FetchKind) -> bool {
        match (&self.mode, kind) {
            (Mode::World, FetchKind::World) => true,
            (Mode::Country { code, .. }, FetchKind::Country { code: wanted }) => code == wanted,
            (Mode::Search { .. }, FetchKind::Search { .. }) => true,
            (Mode::Random, FetchKind::Random) => true,
            _ => false,
        }
    }

    fn start_fetch(&mut self, kind: FetchKind) -> Task<cosmic::Action<Message>> {
        if self.fetching.is_some() {
            if self.fetching.as_ref() == Some(&kind) {
                self.pending_fetch = None;
            } else {
                self.pending_fetch = Some(kind);
            }
            return Task::none();
        }
        self.fetching = Some(kind.clone());
        match kind.clone() {
            FetchKind::World => {
                cmd(async { Message::FetchCompleted(FetchKind::World, tasks::fetch_world().await) })
            }
            FetchKind::Country { code } => cmd(async move {
                let result = tasks::fetch_country(code.clone()).await;
                Message::FetchCompleted(FetchKind::Country { code }, result)
            }),
            FetchKind::Search { query } => cmd(async move {
                let result = tasks::fetch_search(query.clone()).await;
                Message::FetchCompleted(FetchKind::Search { query }, result)
            }),
            FetchKind::Random => {
                let exclusions = self.random_exclusions();
                cmd(async move {
                    let result = tasks::fetch_random(exclusions).await;
                    Message::FetchCompleted(FetchKind::Random, result)
                })
            }
        }
    }

    fn random_exclusions(&self) -> String {
        let mut seen = std::collections::HashSet::new();
        let mut output = Vec::new();
        let mut candidates = vec![
            self.playing_uuid().to_string(),
            self.last_random_uuid.clone(),
        ];
        candidates.extend(self.recent.iter().map(|station| station.uuid.clone()));
        for uuid in candidates {
            if output.len() >= 32 {
                break;
            }
            let valid = (20..=64).contains(&uuid.len())
                && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
            if valid && seen.insert(uuid.clone()) {
                output.push(uuid);
            }
        }
        output.join(",")
    }

    fn search_text_trimmed(&self) -> String {
        self.search_text.trim().chars().take(128).collect()
    }

    /// Instant cached preview; returns false when the query cleared the search.
    fn preview_search(&mut self) -> bool {
        let query = self.search_text_trimmed();
        if query.is_empty() {
            let _ = self.show_world_inner(false);
            return false;
        }
        self.restore_playing_country(false);
        self.fetch_error.clear();
        let matches = geo::search_stations(&self.world_stations, &query, SEARCH_PREVIEW_LIMIT);
        self.set_station_list(Mode::Search { query }, matches);
        true
    }

    fn show_world_inner(&mut self, fetch: bool) -> Task<cosmic::Action<Message>> {
        self.pending_fetch = None;
        self.search_text.clear();
        self.restore_playing_country(false);
        self.fetch_error.clear();
        self.local_error.clear();
        let world = self.world_stations.clone();
        self.set_station_list(Mode::World, world);
        if fetch {
            self.start_fetch(FetchKind::World)
        } else {
            Task::none()
        }
    }

    fn schedule_expansion(&mut self, delay: Duration) -> Task<cosmic::Action<Message>> {
        if self.expansion_scheduled
            || self.world_stations.is_empty()
            || self.world_stations.len() >= WORLD_STATION_LIMIT
            || self.expansion_misses >= 3
        {
            return Task::none();
        }
        self.expansion_scheduled = true;
        cmd(async move {
            after(delay.max(Duration::from_millis(500))).await;
            Message::WorldExpandDue
        })
    }

    fn browse_country(&mut self, code: &str, name: &str) -> Task<cosmic::Action<Message>> {
        let code = code.to_ascii_uppercase();
        if code.len() != 2 || !code.bytes().all(|b| b.is_ascii_alphabetic()) {
            return Task::none();
        }
        let name = if name.is_empty() {
            radio_core::countries::country_name(embedded_countries(), &code)
                .unwrap_or(&code)
                .to_string()
        } else {
            name.to_string()
        };
        self.search_token += 1;
        self.pending_fetch = None;
        let cached = geo::stations_for_country(&self.world_stations, &code, COUNTRY_PREVIEW_LIMIT);
        let world = std::mem::take(&mut self.world_stations);
        self.world_stations = geo::prioritize_stations(&cached, &world, WORLD_STATION_LIMIT);
        self.set_station_list(
            Mode::Country {
                code: code.clone(),
                name: name.clone(),
            },
            cached,
        );
        self.active_country_code = code.clone();
        self.active_country_name = name.clone();
        self.fetch_error.clear();
        self.search_text = name;
        self.start_fetch(FetchKind::Country { code })
    }

    // --- Playback ---------------------------------------------------------------

    fn play_station(
        &mut self,
        station: Station,
        scope: PlayScope,
        stations: Vec<Station>,
    ) -> Task<cosmic::Action<Message>> {
        if self.player_busy {
            self.pending_play = Some(PendingPlay {
                uuid: station.uuid,
                scope,
                selection: Some(stations),
            });
            return Task::none();
        }
        self.pending_play = None;
        let (scope, selection) = match scope {
            PlayScope::World | PlayScope::Results => {
                let window = geo::station_window(&stations, &station.uuid, 500);
                if window.is_empty() {
                    self.player_error = "Could not prepare this station".to_string();
                    return Task::none();
                }
                (PlayScope::Selection, Some(window))
            }
            other => (other, None),
        };
        self.highlight_station_country(&station, true);
        self.player_error.clear();
        self.player_busy = true;
        self.play_in_flight = true;
        let uuid = station.uuid.clone();
        cmd(
            async move { Message::PlayCompleted(tasks::play_station(uuid, scope, selection).await) },
        )
    }

    fn play_selected(&mut self) -> Task<cosmic::Action<Message>> {
        let Some(station) = self.selected_station().cloned() else {
            return Task::none();
        };
        let scope = self.playlist_scope();
        let stations = self.display_stations().to_vec();
        self.play_station(station, scope, stations)
    }

    fn play_pending(&mut self) -> Task<cosmic::Action<Message>> {
        let Some(pending) = self.pending_play.take() else {
            return Task::none();
        };
        if self.player_busy {
            self.pending_play = Some(pending);
            return Task::none();
        }
        let stations = pending.selection.clone().unwrap_or_default();
        let Some(station) = stations
            .iter()
            .find(|station| station.uuid == pending.uuid)
            .cloned()
        else {
            return Task::none();
        };
        self.play_station(station, pending.scope, stations)
    }

    fn activate_map_station(&mut self, uuid: &str) -> Task<cosmic::Action<Message>> {
        if geo::index_by_uuid(self.display_stations(), uuid).is_none() {
            if geo::index_by_uuid(&self.world_stations, uuid).is_none() {
                return Task::none();
            }
            let world = self.world_stations.clone();
            self.set_station_list(Mode::World, world);
            self.restore_playing_country(false);
        }
        let index = geo::index_by_uuid(self.display_stations(), uuid);
        self.set_selection(index, false);
        self.play_selected()
    }

    fn apply_player_status(&mut self, status: PlayerStatus) -> Task<cosmic::Action<Message>> {
        let previous_uuid = self.playing_uuid().to_string();
        let next_uuid = if status.running {
            status.station.uuid.clone()
        } else {
            String::new()
        };
        self.player = status;
        self.volume_display = self.player.volume;

        let mut matching = if next_uuid.is_empty() {
            None
        } else {
            geo::index_by_uuid(self.display_stations(), &next_uuid)
        };
        if matching.is_none() && !self.player.title.is_empty() {
            matching = self
                .display_stations()
                .iter()
                .position(|station| station.name == self.player.title);
        }
        let changed = self.player.running && !next_uuid.is_empty() && next_uuid != previous_uuid;
        if changed {
            if let Some(index) = matching {
                self.set_selection(Some(index), false);
            }
            let station = Station {
                uuid: self.player.station.uuid.clone(),
                name: self.player.station.name.clone(),
                country: self.player.station.country.clone(),
                country_code: self.player.station.country_code.clone(),
                latitude: self.player.station.latitude,
                longitude: self.player.station.longitude,
                ..Station::default()
            };
            self.highlight_station_country(&station, true);
        }
        if self.player.loaded && !next_uuid.is_empty() && next_uuid != self.recorded_uuid {
            self.recorded_uuid = next_uuid.clone();
            return cmd(async move { Message::LocalState(tasks::record_played(next_uuid).await) });
        }
        if !self.player.running {
            self.recorded_uuid.clear();
        }
        Task::none()
    }

    fn apply_local_snapshot(&mut self, snapshot: LocalSnapshot) -> Task<cosmic::Action<Message>> {
        self.favorites = snapshot.favorites;
        self.recent = snapshot.recent;
        self.volume_display = snapshot.volume;
        self.local_error.clear();
        self.refresh_geo();
        // Keep the selection on a sensible row for local modes.
        if matches!(self.mode, Mode::Favorites | Mode::Recent) {
            let len = self.display_stations().len();
            let preferred = self
                .selected_station()
                .map(|station| station.uuid.clone())
                .or_else(|| {
                    let uuid = self.playing_uuid().to_string();
                    (!uuid.is_empty()).then_some(uuid)
                });
            let index = preferred
                .and_then(|uuid| geo::index_by_uuid(self.display_stations(), &uuid))
                .or(if len > 0 {
                    Some(self.selected_index.unwrap_or(0).min(len - 1))
                } else {
                    None
                });
            self.set_selection(index, self.keyboard_selection);
        }
        self.favorite_busy = false;
        if let Some((uuid, selection)) = self.pending_favorites.pop() {
            self.favorite_busy = true;
            return cmd(async move {
                Message::LocalState(tasks::toggle_favorite(uuid, selection).await)
            });
        }
        Task::none()
    }

    fn toggle_favorite(&mut self, uuid: &str) -> Task<cosmic::Action<Message>> {
        if uuid.is_empty() {
            return Task::none();
        }
        self.local_error.clear();
        let mut selection = None;
        if self.remote_mode() && geo::index_by_uuid(self.display_stations(), uuid).is_some() {
            let window = geo::station_window(self.display_stations(), uuid, 500);
            if window.is_empty() {
                self.local_error = "Favorite could not be updated".to_string();
                return Task::none();
            }
            selection = Some(window);
        }
        if self.favorite_busy {
            self.pending_favorites.push((uuid.to_string(), selection));
            return Task::none();
        }
        self.favorite_busy = true;
        let uuid = uuid.to_string();
        cmd(async move { Message::LocalState(tasks::toggle_favorite(uuid, selection).await) })
    }

    fn is_favorite(&self, uuid: &str) -> bool {
        geo::index_by_uuid(&self.favorites, uuid).is_some()
    }

    fn empty_state_text(&self) -> String {
        if !self.fetch_error.is_empty() {
            return self.fetch_error.clone();
        }
        if !self.local_error.is_empty() {
            return self.local_error.clone();
        }
        match &self.mode {
            Mode::Favorites => "No favorites yet. Select a station and press F.".to_string(),
            Mode::Recent => "No listening history yet.".to_string(),
            Mode::Search { .. } => format!("No stations match “{}”", self.search_text_trimmed()),
            Mode::Country { .. } => format!(
                "No working stations found in {}.",
                if self.active_country_name.is_empty() {
                    "this country"
                } else {
                    self.active_country_name.as_str()
                }
            ),
            _ => "No working stations found.".to_string(),
        }
    }

    fn playing_track_title(&self) -> String {
        let title = self.player.title.trim();
        let station = self.player.station.name.trim().to_lowercase();
        if !title.is_empty() && !station.is_empty() && title.to_lowercase() != station {
            title.to_string()
        } else {
            String::new()
        }
    }

    fn update_title(&mut self) -> Task<cosmic::Action<Message>> {
        let mut title = fl!("app-title").to_string();
        let page = match &self.mode {
            Mode::World => "World",
            Mode::Country { name, .. } => name.as_str(),
            Mode::Search { query } => query.as_str(),
            Mode::Random => "Random",
            Mode::Favorites => "Favorites",
            Mode::Recent => "Recent",
        };
        if !page.is_empty() {
            title.push_str(" — ");
            title.push_str(page);
        }
        if let Some(id) = self.core.main_window_id() {
            self.set_window_title(title, id)
        } else {
            Task::none()
        }
    }
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
    const APP_ID: &'static str = "io.github.goshitsarcheng.RadioAtlas";

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
        let about = About::default()
            .name(fl!("app-title"))
            .icon(widget::icon::from_svg_bytes(APP_ICON))
            .version(env!("CARGO_PKG_VERSION"))
            .links([(fl!("repository"), REPOSITORY)])
            .license(env!("CARGO_PKG_LICENSE"));

        let config: Config = cosmic_config::Config::new(Self::APP_ID, Config::VERSION)
            .map(|context| match Config::get_entry(&context) {
                Ok(config) => config,
                Err((_errors, config)) => config,
            })
            .unwrap_or_default();

        let mut app = AppModel {
            core,
            context_page: ContextPage::default(),
            about,
            config: config.clone(),
            world_stations: Vec::new(),
            results: Vec::new(),
            favorites: Vec::new(),
            recent: Vec::new(),
            geo_stations: Vec::new(),
            mode: Mode::World,
            active_country_code: String::new(),
            active_country_name: String::new(),
            selected_index: None,
            keyboard_selection: false,
            search_text: String::new(),
            search_token: 0,
            search_id: widget::Id::new("radio-atlas-search"),
            fetching: None,
            pending_fetch: None,
            fetch_error: String::new(),
            local_error: String::new(),
            expansion_misses: 0,
            expansion_scheduled: false,
            player: PlayerStatus::default(),
            player_error: String::new(),
            recorded_uuid: String::new(),
            last_random_uuid: String::new(),
            poll_in_flight: true,
            player_busy: false,
            play_in_flight: false,
            play_cancel_requested: false,
            pending_play: None,
            pending_favorites: Vec::new(),
            favorite_busy: false,
            volume_display: 70,
            outputs: Vec::new(),
            outputs_error: String::new(),
            outputs_loading: false,
            output_menu: false,
            globe: GlobeState::new(
                config.center_latitude as f64,
                config.center_longitude as f64,
                embedded_countries(),
            ),
            help_visible: false,
        };

        let title = app.update_title();
        let local = cmd(async { Message::LocalState(tasks::load_local_state().await) });
        let world = app.start_fetch(FetchKind::World);
        let poll = cmd(async { Message::PlayerPolled(tasks::poll_player().await) });
        (app, Task::batch([title, local, world, poll]))
    }

    /// Search field in the middle of the header bar.
    fn header_center(&self) -> Vec<Element<'_, Self::Message>> {
        vec![
            widget::search_input(
                "Search station, country, or genre",
                self.search_text.as_str(),
            )
            .id(self.search_id.clone())
            .on_input(Message::SearchInput)
            .on_submit(|_| Message::SearchSubmit)
            .width(300)
            .into(),
        ]
    }

    /// Random-tune, help, and about actions at the end of the header bar.
    fn header_end(&self) -> Vec<Element<'_, Self::Message>> {
        vec![
            icon_button("media-playlist-shuffle-symbolic", Message::TuneRandom).into(),
            widget::button::text("?")
                .on_press(Message::ToggleHelp)
                .into(),
            icon_button(
                "help-about-symbolic",
                Message::ToggleContextPage(ContextPage::About),
            )
            .into(),
        ]
    }

    /// Display a context drawer if the context page is requested.
    fn context_drawer(&self) -> Option<context_drawer::ContextDrawer<'_, Self::Message>> {
        if !self.core.window.show_context {
            return None;
        }
        Some(match self.context_page {
            ContextPage::About => context_drawer::about(
                &self.about,
                |url| Message::LaunchUrl(url.to_string()),
                Message::ToggleContextPage(ContextPage::About),
            ),
        })
    }

    /// Describes the interface based on the current state of the application model.
    fn view(&self) -> Element<'_, Self::Message> {
        if self.help_visible {
            return self.controls_view();
        }
        widget::row::with_capacity(2)
            .push(self.globe_view())
            .push(self.sidebar_view())
            .spacing(0)
            .into()
    }

    /// Register subscriptions for this application.
    fn subscription(&self) -> Subscription<Self::Message> {
        let mut subscriptions = vec![
            self.core()
                .watch_config::<Config>(Self::APP_ID)
                .map(|update| Message::UpdateConfig(update.config)),
            keyboard::listen().map(Message::Keyboard),
            cosmic::iced::time::every(POLL_INTERVAL).map(|_| Message::PollTick),
        ];
        if self.globe.kinetic {
            subscriptions
                .push(cosmic::iced::time::every(KINETIC_INTERVAL).map(Message::KineticTick));
        }
        Subscription::batch(subscriptions)
    }

    /// Handles messages emitted by the application and its widgets.
    fn update(&mut self, message: Self::Message) -> Task<cosmic::Action<Self::Message>> {
        match message {
            Message::UpdateConfig(config) => {
                self.config = config;
            }
            Message::ToggleContextPage(context_page) => {
                if self.context_page == context_page {
                    self.core.window.show_context = !self.core.window.show_context;
                } else {
                    self.context_page = context_page;
                    self.core.window.show_context = true;
                }
            }
            Message::LaunchUrl(url) => match open::that_detached(&url) {
                Ok(()) => {}
                Err(err) => eprintln!("failed to open {url:?}: {err}"),
            },
            Message::ToggleHelp => {
                self.help_visible = !self.help_visible;
                self.output_menu = false;
            }
            Message::ToggleOutputMenu => {
                self.output_menu = !self.output_menu;
                if self.output_menu {
                    return self.refresh_outputs();
                }
            }
            Message::SearchInput(text) => {
                self.search_text = text.chars().take(128).collect();
                self.preview_search();
                self.search_token += 1;
                let token = self.search_token;
                let query = self.search_text.clone();
                return cmd(async move {
                    after(SEARCH_DEBOUNCE).await;
                    Message::SearchDebounced(token, query)
                });
            }
            Message::SearchSubmit => {
                self.search_token += 1;
                let query = self.search_text_trimmed();
                if query.is_empty() {
                    return self.show_world_inner(true);
                }
                return self.start_fetch(FetchKind::Search { query });
            }
            Message::SearchDebounced(token, query) => {
                if token == self.search_token && query == self.search_text {
                    let trimmed = self.search_text_trimmed();
                    if !trimmed.is_empty() {
                        return self.start_fetch(FetchKind::Search { query: trimmed });
                    }
                }
            }
            Message::TuneRandom => return self.tune_random(),
            Message::ShowWorld => {
                let task = self.show_world_inner(true);
                let title = self.update_title();
                return Task::batch([task, title]);
            }
            Message::ShowFavorites => {
                self.search_token += 1;
                self.pending_fetch = None;
                self.search_text.clear();
                self.mode = Mode::Favorites;
                self.restore_playing_country(true);
                self.fetch_error.clear();
                self.local_error.clear();
                let len = self.display_stations().len();
                self.set_selection(len.checked_sub(1).map(|_| 0), false);
                self.refresh_geo();
                return self.update_title();
            }
            Message::ShowRecent => {
                self.search_token += 1;
                self.pending_fetch = None;
                self.search_text.clear();
                self.mode = Mode::Recent;
                self.restore_playing_country(true);
                self.fetch_error.clear();
                self.local_error.clear();
                let len = self.display_stations().len();
                self.set_selection(len.checked_sub(1).map(|_| 0), false);
                self.refresh_geo();
                return self.update_title();
            }
            Message::FetchCompleted(kind, result) => {
                return self.finish_fetch(kind, result);
            }
            Message::WorldExpandDue => {
                self.expansion_scheduled = false;
                if self.world_stations.is_empty()
                    || self.world_stations.len() >= WORLD_STATION_LIMIT
                    || self.expansion_misses >= 3
                {
                    return Task::none();
                }
                return cmd(async { Message::WorldExpanded(tasks::fetch_world_more().await) });
            }
            Message::WorldExpanded(result) => {
                self.expansion_misses += 1;
                let Ok(stations) = result else {
                    return self.schedule_expansion(Duration::from_secs(30));
                };
                if stations.is_empty() {
                    return self.schedule_expansion(Duration::from_secs(30));
                }
                let merged =
                    geo::merge_stations(&self.world_stations, &stations, WORLD_STATION_LIMIT);
                if merged.len() == self.world_stations.len() {
                    return self.schedule_expansion(Duration::from_secs(10));
                }
                self.expansion_misses = 0;
                self.world_stations = merged;
                if self.mode == Mode::World {
                    self.results = self.world_stations.clone();
                    let len = self.results.len();
                    if let Some(selected) = self.selected_index {
                        self.selected_index = Some(selected.min(len.saturating_sub(1)));
                    }
                }
                self.refresh_geo();
                return self.schedule_expansion(Duration::from_millis(1600));
            }
            Message::LocalState(result) => {
                self.favorite_busy = false;
                match result {
                    Ok(snapshot) => return self.apply_local_snapshot(snapshot),
                    Err(_) => {
                        self.local_error = "Saved stations could not be loaded".to_string();
                        if let Some((uuid, selection)) = self.pending_favorites.pop() {
                            self.favorite_busy = true;
                            return cmd(async move {
                                Message::LocalState(tasks::toggle_favorite(uuid, selection).await)
                            });
                        }
                    }
                }
            }
            Message::PlaySelected => return self.play_selected(),
            Message::StationRowPressed(uuid) => {
                if let Some(index) = geo::index_by_uuid(self.display_stations(), &uuid) {
                    self.set_selection(Some(index), false);
                    return self.play_selected();
                }
            }
            Message::MapStationTapped(uuid) => return self.activate_map_station(&uuid),
            Message::MapCountryTapped(code) => {
                let task = self.browse_country(&code, "");
                let title = self.update_title();
                return Task::batch([task, title]);
            }
            Message::PlayerAction(command) => {
                if command == PlayerCommand::Stop {
                    self.pending_play = None;
                    if self.play_in_flight {
                        self.play_cancel_requested = true;
                    }
                    self.player_busy = true;
                    return cmd(async move {
                        Message::PlayerResult(command, tasks::run_player_command(command).await)
                    });
                }
                if self.player_busy {
                    return Task::none();
                }
                self.player_error.clear();
                self.player_busy = true;
                return cmd(async move {
                    Message::PlayerResult(command, tasks::run_player_command(command).await)
                });
            }
            Message::PlayerResult(command, result) => {
                self.player_busy = false;
                match result {
                    Err(_) => {
                        self.player_error = if command == PlayerCommand::Stop {
                            "Could not stop the player".to_string()
                        } else {
                            "Player action failed".to_string()
                        };
                    }
                    Ok(status) => {
                        let applied = self.apply_player_status(status);
                        let pending = self.play_pending();
                        return Task::batch([applied, pending]);
                    }
                }
            }
            Message::PlayCompleted(result) => {
                self.player_busy = false;
                self.play_in_flight = false;
                let canceled = self.play_cancel_requested;
                self.play_cancel_requested = false;
                match result {
                    Err(_) if !canceled => {
                        self.player_error = "Could not play this station".to_string();
                    }
                    Err(_) => {}
                    Ok(status) => {
                        let applied = self.apply_player_status(status);
                        let pending = self.play_pending();
                        return Task::batch([applied, pending]);
                    }
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
                    return self.apply_player_status(status);
                }
            }
            Message::ToggleFavorite(uuid) => return self.toggle_favorite(&uuid),
            Message::VolumeChanged(value) => {
                self.volume_display = (value.round() as i64).clamp(0, 100);
            }
            Message::VolumeCommit => {
                let volume = self.volume_display;
                return cmd(async move {
                    Message::VolumeCommitted(tasks::set_player_volume(volume).await)
                });
            }
            Message::VolumeCommitted(result) => match result {
                Ok(status) => return self.apply_player_status(status),
                Err(_) => {
                    self.volume_display = self.player.volume;
                    self.player_error = "Could not change volume".to_string();
                }
            },
            Message::OutputsLoaded(result) => {
                self.outputs_loading = false;
                match result {
                    Ok(outputs) => {
                        self.outputs = outputs;
                        self.outputs_error.clear();
                    }
                    Err(_) => {
                        self.outputs_error = "Audio outputs are unavailable".to_string();
                        self.outputs.clear();
                    }
                }
            }
            Message::SelectOutput(name) => {
                return cmd(async move {
                    Message::OutputSelected(tasks::set_player_output(name).await)
                });
            }
            Message::OutputSelected(result) => match result {
                Ok(status) => {
                    self.player_error.clear();
                    self.output_menu = false;
                    return self.apply_player_status(status);
                }
                Err(_) => {
                    self.player_error = "Could not change audio output".to_string();
                }
            },
            Message::GlobePressed => {
                self.keyboard_selection = false;
                self.globe.stop_kinetic(true);
                self.globe.hover_uuid = None;
            }
            Message::GlobeDragged(dx, dy) => {
                self.globe.center_lon = geo::wrap_longitude(
                    self.globe.center_lon - dx * globe::LON_SENSITIVITY / self.globe.scale,
                );
                self.globe.center_lat = geo::clamp(
                    self.globe.center_lat + dy * globe::LAT_SENSITIVITY / self.globe.scale,
                    geo::MIN_LATITUDE,
                    geo::MAX_LATITUDE,
                );
            }
            Message::GlobeFlick(vx, vy) => {
                let launch = geo::kinetic_launch_velocity(
                    vx,
                    vy,
                    globe::KINETIC_LAUNCH_SPEED,
                    globe::KINETIC_MAX_SPEED,
                );
                if launch.active {
                    self.globe.hover_uuid = None;
                    self.globe.landing_uuid = None;
                    self.globe.kin_vx = launch.x;
                    self.globe.kin_vy = launch.y;
                    self.globe.kinetic = true;
                    self.globe.kin_last_tick = None;
                }
            }
            Message::GlobeZoom(factor) => {
                self.globe.stop_kinetic(true);
                self.globe.hover_uuid = None;
                self.globe.scale =
                    (self.globe.scale * factor).clamp(globe::MIN_SCALE, globe::MAX_SCALE);
            }
            Message::GlobeHover(uuid) => {
                self.globe.hover_uuid = uuid;
            }
            Message::KineticTick(now) => {
                if !self.globe.kinetic {
                    return Task::none();
                }
                let Some(last) = self.globe.kin_last_tick.replace(now) else {
                    return Task::none();
                };
                let elapsed = now.duration_since(last).as_secs_f64();
                if elapsed > globe::KINETIC_MAX_FRAME_TIME {
                    self.globe.stop_kinetic(true);
                    return Task::none();
                }
                if elapsed <= 0.0 {
                    return Task::none();
                }
                let step = geo::advance_kinetic_rotation(
                    geo::KineticState {
                        longitude: self.globe.center_lon,
                        latitude: self.globe.center_lat,
                        velocity_x: self.globe.kin_vx,
                        velocity_y: self.globe.kin_vy,
                    },
                    elapsed,
                    geo::KineticOptions {
                        deceleration: globe::KINETIC_DECELERATION,
                        scale: self.globe.scale,
                        longitude_sensitivity: globe::LON_SENSITIVITY,
                        latitude_sensitivity: globe::LAT_SENSITIVITY,
                        minimum_latitude: geo::MIN_LATITUDE,
                        maximum_latitude: geo::MAX_LATITUDE,
                    },
                );
                self.globe.kin_vx = step.velocity_x;
                self.globe.kin_vy = step.velocity_y;
                self.globe.center_lon = step.longitude;
                self.globe.center_lat = step.latitude;
                if !step.active {
                    self.globe.kinetic = false;
                    self.globe.kin_vx = 0.0;
                    self.globe.kin_vy = 0.0;
                    let excluded = if self.playing_uuid().is_empty() {
                        self.selected_station()
                            .map(|station| station.uuid.as_str())
                            .unwrap_or("")
                    } else {
                        self.playing_uuid()
                    };
                    self.globe.landing_uuid = geo::nearest_visible_station(
                        &self.geo_stations,
                        self.globe.center_lat,
                        self.globe.center_lon,
                        excluded,
                        None,
                    )
                    .map(|station| station.uuid.clone());
                }
            }
            Message::Keyboard(event) => return self.handle_key(event),
        }
        Task::none()
    }
}

/// The context page to display in the context drawer.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum ContextPage {
    #[default]
    About,
}

fn icon_button(name: &'static str, message: Message) -> widget::Button<'static, Message> {
    widget::button::custom(widget::icon::from_name(name).size(16))
        .on_press(message)
        .padding(8)
}

impl AppModel {
    fn finish_fetch(
        &mut self,
        kind: FetchKind,
        result: Result<Vec<Station>, String>,
    ) -> Task<cosmic::Action<Message>> {
        if kind == FetchKind::World {
            if let Ok(stations) = &result {
                self.world_stations =
                    geo::merge_stations(&self.world_stations, stations, WORLD_STATION_LIMIT);
            }
        }
        if let Some(pending) = self.pending_fetch.take() {
            self.fetching = None;
            if self.mode_matches(&pending) {
                return self.start_fetch(pending);
            }
            return Task::none();
        }
        self.fetching = None;
        if !self.mode_matches(&kind) {
            return Task::none();
        }
        if let FetchKind::Search { query } = &kind {
            if self.search_text_trimmed() != *query {
                return Task::none();
            }
        }
        let stations = match result {
            Ok(stations) => stations,
            Err(_) => {
                self.fetch_error = if self.display_stations().is_empty() {
                    "Radio Browser is unavailable. Try again shortly.".to_string()
                } else {
                    "Showing cached stations · Radio Browser is unavailable".to_string()
                };
                return Task::none();
            }
        };
        self.fetch_error.clear();
        match kind {
            FetchKind::World => {
                let world = self.world_stations.clone();
                self.set_station_list(Mode::World, world);
                let expand = self.schedule_expansion(Duration::from_millis(1200));
                let title = self.update_title();
                Task::batch([expand, title])
            }
            FetchKind::Country { code } => {
                let merged = geo::merge_stations(&self.results.clone(), &stations, 500);
                let world = std::mem::take(&mut self.world_stations);
                self.world_stations =
                    geo::prioritize_stations(&merged, &world, WORLD_STATION_LIMIT);
                let name = match &self.mode {
                    Mode::Country { name, .. } => name.clone(),
                    _ => code.clone(),
                };
                self.set_station_list(Mode::Country { code, name }, merged);
                self.update_title()
            }
            FetchKind::Search { query } => {
                self.set_station_list(Mode::Search { query }, stations);
                self.update_title()
            }
            FetchKind::Random => {
                self.set_station_list(Mode::Random, stations);
                if let Some(first) = self.display_stations().first() {
                    self.last_random_uuid = first.uuid.clone();
                }
                let play = self.play_selected();
                let title = self.update_title();
                Task::batch([play, title])
            }
        }
    }

    fn tune_random(&mut self) -> Task<cosmic::Action<Message>> {
        self.search_token += 1;
        self.pending_fetch = None;
        self.search_text.clear();
        self.set_station_list(Mode::Random, Vec::new());
        self.restore_playing_country(false);
        self.fetch_error.clear();
        self.start_fetch(FetchKind::Random)
    }

    fn command_task(&mut self, command: PlayerCommand) -> Task<cosmic::Action<Message>> {
        if self.player_busy {
            return Task::none();
        }
        self.player_error.clear();
        self.player_busy = true;
        cmd(async move { Message::PlayerResult(command, tasks::run_player_command(command).await) })
    }

    fn volume_step_task(&mut self, delta: i64) -> Task<cosmic::Action<Message>> {
        self.volume_display = (self.volume_display + delta).clamp(0, 100);
        let volume = self.volume_display;
        cmd(async move { Message::VolumeCommitted(tasks::set_player_volume(volume).await) })
    }

    fn refresh_outputs(&mut self) -> Task<cosmic::Action<Message>> {
        if self.outputs_loading {
            return Task::none();
        }
        self.outputs_loading = true;
        self.outputs_error.clear();
        cmd(async { Message::OutputsLoaded(tasks::list_player_outputs().await) })
    }

    fn handle_key(&mut self, event: keyboard::Event) -> Task<cosmic::Action<Message>> {
        let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
            return Task::none();
        };
        if modifiers.control() || modifiers.logo() {
            return Task::none();
        }
        match key {
            keyboard::Key::Named(name) => {
                use cosmic::iced::keyboard::key::Named;
                match name {
                    Named::ArrowUp => {
                        self.move_selection(-1);
                        Task::none()
                    }
                    Named::ArrowDown => {
                        self.move_selection(1);
                        Task::none()
                    }
                    Named::Enter => self.play_selected(),
                    Named::Escape => {
                        if self.help_visible {
                            self.help_visible = false;
                        } else if self.output_menu {
                            self.output_menu = false;
                        } else if !self.search_text.is_empty() {
                            return self.show_world_inner(true);
                        }
                        Task::none()
                    }
                    _ => Task::none(),
                }
            }
            keyboard::Key::Character(text) => {
                let text = text.as_str();
                if text == " " {
                    if self.player.running {
                        return self.command_task(PlayerCommand::Toggle);
                    }
                    return self.play_selected();
                }
                match text.to_ascii_lowercase().as_str() {
                    "r" => self.tune_random(),
                    "f" => {
                        if let Some(station) = self.selected_station().cloned() {
                            self.toggle_favorite(&station.uuid)
                        } else {
                            Task::none()
                        }
                    }
                    "m" => self.command_task(PlayerCommand::Mute),
                    "+" | "=" => self.volume_step_task(5),
                    "-" | "_" => self.volume_step_task(-5),
                    "/" => {
                        widget::text_input::focus(self.search_id.clone()).map(cosmic::Action::App)
                    }
                    "?" => {
                        self.help_visible = !self.help_visible;
                        self.output_menu = false;
                        Task::none()
                    }
                    _ => Task::none(),
                }
            }
            _ => Task::none(),
        }
    }

    // --- Views ------------------------------------------------------------------

    fn map_hint_text(&self) -> String {
        let featured = self
            .globe
            .hover_uuid
            .as_deref()
            .or(self.globe.landing_uuid.as_deref())
            .and_then(|uuid| {
                self.geo_stations
                    .iter()
                    .find(|station| station.uuid == uuid)
            });
        if let Some(station) = featured {
            let landing = self.globe.hover_uuid.is_none();
            return format!(
                "{}{} — click to play",
                if landing { "Landed near · " } else { "" },
                station.name
            );
        }
        if !self.fetch_error.is_empty() {
            return self.fetch_error.clone();
        }
        if !self.local_error.is_empty() {
            return self.local_error.clone();
        }
        if !self.active_country_name.is_empty() {
            return format!(
                "{}  ·  click another country to browse",
                self.active_country_name
            );
        }
        "Drag or flick to spin  ·  wheel to zoom  ·  click a signal or country".to_string()
    }

    fn globe_view(&self) -> Element<'_, Message> {
        let selected = if !self.playing_uuid().is_empty() {
            self.playing_uuid()
        } else {
            self.selected_station()
                .map(|station| station.uuid.as_str())
                .unwrap_or("")
        };
        let canvas = widget::Canvas::new(GlobeProgram {
            state: &self.globe,
            stations: &self.geo_stations,
            selected_uuid: selected,
            active_country: &self.active_country_code,
            features: embedded_countries(),
        })
        .width(Length::Fill)
        .height(Length::Fill);
        widget::container(
            widget::column::with_capacity(2)
                .push(canvas)
                .push(
                    widget::row::with_capacity(2)
                        .push(widget::text::caption(self.map_hint_text()).width(Length::Fill))
                        .push(widget::text::caption(format!(
                            "{} signals",
                            self.geo_stations.len()
                        )))
                        .spacing(8),
                )
                .spacing(4),
        )
        .padding(12)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    fn sidebar_view(&self) -> Element<'_, Message> {
        let tabs = widget::row::with_capacity(3)
            .push(tab_button(
                "World",
                self.mode == Mode::World,
                Message::ShowWorld,
            ))
            .push(tab_button(
                "Favorites",
                self.mode == Mode::Favorites,
                Message::ShowFavorites,
            ))
            .push(tab_button(
                "Recent",
                self.mode == Mode::Recent,
                Message::ShowRecent,
            ))
            .spacing(8);
        let list = if self.output_menu {
            self.outputs_view()
        } else {
            self.stations_view()
        };
        widget::container(
            widget::column::with_capacity(5)
                .push(tabs)
                .push(widget::divider::horizontal::default())
                .push(list)
                .push(widget::divider::horizontal::default())
                .push(self.player_view())
                .spacing(8),
        )
        .padding(12)
        .width(390)
        .height(Length::Fill)
        .into()
    }

    fn stations_view(&self) -> Element<'_, Message> {
        use cosmic::iced::alignment::{Horizontal, Vertical};
        let display = self.display_stations();
        if self.fetching.is_some() && self.remote_mode() && display.is_empty() {
            return widget::container(widget::text::caption("LOADING STATIONS"))
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Horizontal::Center)
                .align_y(Vertical::Center)
                .into();
        }
        if display.is_empty() {
            return widget::container(widget::text::body(self.empty_state_text()))
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Horizontal::Center)
                .align_y(Vertical::Center)
                .into();
        }
        let mut column =
            widget::column::with_capacity(display.len().min(MAX_VISIBLE_ROWS) + 1).spacing(2);
        for (index, station) in display.iter().take(MAX_VISIBLE_ROWS).enumerate() {
            column = column.push(self.station_row(index, station));
        }
        if display.len() > MAX_VISIBLE_ROWS {
            column = column.push(widget::text::caption(format!(
                "Showing {MAX_VISIBLE_ROWS} of {} — search to refine",
                display.len()
            )));
        }
        widget::scrollable(column)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn station_row(&self, index: usize, station: &Station) -> Element<'_, Message> {
        let playing = self.playing_uuid() == station.uuid;
        let selected = self.selected_index == Some(index);
        let favorite = self.is_favorite(&station.uuid);
        let mut meta = geo::station_meta(station);
        let tags = geo::compact_tags(&station.tags, 2);
        if !tags.is_empty() {
            meta.push_str("  ·  ");
            meta.push_str(&tags);
        }
        let name: Element<'_, Message> = if playing {
            widget::row::with_capacity(2)
                .push(widget::icon::from_name("media-playback-start-symbolic").size(12))
                .push(widget::text::body(station.name.clone()).width(Length::Fill))
                .spacing(6)
                .into()
        } else {
            widget::text::body(station.name.clone())
                .width(Length::Fill)
                .into()
        };
        let row = widget::row::with_capacity(2)
            .push(
                widget::button::custom(
                    widget::column::with_capacity(2)
                        .push(name)
                        .push(widget::text::caption(meta))
                        .spacing(2)
                        .width(Length::Fill),
                )
                .on_press(Message::StationRowPressed(station.uuid.clone()))
                .padding(8)
                .width(Length::Fill),
            )
            .push(
                widget::button::custom(
                    widget::icon::from_name("emblem-favorite-symbolic").size(16),
                )
                .class(if favorite {
                    cosmic::theme::Button::Suggested
                } else {
                    cosmic::theme::Button::Text
                })
                .on_press(Message::ToggleFavorite(station.uuid.clone()))
                .padding(8),
            )
            .spacing(4);
        if selected || playing {
            widget::container(row)
                .class(cosmic::theme::Container::Card)
                .width(Length::Fill)
                .into()
        } else {
            row.into()
        }
    }

    fn outputs_view(&self) -> Element<'_, Message> {
        let mut column = widget::column::with_capacity(self.outputs.len() + 3).spacing(4);
        column = column.push(widget::text::body("Audio output"));
        column = column.push(output_row(
            "System default".to_string(),
            String::new(),
            self.player.output.is_empty(),
        ));
        for output in &self.outputs {
            column = column.push(output_row(
                output.label.clone(),
                output.id.clone(),
                self.player.output == output.id,
            ));
        }
        if self.outputs_loading {
            column = column.push(widget::text::caption("Loading outputs…"));
        } else if self.outputs.is_empty() {
            column = column.push(widget::text::caption(if self.outputs_error.is_empty() {
                "No other audio outputs found"
            } else {
                self.outputs_error.as_str()
            }));
        }
        column = column.push(
            widget::button::standard("Back")
                .on_press(Message::ToggleOutputMenu)
                .width(Length::Fill),
        );
        widget::scrollable(column)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn player_view(&self) -> Element<'_, Message> {
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
        let status = if !self.player_error.is_empty() {
            self.player_error.clone()
        } else if !self.player.error.is_empty() {
            format!("{}. Play to retry, or Next.", self.player.error)
        } else if !self.player.running {
            "Choose a signal to begin".to_string()
        } else {
            let mut parts = Vec::new();
            let track = self.playing_track_title();
            if !track.is_empty() {
                parts.push(track);
            }
            parts.push(if self.player.paused { "Paused" } else { "Live" }.to_string());
            if self.player.playlist_count > 1 {
                parts.push(format!("{} stations queued", self.player.playlist_count));
            }
            parts.join("  ·  ")
        };
        let playing = self.player.running && !self.player.paused;
        let transport = widget::row::with_capacity(4)
            .push(transport_button(
                "media-skip-backward-symbolic",
                Message::PlayerAction(PlayerCommand::Previous),
                self.player.running && !self.player_busy,
            ))
            .push(transport_button(
                if playing {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                },
                if self.player.running {
                    Message::PlayerAction(PlayerCommand::Toggle)
                } else {
                    Message::PlaySelected
                },
                !self.player_busy,
            ))
            .push(transport_button(
                "media-skip-forward-symbolic",
                Message::PlayerAction(PlayerCommand::Next),
                self.player.running && !self.player_busy,
            ))
            .push(transport_button(
                "media-playback-stop-symbolic",
                Message::PlayerAction(PlayerCommand::Stop),
                self.player.running || self.play_in_flight,
            ))
            .spacing(4);
        let volume = widget::row::with_capacity(4)
            .push(icon_button(
                "audio-speakers-symbolic",
                Message::ToggleOutputMenu,
            ))
            .push(transport_button(
                if self.player.muted {
                    "audio-volume-muted-symbolic"
                } else {
                    "audio-volume-high-symbolic"
                },
                Message::PlayerAction(PlayerCommand::Mute),
                self.player.running && !self.player_busy,
            ))
            .push(
                widget::Slider::new(
                    0.0..=100.0,
                    self.volume_display as f32,
                    Message::VolumeChanged,
                )
                .step(1.0_f32)
                .on_release(Message::VolumeCommit)
                .width(110),
            )
            .push(widget::text::caption(format!("{}%", self.volume_display)))
            .spacing(4)
            .align_y(cosmic::iced::Alignment::Center);
        widget::column::with_capacity(4)
            .push(widget::text::body(name))
            .push(widget::text::caption(status))
            .push(transport)
            .push(volume)
            .spacing(6)
            .into()
    }

    fn controls_view(&self) -> Element<'_, Message> {
        let mut sections = widget::row::with_capacity(CONTROLS.len()).spacing(48);
        for (title, controls) in CONTROLS {
            let mut column = widget::column::with_capacity(controls.len() + 1).spacing(8);
            column = column.push(widget::text::body(title.to_string()));
            for (input, action) in *controls {
                column = column.push(
                    widget::row::with_capacity(2)
                        .push(widget::text::caption(input.to_string()).width(140))
                        .push(widget::text::body(action.to_string()).width(Length::Fill))
                        .spacing(12),
                );
            }
            sections = sections.push(column.width(Length::FillPortion(1)));
        }
        widget::container(
            widget::column::with_capacity(3)
                .push(widget::text::title1("Controls"))
                .push(sections)
                .push(
                    widget::button::standard("Back")
                        .on_press(Message::ToggleHelp)
                        .width(160),
                )
                .spacing(24),
        )
        .padding(48)
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
    }
}

fn tab_button(label: &'static str, active: bool, message: Message) -> Element<'static, Message> {
    let button = if active {
        widget::button::suggested(label)
    } else {
        widget::button::standard(label)
    };
    button.on_press(message).into()
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

fn output_row(label: String, id: String, selected: bool) -> Element<'static, Message> {
    let button = if selected {
        widget::button::suggested(label)
    } else {
        widget::button::standard(label)
    };
    button
        .on_press(Message::SelectOutput(id))
        .width(Length::Fill)
        .into()
}

const CONTROLS: &[(&str, &[(&str, &str)])] = &[
    (
        "KEYBOARD",
        &[
            ("/", "Search"),
            ("UP / DOWN", "Select station"),
            ("ENTER", "Play selected station"),
            ("SPACE", "Play or pause"),
            ("R", "Tune randomly"),
            ("F", "Favorite selected station"),
            ("M", "Mute or unmute"),
            ("+ / -", "Change volume"),
            ("ESC", "Back or clear"),
            ("?", "Show or hide controls"),
        ],
    ),
    (
        "GLOBE AND PLAYBACK",
        &[
            ("DRAG / FLICK", "Spin globe"),
            ("GLOBE WHEEL", "Zoom"),
            ("CLICK SIGNAL", "Play station"),
            ("CLICK COUNTRY", "Browse stations"),
            ("SPEAKER", "Choose audio output"),
        ],
    ),
];
