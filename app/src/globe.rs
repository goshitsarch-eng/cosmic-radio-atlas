// SPDX-License-Identifier: MIT

//! Interactive world-radio globe rendered on an iced canvas.
//!
//! Ports `Globe.qml`: an orthographic globe with country fills, graticule,
//! station signals, drag/flick kinetic rotation, wheel zoom, and tap
//! selection. Hover and landing-callout details are reported to the status
//! line under the globe instead of floating tooltips.

use std::collections::HashMap;
use std::time::Instant;

use cosmic::iced::mouse;
use cosmic::iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use cosmic::iced::{Color, Point, Rectangle};
use cosmic::{Renderer, Theme};
use radio_core::countries::Feature;
use radio_core::geo::{self, Coord};
use radio_core::model::Station;

use crate::app::Message;

pub const MIN_SCALE: f64 = 0.72;
pub const MAX_SCALE: f64 = 24.0;
pub const LON_SENSITIVITY: f64 = 0.22;
pub const LAT_SENSITIVITY: f64 = 0.18;
pub const KINETIC_LAUNCH_SPEED: f64 = 120.0;
pub const KINETIC_MAX_SPEED: f64 = 2400.0;
pub const KINETIC_DECELERATION: f64 = 1800.0;
pub const KINETIC_MAX_FRAME_TIME: f64 = 0.1;
pub const KINETIC_MAX_SAMPLE_AGE_MS: f64 = 100.0;
pub const STATION_HIT_RADIUS: f32 = 12.0;
const DRAG_THRESHOLD: f32 = 4.0;

/// World-space country geometry prepared once at startup.
#[derive(Debug, Clone)]
pub struct PreparedCountry {
    pub code: String,
    pub rings: Vec<Vec<[f64; 3]>>,
}

fn prepare_ring(ring: &[[f64; 2]]) -> Vec<[f64; 3]> {
    ring.iter()
        .filter_map(|point| {
            let lat = point[1] * geo::RADIANS;
            let lon = point[0] * geo::RADIANS;
            if !lat.is_finite() || !lon.is_finite() {
                return None;
            }
            Some([lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()])
        })
        .collect()
}

pub fn prepare_countries(features: &[Feature]) -> Vec<PreparedCountry> {
    features
        .iter()
        .filter_map(|feature| {
            let mut rings = Vec::new();
            for polygon in feature.geometry.polygons() {
                let Some(outer) = polygon.first() else {
                    continue;
                };
                let prepared = prepare_ring(outer);
                if prepared.len() >= 3 {
                    rings.push(prepared);
                }
            }
            if rings.is_empty() {
                return None;
            }
            Some(PreparedCountry {
                code: feature.properties.code.to_ascii_uppercase(),
                rings,
            })
        })
        .collect()
}

pub fn prepare_grid() -> Vec<Vec<[f64; 3]>> {
    let mut lines = Vec::new();
    let mut lat = -60;
    while lat <= 60 {
        let mut line = Vec::new();
        let mut lon = -180;
        while lon <= 180 {
            line.push([lon as f64, lat as f64]);
            lon += 3;
        }
        lines.push(prepare_ring(&line));
        lat += 30;
    }
    let mut meridian = -150;
    while meridian <= 180 {
        let mut line = Vec::new();
        let mut lat = -90;
        while lat <= 90 {
            line.push([meridian as f64, lat as f64]);
            lat += 3;
        }
        lines.push(prepare_ring(&line));
        meridian += 30;
    }
    lines.into_iter().filter(|line| line.len() >= 2).collect()
}

/// App-owned globe camera and highlight state.
#[derive(Debug)]
pub struct GlobeState {
    pub center_lat: f64,
    pub center_lon: f64,
    pub scale: f64,
    pub kin_vx: f64,
    pub kin_vy: f64,
    pub kinetic: bool,
    pub kin_last_tick: Option<Instant>,
    pub hover_uuid: Option<String>,
    pub landing_uuid: Option<String>,
    pub estimates: HashMap<String, Coord>,
    pub grid: Vec<Vec<[f64; 3]>>,
    pub countries: Vec<PreparedCountry>,
}

impl GlobeState {
    pub fn new(center_lat: f64, center_lon: f64, features: &[Feature]) -> Self {
        GlobeState {
            center_lat: center_lat.clamp(geo::MIN_LATITUDE, geo::MAX_LATITUDE),
            center_lon: geo::wrap_longitude(center_lon),
            scale: 1.0,
            kin_vx: 0.0,
            kin_vy: 0.0,
            kinetic: false,
            kin_last_tick: None,
            hover_uuid: None,
            landing_uuid: None,
            estimates: HashMap::new(),
            grid: prepare_grid(),
            countries: prepare_countries(features),
        }
    }

    pub fn radius(&self, width: f32, height: f32) -> f32 {
        width.min(height) * 0.44 * self.scale as f32
    }

    pub fn stop_kinetic(&mut self, clear_landing: bool) {
        self.kin_vx = 0.0;
        self.kin_vy = 0.0;
        self.kinetic = false;
        self.kin_last_tick = None;
        if clear_landing {
            self.landing_uuid = None;
        }
    }

    pub fn focus_coordinate(&mut self, lat: f64, lon: f64) {
        if !lat.is_finite() || !lon.is_finite() {
            return;
        }
        self.stop_kinetic(true);
        self.center_lat = lat.clamp(geo::MIN_LATITUDE, geo::MAX_LATITUDE);
        self.center_lon = geo::wrap_longitude(lon);
    }
}

/// Canvas-local press/drag tracking.
#[derive(Debug, Default)]
pub struct GlobeInteraction {
    press: Option<Press>,
    last_hover: Option<String>,
}

#[derive(Debug, Clone)]
struct Press {
    origin: Point,
    last: Point,
    moved: bool,
    sample_vx: f32,
    sample_vy: f32,
    sample_at: Option<Instant>,
    sample_pos: Point,
    sample_started: Instant,
}

pub struct GlobeProgram<'a> {
    pub state: &'a GlobeState,
    pub stations: &'a [Station],
    pub selected_uuid: &'a str,
    pub active_country: &'a str,
    pub features: &'a [Feature],
}

impl GlobeProgram<'_> {
    fn station_at(&self, x: f32, y: f32, width: f32, height: f32) -> Option<&Station> {
        let mut nearest: Option<(&Station, f32)> = None;
        for station in self.stations {
            let (Some(lat), Some(lon)) = (station.latitude, station.longitude) else {
                continue;
            };
            let point = geo::project(lat, lon, self.state.center_lat, self.state.center_lon);
            if point.z < 0.0 {
                continue;
            }
            let radius = self.state.radius(width, height) as f64;
            let sx = width as f64 / 2.0 + point.x * radius;
            let sy = height as f64 / 2.0 - point.y * radius;
            let distance = ((sx - x as f64).powi(2) + (sy - y as f64).powi(2)).sqrt() as f32;
            let best = nearest.map(|(_, d)| d).unwrap_or(STATION_HIT_RADIUS);
            if distance <= best {
                nearest = Some((station, distance));
            }
        }
        nearest.map(|(station, _)| station)
    }
}

impl canvas::Program<Message, Theme, Renderer> for GlobeProgram<'_> {
    type State = GlobeInteraction;

    fn update(
        &self,
        interaction: &mut Self::State,
        event: &canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        let canvas::Event::Mouse(mouse_event) = event else {
            return None;
        };
        match mouse_event {
            mouse::Event::ButtonPressed(mouse::Button::Left) => {
                let position = cursor.position_in(bounds)?;
                let now = Instant::now();
                interaction.press = Some(Press {
                    origin: position,
                    last: position,
                    moved: false,
                    sample_vx: 0.0,
                    sample_vy: 0.0,
                    sample_at: None,
                    sample_pos: position,
                    sample_started: now,
                });
                interaction.last_hover = None;
                Some(canvas::Action::publish(Message::GlobePressed).and_capture())
            }
            mouse::Event::CursorMoved { position } => {
                let position_in = cursor.position_in(bounds).unwrap_or(*position);
                if let Some(press) = interaction.press.as_mut() {
                    let dx = position_in.x - press.last.x;
                    let dy = position_in.y - press.last.y;
                    press.last = position_in;
                    // Keep a short blended velocity sample for flicks.
                    let now = Instant::now();
                    let elapsed = now.duration_since(press.sample_started).as_secs_f32() * 1000.0;
                    if elapsed > 0.0 && elapsed <= 250.0 {
                        let vx = dx * 1000.0 / elapsed;
                        let vy = dy * 1000.0 / elapsed;
                        if vx.is_finite() && vy.is_finite() {
                            if press.sample_at.is_some() {
                                press.sample_vx = press.sample_vx * 0.25 + vx * 0.75;
                                press.sample_vy = press.sample_vy * 0.25 + vy * 0.75;
                            } else {
                                press.sample_vx = vx;
                                press.sample_vy = vy;
                            }
                            press.sample_at = Some(now);
                        }
                    }
                    press.sample_pos = position_in;
                    press.sample_started = now;
                    if !press.moved
                        && (position_in.x - press.origin.x).hypot(position_in.y - press.origin.y)
                            > DRAG_THRESHOLD
                    {
                        press.moved = true;
                    }
                    if press.moved {
                        return Some(
                            canvas::Action::publish(Message::GlobeDragged(dx as f64, dy as f64))
                                .and_capture(),
                        );
                    }
                    return Some(canvas::Action::capture());
                }
                if !cursor.is_over(bounds) {
                    if interaction.last_hover.take().is_some() {
                        return Some(canvas::Action::publish(Message::GlobeHover(None)));
                    }
                    return None;
                }
                let hovered = self
                    .station_at(position_in.x, position_in.y, bounds.width, bounds.height)
                    .map(|station| station.uuid.clone());
                if hovered != interaction.last_hover {
                    interaction.last_hover = hovered.clone();
                    return Some(canvas::Action::publish(Message::GlobeHover(hovered)));
                }
                None
            }
            mouse::Event::ButtonReleased(mouse::Button::Left) => {
                let press = interaction.press.take()?;
                let position = cursor.position_in(bounds).unwrap_or(press.last);
                if !press.moved {
                    // Tap: hit-test here where the canvas bounds are known.
                    if let Some(station) =
                        self.station_at(position.x, position.y, bounds.width, bounds.height)
                    {
                        return Some(
                            canvas::Action::publish(Message::MapStationTapped(
                                station.uuid.clone(),
                            ))
                            .and_capture(),
                        );
                    }
                    let radius = self.state.radius(bounds.width, bounds.height) as f64;
                    if radius > 0.0 {
                        let nx = (position.x as f64 - bounds.width as f64 / 2.0) / radius;
                        let ny = -(position.y as f64 - bounds.height as f64 / 2.0) / radius;
                        if let Some(coord) =
                            geo::unproject(nx, ny, self.state.center_lat, self.state.center_lon)
                        {
                            if let Some(country) =
                                geo::country_at(self.features, coord.latitude, coord.longitude)
                            {
                                let code = country.properties.code.to_ascii_uppercase();
                                if !code.is_empty() && code != "-99" {
                                    return Some(
                                        canvas::Action::publish(Message::MapCountryTapped(code))
                                            .and_capture(),
                                    );
                                }
                            }
                        }
                    }
                    return Some(canvas::Action::capture());
                }
                let age_ms = press
                    .sample_at
                    .map(|at| Instant::now().duration_since(at).as_secs_f64() * 1000.0)
                    .unwrap_or(f64::INFINITY);
                let (vx, vy) = geo::kinetic_release_velocity(
                    0.0,
                    0.0,
                    press.sample_vx as f64,
                    press.sample_vy as f64,
                    age_ms,
                    KINETIC_MAX_SAMPLE_AGE_MS,
                );
                Some(canvas::Action::publish(Message::GlobeFlick(vx, vy)).and_capture())
            }
            mouse::Event::WheelScrolled { delta } => {
                if !cursor.is_over(bounds) {
                    return None;
                }
                let pixels = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y * 120.0,
                    mouse::ScrollDelta::Pixels { y, .. } => *y,
                };
                let factor = (pixels as f64 / 720.0).exp();
                Some(canvas::Action::publish(Message::GlobeZoom(factor)).and_capture())
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        interaction: &Self::State,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<Renderer>> {
        let mut frame = Frame::new(renderer, bounds.size());
        let (width, height) = (bounds.width, bounds.height);
        let radius = self.state.radius(width, height);
        if !radius.is_finite() || radius <= 0.0 {
            return vec![frame.into_geometry()];
        }
        let cosmic = theme.cosmic();
        let srgb = cosmic.accent_color();
        let accent = Color::from_rgba(srgb.red, srgb.green, srgb.blue, srgb.alpha);
        let palette = Palette::new(theme.theme_type.is_dark(), accent);
        let (cx, cy) = (width / 2.0, height / 2.0);

        frame.fill_rectangle(Point::new(0.0, 0.0), bounds.size(), palette.background);

        // Sphere with a soft diagonal light falloff.
        let sphere = Path::circle(Point::new(cx, cy), radius);
        let gradient = canvas::gradient::Linear::new(
            Point::new(cx - radius * 0.4, cy - radius * 0.45),
            Point::new(cx + radius * 0.6, cy + radius * 0.6),
        )
        .add_stop(0.0, palette.sphere_light)
        .add_stop(0.55, palette.sphere)
        .add_stop(1.0, palette.sphere_dark);
        frame.fill(&sphere, canvas::gradient::Gradient::Linear(gradient));

        let lat0 = self.state.center_lat * geo::RADIANS;
        let lon0 = self.state.center_lon * geo::RADIANS;
        let (sin_lat, cos_lat) = lat0.sin_cos();
        let (sin_lon, cos_lon) = lon0.sin_cos();
        let project = |world: [f64; 3]| -> (f32, f32, f64) {
            let horizontal = world[0] * cos_lon + world[1] * sin_lon;
            let x = world[1] * cos_lon - world[0] * sin_lon;
            let y = cos_lat * world[2] - sin_lat * horizontal;
            let depth = sin_lat * world[2] + cos_lat * horizontal;
            (cx + x as f32 * radius, cy - y as f32 * radius, depth)
        };

        // Graticule.
        let grid_width = (radius / 500.0).clamp(0.7, 1.5);
        let mut grid = canvas::path::Builder::new();
        for line in &self.state.grid {
            let mut drawing = false;
            for &world in line {
                let (x, y, depth) = project(world);
                if depth < 0.0 {
                    drawing = false;
                    continue;
                }
                if drawing {
                    grid.line_to(Point::new(x, y));
                } else {
                    grid.move_to(Point::new(x, y));
                    drawing = true;
                }
            }
        }
        frame.stroke(
            &grid.build(),
            Stroke {
                style: canvas::Style::Solid(palette.grid),
                width: grid_width,
                ..Stroke::default()
            },
        );

        // Countries: fill fully-visible rings; stroke visible runs plus a
        // chord-closed fill for rings crossing the limb.
        for country in &self.state.countries {
            let active = country.code == self.active_country;
            let fill = if active {
                palette.active_fill
            } else {
                palette.land
            };
            let edge = if active {
                palette.active_edge
            } else {
                palette.outline
            };
            let edge_width = if active { 1.5 } else { 0.7 };
            for ring in &country.rings {
                let projected: Vec<(f32, f32, f64)> = ring.iter().map(|&w| project(w)).collect();
                if projected.iter().all(|(_, _, d)| *d >= 0.0) {
                    let path = Path::new(|builder| {
                        for (i, &(x, y, _)) in projected.iter().enumerate() {
                            if i == 0 {
                                builder.move_to(Point::new(x, y));
                            } else {
                                builder.line_to(Point::new(x, y));
                            }
                        }
                        builder.close();
                    });
                    frame.fill(&path, fill);
                    frame.stroke(
                        &path,
                        Stroke {
                            style: canvas::Style::Solid(edge),
                            width: edge_width,
                            ..Stroke::default()
                        },
                    );
                    continue;
                }
                // Partially visible: stroke each visible run; fill the runs
                // closed by chords so paint never leaves the disc.
                let mut outline = canvas::path::Builder::new();
                let mut fill_path = canvas::path::Builder::new();
                let mut drawing = false;
                let mut fill_drawing = false;
                let mut first_visible: Option<(f32, f32)> = None;
                for &(x, y, depth) in &projected {
                    if depth < 0.0 {
                        drawing = false;
                        // Chord-close the run so its fill never leaves the disc.
                        if fill_drawing {
                            if let Some(first) = first_visible {
                                fill_path.line_to(Point::new(first.0, first.1));
                            }
                            first_visible = None;
                            fill_drawing = false;
                        }
                        continue;
                    }
                    if drawing {
                        outline.line_to(Point::new(x, y));
                    } else {
                        outline.move_to(Point::new(x, y));
                        drawing = true;
                    }
                    if fill_drawing {
                        fill_path.line_to(Point::new(x, y));
                    } else {
                        fill_path.move_to(Point::new(x, y));
                        fill_drawing = true;
                        first_visible = Some((x, y));
                    }
                }
                if let (Some(first), true) = (first_visible, fill_drawing) {
                    fill_path.line_to(Point::new(first.0, first.1));
                }
                frame.fill(&fill_path.build(), fill);
                frame.stroke(
                    &outline.build(),
                    Stroke {
                        style: canvas::Style::Solid(edge),
                        width: edge_width,
                        ..Stroke::default()
                    },
                );
            }
        }

        // Station signals.
        let margin = STATION_HIT_RADIUS;
        for station in self.stations {
            let (Some(lat), Some(lon)) = (station.latitude, station.longitude) else {
                continue;
            };
            let point = geo::project(lat, lon, self.state.center_lat, self.state.center_lon);
            if point.z < 0.0 {
                continue;
            }
            let x = cx as f64 + point.x * radius as f64;
            let y = cy as f64 - point.y * radius as f64;
            if x < -margin as f64
                || x > width as f64 + margin as f64
                || y < -margin as f64
                || y > height as f64 + margin as f64
            {
                continue;
            }
            let selected = station.uuid == self.selected_uuid;
            let highlighted = !selected
                && (Some(station.uuid.as_str()) == self.state.landing_uuid.as_deref()
                    || interaction.last_hover.as_deref() == Some(station.uuid.as_str()));
            let dot_radius = if selected {
                4.2
            } else if highlighted {
                3.7
            } else {
                (1.7 + point.z * 1.25) as f32
            };
            let mut dot = palette.signal;
            dot.a = if selected || highlighted {
                1.0
            } else {
                (0.42 + point.z * 0.48) as f32
            };
            let dot_color = if selected || highlighted {
                palette.accent
            } else {
                dot
            };
            frame.fill(
                &Path::circle(Point::new(x as f32, y as f32), dot_radius),
                dot_color,
            );
            if selected || highlighted {
                let mut ring = palette.accent;
                ring.a = if selected { 0.72 } else { 0.92 };
                frame.stroke(
                    &Path::circle(
                        Point::new(x as f32, y as f32),
                        if selected { 8.5 } else { 7.5 },
                    ),
                    Stroke {
                        style: canvas::Style::Solid(ring),
                        width: if selected { 1.2 } else { 1.4 },
                        ..Stroke::default()
                    },
                );
            }
        }

        // Limb.
        frame.stroke(
            &Path::circle(Point::new(cx, cy), radius),
            Stroke {
                style: canvas::Style::Solid(palette.limb),
                width: 1.1,
                ..Stroke::default()
            },
        );

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        interaction: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if interaction.press.as_ref().is_some_and(|press| press.moved) {
            return mouse::Interaction::Grabbing;
        }
        if !cursor.is_over(bounds) {
            return mouse::Interaction::default();
        }
        if interaction.last_hover.is_some() {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::Grab
        }
    }
}

struct Palette {
    background: Color,
    sphere: Color,
    sphere_light: Color,
    sphere_dark: Color,
    land: Color,
    grid: Color,
    outline: Color,
    limb: Color,
    signal: Color,
    accent: Color,
    active_fill: Color,
    active_edge: Color,
}

impl Palette {
    fn new(dark: bool, accent: Color) -> Self {
        let (background, sphere, land, grid, outline, signal) = if dark {
            (
                hex("#090a0c"),
                hex("#11151a"),
                hex("#283039"),
                hex("#7d8791"),
                hex("#9099a3"),
                hex("#d9dee3"),
            )
        } else {
            (
                hex("#e7e6e1"),
                hex("#d2d0ca"),
                hex("#a9aaa6"),
                hex("#3f454a"),
                hex("#3c4247"),
                hex("#202428"),
            )
        };
        let mut active_fill = accent;
        active_fill.a = 0.38;
        let mut active_edge = accent;
        active_edge.a = 0.95;
        let mut grid = grid;
        grid.a = 0.18;
        let mut land = land;
        land.a = 0.9;
        let mut outline = outline;
        outline.a = 0.34;
        let mut limb = outline;
        limb.a = 0.52;
        Palette {
            background,
            sphere_light: lighten(sphere, 1.7),
            sphere,
            sphere_dark: darken(sphere, 1.8),
            land,
            grid,
            outline,
            limb,
            signal,
            accent,
            active_fill,
            active_edge,
        }
    }
}

fn hex(rgb: &str) -> Color {
    let bytes = rgb.trim_start_matches('#');
    let channel = |i: usize| u8::from_str_radix(&bytes[i..i + 2], 16).unwrap_or(0);
    Color::from_rgb8(channel(0), channel(2), channel(4))
}

fn lighten(color: Color, factor: f32) -> Color {
    Color::from_rgb(
        (color.r * factor).min(1.0),
        (color.g * factor).min(1.0),
        (color.b * factor).min(1.0),
    )
}

fn darken(color: Color, factor: f32) -> Color {
    Color::from_rgb(color.r / factor, color.g / factor, color.b / factor)
}
