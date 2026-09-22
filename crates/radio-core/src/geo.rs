//! Globe math and station-list helpers.
//!
//! A line-by-line port of the legacy `RadioModel.js`: orthographic globe
//! projection, kinetic spin physics, country hit-testing, deterministic
//! jitter for stations without coordinates, and station-list combinators.

use std::collections::{HashMap, HashSet};

use crate::countries::{Feature, Polygon};
use crate::model::Station;

pub const RADIANS: f64 = std::f64::consts::PI / 180.0;
pub const DEGREES: f64 = 180.0 / std::f64::consts::PI;
pub const MIN_LATITUDE: f64 = -78.0;
pub const MAX_LATITUDE: f64 = 78.0;
pub const WORLD_STATION_LIMIT: usize = 5000;
pub const GEO_STATION_LIMIT: usize = 5500;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coord {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenPoint {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

pub fn clamp(value: f64, minimum: f64, maximum: f64) -> f64 {
    value.max(minimum).min(maximum)
}

pub fn wrap_longitude(value: f64) -> f64 {
    let mut wrapped = (value + 180.0) % 360.0;
    if wrapped < 0.0 {
        wrapped += 360.0;
    }
    wrapped - 180.0
}

// --- Kinetic rotation -------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct LimitedVelocity {
    pub x: f64,
    pub y: f64,
    pub speed: f64,
}

pub fn limit_kinetic_velocity(x: f64, y: f64, maximum_speed: f64) -> LimitedVelocity {
    if !x.is_finite() || !y.is_finite() || !maximum_speed.is_finite() || maximum_speed <= 0.0 {
        return LimitedVelocity {
            x: 0.0,
            y: 0.0,
            speed: 0.0,
        };
    }
    let speed = x.hypot(y);
    if !speed.is_finite() || speed <= 0.0 {
        return LimitedVelocity {
            x: 0.0,
            y: 0.0,
            speed: 0.0,
        };
    }
    if speed <= maximum_speed {
        return LimitedVelocity { x, y, speed };
    }
    let ratio = maximum_speed / speed;
    LimitedVelocity {
        x: x * ratio,
        y: y * ratio,
        speed: maximum_speed,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LaunchVelocity {
    pub x: f64,
    pub y: f64,
    pub active: bool,
}

pub fn kinetic_launch_velocity(
    x: f64,
    y: f64,
    minimum_speed: f64,
    maximum_speed: f64,
) -> LaunchVelocity {
    let limited = limit_kinetic_velocity(x, y, maximum_speed);
    let threshold = minimum_speed.max(0.0);
    let active = limited.speed >= threshold;
    LaunchVelocity {
        x: if active { limited.x } else { 0.0 },
        y: if active { limited.y } else { 0.0 },
        active,
    }
}

/// Pick the release velocity, preferring a fresh aligned drag sample when the
/// toolkit's filtered velocity has already decayed below it.
pub fn kinetic_release_velocity(
    native_x: f64,
    native_y: f64,
    sampled_x: f64,
    sampled_y: f64,
    sample_age_ms: f64,
    max_sample_age_ms: f64,
) -> (f64, f64) {
    let nx = if native_x.is_finite() { native_x } else { 0.0 };
    let ny = if native_y.is_finite() { native_y } else { 0.0 };
    let sx = if sampled_x.is_finite() {
        sampled_x
    } else {
        0.0
    };
    let sy = if sampled_y.is_finite() {
        sampled_y
    } else {
        0.0
    };
    let native_speed = nx.hypot(ny);
    let sampled_speed = sx.hypot(sy);
    let max_age = max_sample_age_ms.max(0.0);
    let fresh = sample_age_ms.is_finite() && sample_age_ms >= 0.0 && sample_age_ms <= max_age;
    let aligned = native_speed == 0.0 || nx * sx + ny * sy > 0.0;
    if fresh && sampled_speed > native_speed && aligned {
        (sx, sy)
    } else {
        (nx, ny)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct KineticState {
    pub longitude: f64,
    pub latitude: f64,
    pub velocity_x: f64,
    pub velocity_y: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct KineticOptions {
    pub deceleration: f64,
    pub scale: f64,
    pub longitude_sensitivity: f64,
    pub latitude_sensitivity: f64,
    pub minimum_latitude: f64,
    pub maximum_latitude: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct KineticStep {
    pub longitude: f64,
    pub latitude: f64,
    pub velocity_x: f64,
    pub velocity_y: f64,
    pub active: bool,
}

pub fn advance_kinetic_rotation(
    state: KineticState,
    elapsed_seconds: f64,
    options: KineticOptions,
) -> KineticStep {
    let longitude = if state.longitude.is_finite() {
        state.longitude
    } else {
        0.0
    };
    let latitude = if state.latitude.is_finite() {
        state.latitude
    } else {
        0.0
    };
    let velocity_x = if state.velocity_x.is_finite() {
        state.velocity_x
    } else {
        0.0
    };
    let velocity_y = if state.velocity_y.is_finite() {
        state.velocity_y
    } else {
        0.0
    };
    let speed = velocity_x.hypot(velocity_y);
    if !speed.is_finite() || speed <= 0.0 || !elapsed_seconds.is_finite() || elapsed_seconds <= 0.0
    {
        return KineticStep {
            longitude: wrap_longitude(longitude),
            latitude,
            velocity_x,
            velocity_y,
            active: speed > 0.0,
        };
    }

    let deceleration = if options.deceleration.is_finite() && options.deceleration >= 0.0 {
        options.deceleration
    } else {
        0.0
    };
    let active_time = if deceleration > 0.0 {
        elapsed_seconds.min(speed / deceleration)
    } else {
        elapsed_seconds
    };
    let next_speed = (speed - deceleration * active_time).max(0.0);
    let distance = (speed + next_speed) * active_time / 2.0;
    let direction_x = velocity_x / speed;
    let direction_y = velocity_y / speed;

    let scale = if options.scale.is_finite() && options.scale > 0.0 {
        options.scale
    } else {
        1.0
    };
    let lon_sensitivity = if options.longitude_sensitivity.is_finite() {
        options.longitude_sensitivity
    } else {
        0.0
    };
    let lat_sensitivity = if options.latitude_sensitivity.is_finite() {
        options.latitude_sensitivity
    } else {
        0.0
    };
    let min_lat = if options.minimum_latitude.is_finite() {
        options.minimum_latitude
    } else {
        MIN_LATITUDE
    };
    let max_lat = if options.maximum_latitude.is_finite() {
        options.maximum_latitude
    } else {
        MAX_LATITUDE
    };

    let next_longitude =
        wrap_longitude(longitude - direction_x * distance * lon_sensitivity / scale);
    let next_latitude = clamp(
        latitude + direction_y * distance * lat_sensitivity / scale,
        min_lat,
        max_lat,
    );
    let mut next_velocity_y = direction_y * next_speed;
    if (next_latitude >= max_lat && next_velocity_y > 0.0)
        || (next_latitude <= min_lat && next_velocity_y < 0.0)
    {
        next_velocity_y = 0.0;
    }
    let next_velocity_x = direction_x * next_speed;
    let remaining = next_velocity_x.hypot(next_velocity_y);
    KineticStep {
        longitude: next_longitude,
        latitude: next_latitude,
        velocity_x: next_velocity_x,
        velocity_y: next_velocity_y,
        active: remaining > 1e-9,
    }
}

// --- Projection -------------------------------------------------------------

/// Orthographic projection of a coordinate onto the unit globe.
pub fn project(latitude: f64, longitude: f64, centre_latitude: f64, centre_longitude: f64) -> Vec3 {
    let phi = latitude * RADIANS;
    let lambda = wrap_longitude(longitude - centre_longitude) * RADIANS;
    let phi0 = centre_latitude * RADIANS;
    Vec3 {
        x: phi.cos() * lambda.sin(),
        y: phi0.cos() * phi.sin() - phi0.sin() * phi.cos() * lambda.cos(),
        z: phi0.sin() * phi.sin() + phi0.cos() * phi.cos() * lambda.cos(),
    }
}

/// Inverse of [`project`]; `None` when the point falls off the disc.
pub fn unproject(x: f64, y: f64, centre_latitude: f64, centre_longitude: f64) -> Option<Coord> {
    if x * x + y * y > 1.0 {
        return None;
    }
    let z = (0.0f64.max(1.0 - x * x - y * y)).sqrt();
    let phi0 = centre_latitude * RADIANS;
    let latitude = (y * phi0.cos() + z * phi0.sin()).asin();
    let longitude = centre_longitude * RADIANS + x.atan2(z * phi0.cos() - y * phi0.sin());
    Some(Coord {
        latitude: latitude * DEGREES,
        longitude: wrap_longitude(longitude * DEGREES),
    })
}

// --- Country geometry -------------------------------------------------------

pub fn point_in_ring(longitude: f64, latitude: f64, ring: &[[f64; 2]]) -> bool {
    if ring.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = ring.len() - 1;
    for i in 0..ring.len() {
        let xi = ring[i][0];
        let yi = ring[i][1];
        let xj = ring[j][0];
        let yj = ring[j][1];
        // When the edge straddles the latitude, yj - yi cannot be zero.
        if (yi > latitude) != (yj > latitude)
            && longitude < (xj - xi) * (latitude - yi) / (yj - yi) + xi
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn polygons_of(feature: &Feature) -> &[Polygon] {
    feature.geometry.polygons()
}

/// Properties of the country containing a coordinate, if any.
pub fn country_at(features: &[Feature], latitude: f64, longitude: f64) -> Option<&Feature> {
    for feature in features {
        for polygon in polygons_of(feature) {
            let Some(outer) = polygon.first() else {
                continue;
            };
            if !point_in_ring(longitude, latitude, outer) {
                continue;
            }
            let in_hole = polygon
                .iter()
                .skip(1)
                .any(|hole| point_in_ring(longitude, latitude, hole));
            if !in_hole {
                return Some(feature);
            }
        }
    }
    None
}

/// Unwrap a ring across the antimeridian so area math stays sane.
fn unwrapped_points(ring: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let mut points = Vec::with_capacity(ring.len());
    let mut previous = f64::NAN;
    for point in ring {
        let (mut lon, lat) = (point[0], point[1]);
        if !lon.is_finite() || !lat.is_finite() {
            continue;
        }
        if !points.is_empty() {
            while lon - previous > 180.0 {
                lon -= 360.0;
            }
            while lon - previous < -180.0 {
                lon += 360.0;
            }
        }
        points.push([lon, lat]);
        previous = lon;
    }
    points
}

fn ring_area(points: &[[f64; 2]]) -> f64 {
    let mut area = 0.0;
    let mut previous = points.len() - 1;
    for (i, point) in points.iter().enumerate() {
        area += points[previous][0] * point[1] - point[0] * points[previous][1];
        previous = i;
    }
    area.abs()
}

/// Centroid of the largest ring of a country, or `None` when unknown.
pub fn country_centre(features: &[Feature], code: &str) -> Option<Coord> {
    let wanted = code.to_ascii_uppercase();
    for feature in features {
        if feature.properties.code.to_ascii_uppercase() != wanted {
            continue;
        }
        let mut best: Option<(f64, Coord)> = None;
        for polygon in polygons_of(feature) {
            let Some(ring) = polygon.first() else {
                continue;
            };
            let points = unwrapped_points(ring);
            if points.len() < 3 {
                continue;
            }
            let mut cross_sum = 0.0;
            let mut lon_sum = 0.0;
            let mut lat_sum = 0.0;
            let mut prev = points.len() - 1;
            for (i, point) in points.iter().enumerate() {
                let first = points[prev];
                let cross = first[0] * point[1] - point[0] * first[1];
                cross_sum += cross;
                lon_sum += (first[0] + point[0]) * cross;
                lat_sum += (first[1] + point[1]) * cross;
                prev = i;
            }
            let area = cross_sum.abs();
            if area < 1e-9 {
                continue;
            }
            let better = best.map(|(best_area, _)| area > best_area).unwrap_or(true);
            if better {
                best = Some((
                    area,
                    Coord {
                        latitude: lat_sum / (3.0 * cross_sum),
                        longitude: wrap_longitude(lon_sum / (3.0 * cross_sum)),
                    },
                ));
            }
        }
        return best.map(|(_, coord)| coord);
    }
    None
}

/// FNV-1a over UTF-16 code units, matching the legacy JavaScript hash.
fn fnv1a_utf16(text: &str) -> u32 {
    let mut seed: u32 = 2_166_136_261;
    for unit in text.encode_utf16() {
        seed ^= u32::from(unit);
        seed = seed.wrapping_mul(16_777_619);
    }
    seed
}

struct Lcg(u32);

impl Lcg {
    fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(self.0) / 4_294_967_296.0
    }
}

/// Deterministic pseudo-location inside a country for keyed jitter.
///
/// Results are cached in `cache` under `$CODE:key`, mirroring the legacy JS.
pub fn estimated_country_location(
    features: &[Feature],
    code: &str,
    key: &str,
    cache: &mut HashMap<String, Coord>,
) -> Option<Coord> {
    let wanted = code.to_ascii_uppercase();
    let cache_key = format!("${wanted}:{key}");
    if let Some(hit) = cache.get(&cache_key) {
        return Some(*hit);
    }

    let mut best_ring: Option<Vec<[f64; 2]>> = None;
    let mut best_area = 0.0;
    for feature in features {
        if feature.properties.code.to_ascii_uppercase() != wanted {
            continue;
        }
        for polygon in polygons_of(feature) {
            let Some(ring) = polygon.first() else {
                continue;
            };
            let points = unwrapped_points(ring);
            if points.len() < 3 {
                continue;
            }
            let area = ring_area(&points);
            if area > best_area {
                best_area = area;
                best_ring = Some(points);
            }
        }
        break;
    }
    let ring = best_ring?;

    let (mut min_lon, mut max_lon) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut min_lat, mut max_lat) = (f64::INFINITY, f64::NEG_INFINITY);
    for point in &ring {
        min_lon = min_lon.min(point[0]);
        max_lon = max_lon.max(point[0]);
        min_lat = min_lat.min(point[1]);
        max_lat = max_lat.max(point[1]);
    }

    let mut rng = Lcg(fnv1a_utf16(key));
    for _ in 0..96 {
        let lon = min_lon + rng.next_f64() * (max_lon - min_lon);
        let lat = min_lat + rng.next_f64() * (max_lat - min_lat);
        if point_in_ring(lon, lat, &ring) {
            let location = Coord {
                latitude: lat,
                longitude: wrap_longitude(lon),
            };
            cache.insert(cache_key, location);
            return Some(location);
        }
    }
    let centre = country_centre(features, &wanted)?;
    cache.insert(cache_key, centre);
    Some(centre)
}

// --- Station placement ------------------------------------------------------

pub fn station_position(
    station: &Station,
    width: f64,
    height: f64,
    scale: f64,
    centre_latitude: f64,
    centre_longitude: f64,
) -> Option<ScreenPoint> {
    let (lat, lon) = (station.latitude?, station.longitude?);
    let point = project(lat, lon, centre_latitude, centre_longitude);
    if point.z < 0.0 {
        return None;
    }
    let radius = width.min(height) * 0.44 * scale;
    Some(ScreenPoint {
        x: width / 2.0 + point.x * radius,
        y: height / 2.0 - point.y * radius,
        z: point.z,
    })
}

/// The most face-on visible station, preferring any station but `excluded_uuid`.
pub fn nearest_visible_station<'a>(
    stations: &'a [Station],
    centre_latitude: f64,
    centre_longitude: f64,
    excluded_uuid: &str,
    viewport: Option<(f64, f64, f64)>,
) -> Option<&'a Station> {
    let constrained = viewport
        .map(|(w, h, s)| {
            w.is_finite() && w > 0.0 && h.is_finite() && h > 0.0 && s.is_finite() && s > 0.0
        })
        .unwrap_or(false);
    let radius = viewport.map(|(w, h, s)| w.min(h) * 0.44 * s).unwrap_or(0.0);
    let mut nearest: Option<(&Station, f64)> = None;
    let mut preferred: Option<(&Station, f64)> = None;
    for station in stations {
        let (Some(lat), Some(lon)) = (station.latitude, station.longitude) else {
            continue;
        };
        if !lat.is_finite() || !lon.is_finite() {
            continue;
        }
        let point = project(lat, lon, centre_latitude, centre_longitude);
        if !point.z.is_finite() || point.z < 0.0 {
            continue;
        }
        if constrained {
            let (w, h, _) = viewport.unwrap();
            let sx = w / 2.0 + point.x * radius;
            let sy = h / 2.0 - point.y * radius;
            if sx < 0.0 || sx > w || sy < 0.0 || sy > h {
                continue;
            }
        }
        if nearest.map(|(_, depth)| point.z > depth).unwrap_or(true) {
            nearest = Some((station, point.z));
        }
        if station.uuid != excluded_uuid
            && preferred.map(|(_, depth)| point.z > depth).unwrap_or(true)
        {
            preferred = Some((station, point.z));
        }
    }
    preferred.or(nearest).map(|(station, _)| station)
}

/// The station within `hit_radius` pixels of a pointer, nearest wins.
#[allow(clippy::too_many_arguments)]
pub fn station_at(
    stations: &[Station],
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
    centre_latitude: f64,
    centre_longitude: f64,
    hit_radius: f64,
) -> Option<&Station> {
    let mut nearest: Option<(&Station, f64)> = None;
    for station in stations {
        let Some(position) = station_position(
            station,
            width,
            height,
            scale,
            centre_latitude,
            centre_longitude,
        ) else {
            continue;
        };
        let distance = (position.x - x).hypot(position.y - y);
        let within = distance <= nearest.map(|(_, best)| best).unwrap_or(hit_radius);
        if within {
            nearest = Some((station, distance));
        }
    }
    nearest.map(|(station, _)| station)
}

// --- Station lists ------------------------------------------------------------

fn combine_stations(
    groups: &[&[Station]],
    maximum: usize,
    replace_duplicates: bool,
) -> Vec<Station> {
    let limit = maximum.max(1);
    let mut output: Vec<Station> = Vec::new();
    let mut index_of: HashMap<String, usize> = HashMap::new();
    for (g, rows) in groups.iter().enumerate() {
        for row in rows.iter() {
            if row.uuid.is_empty() {
                continue;
            }
            if let Some(&index) = index_of.get(&row.uuid) {
                if replace_duplicates && g > 0 {
                    output[index] = row.clone();
                }
                continue;
            }
            if output.len() >= limit {
                continue;
            }
            index_of.insert(row.uuid.clone(), output.len());
            output.push(row.clone());
        }
    }
    output
}

/// Merge two lists, letting the secondary list refresh duplicates.
pub fn merge_stations(primary: &[Station], secondary: &[Station], maximum: usize) -> Vec<Station> {
    combine_stations(&[primary, secondary], maximum, true)
}

/// Merge two lists, keeping the priority list's rows on duplicates.
pub fn prioritize_stations(
    priority: &[Station],
    fallback: &[Station],
    maximum: usize,
) -> Vec<Station> {
    let limit = maximum.max(1);
    let mut output: Vec<Station> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for row in priority.iter().chain(fallback.iter()) {
        if row.uuid.is_empty() || !seen.insert(row.uuid.as_str()) {
            continue;
        }
        if output.len() >= limit {
            continue;
        }
        output.push(row.clone());
    }
    output
}

/// Merge lists and estimate coordinates for stations missing them.
pub fn merge_geo_stations(
    primary: &[Station],
    secondary: &[Station],
    features: &[Feature],
    cache: &mut HashMap<String, Coord>,
) -> Vec<Station> {
    let rows = merge_stations(primary, secondary, GEO_STATION_LIMIT);
    let mut output = Vec::new();
    for row in rows {
        if row.latitude.is_none() || row.longitude.is_none() {
            let Some(location) =
                estimated_country_location(features, &row.country_code, &row.uuid, cache)
            else {
                continue;
            };
            let mut estimated = row.clone();
            estimated.latitude = Some(location.latitude);
            estimated.longitude = Some(location.longitude);
            estimated.estimated_location = true;
            output.push(estimated);
        } else {
            output.push(row);
        }
        if output.len() >= GEO_STATION_LIMIT {
            break;
        }
    }
    output
}

/// Case-insensitive substring search across the display fields.
pub fn search_stations(stations: &[Station], query: &str, maximum: usize) -> Vec<Station> {
    let wanted = query.trim().to_lowercase();
    if wanted.is_empty() {
        return Vec::new();
    }
    let limit = maximum.max(1);
    let mut output = Vec::new();
    for station in stations {
        if output.len() >= limit {
            break;
        }
        let fields = [
            &station.name,
            &station.country,
            &station.country_code,
            &station.state,
            &station.language,
            &station.tags,
            &station.codec,
        ];
        if fields
            .iter()
            .any(|field| field.to_lowercase().contains(&wanted))
        {
            output.push(station.clone());
        }
    }
    output
}

pub fn stations_for_country(stations: &[Station], code: &str, maximum: usize) -> Vec<Station> {
    let wanted = code.to_ascii_uppercase();
    if wanted.is_empty() {
        return Vec::new();
    }
    let limit = maximum.max(1);
    stations
        .iter()
        .filter(|station| station.country_code.to_ascii_uppercase() == wanted)
        .take(limit)
        .cloned()
        .collect()
}

/// A wrapping window of stations centered on `uuid` for the player queue.
pub fn station_window(stations: &[Station], uuid: &str, maximum: usize) -> Vec<Station> {
    let index = index_by_uuid(stations, uuid);
    let Some(index) = index else {
        return Vec::new();
    };
    let limit = stations.len().min(maximum.max(1));
    let before = (limit - 1) / 2;
    let start = (index + stations.len() - before) % stations.len();
    (0..limit)
        .map(|i| stations[(start + i) % stations.len()].clone())
        .collect()
}

/// First `maximum` unique comma-separated tags joined with middots.
pub fn compact_tags(tags: &str, maximum: usize) -> String {
    let limit = maximum.max(1);
    let mut output = Vec::new();
    for part in tags.split(',') {
        if output.len() >= limit {
            break;
        }
        let part = part.trim();
        if !part.is_empty() && !output.contains(&part) {
            output.push(part);
        }
    }
    output.join(" · ")
}

/// `US · mp3 · 128 kbps`-style station metadata line.
pub fn station_meta(station: &Station) -> String {
    let mut parts = Vec::new();
    if !station.country_code.is_empty() {
        parts.push(station.country_code.clone());
    }
    if !station.codec.is_empty() {
        parts.push(station.codec.clone());
    }
    if station.bitrate > 0 {
        parts.push(format!("{} kbps", station.bitrate));
    }
    parts.join(" · ")
}

pub fn index_by_uuid(stations: &[Station], uuid: &str) -> Option<usize> {
    stations.iter().position(|station| station.uuid == uuid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn station(uuid: &str, lat: Option<f64>, lon: Option<f64>) -> Station {
        Station {
            uuid: uuid.to_string(),
            name: uuid.to_string(),
            url: "https://example.com/x".to_string(),
            latitude: lat,
            longitude: lon,
            ..Station::default()
        }
    }

    #[test]
    fn wraps_longitude_like_js() {
        assert_eq!(wrap_longitude(190.0), -170.0);
        assert_eq!(wrap_longitude(-190.0), 170.0);
        assert_eq!(wrap_longitude(180.0), -180.0);
    }

    #[test]
    fn projects_face_on_point() {
        let point = project(0.0, 0.0, 0.0, 0.0);
        assert!((point.z - 1.0).abs() < 1e-9);
        assert!(point.x.abs() < 1e-9);
    }

    #[test]
    fn unprojects_disc_edge() {
        assert!(unproject(0.0, 0.0, 0.0, 0.0).is_some());
        assert!(unproject(2.0, 0.0, 0.0, 0.0).is_none());
    }

    #[test]
    fn kinetic_coast_decelerates() {
        let step = advance_kinetic_rotation(
            KineticState {
                longitude: 0.0,
                latitude: 0.0,
                velocity_x: 1000.0,
                velocity_y: 0.0,
            },
            1.0,
            KineticOptions {
                deceleration: 1800.0,
                scale: 1.0,
                longitude_sensitivity: 0.22,
                latitude_sensitivity: 0.18,
                minimum_latitude: MIN_LATITUDE,
                maximum_latitude: MAX_LATITUDE,
            },
        );
        assert!(!step.active);
        assert!(step.longitude < 0.0);
    }

    #[test]
    fn merge_replaces_duplicates_from_secondary() {
        let mut updated = station("a", Some(1.0), Some(1.0));
        updated.name = "new".to_string();
        let merged = merge_stations(&[station("a", Some(0.0), Some(0.0))], &[updated], 500);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "new");
    }

    #[test]
    fn prioritize_keeps_priority_rows() {
        let mut updated = station("a", Some(1.0), Some(1.0));
        updated.name = "new".to_string();
        let merged = prioritize_stations(&[station("a", Some(0.0), Some(0.0))], &[updated], 500);
        assert_eq!(merged[0].name, "a");
    }

    #[test]
    fn merge_skips_empty_ids_respects_limit_and_replaces_in_place() {
        let primary = vec![station("a", None, None), station("", None, None)];
        let mut updated = station("a", Some(1.0), Some(1.0));
        updated.name = "new".to_string();
        let merged = merge_stations(&primary, &[updated, station("b", None, None)], 2);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].uuid, "a");
        assert_eq!(merged[0].name, "new");
        assert_eq!(merged[1].uuid, "b");
    }

    #[test]
    fn station_window_wraps() {
        let rows = vec![
            station("a", None, None),
            station("b", None, None),
            station("c", None, None),
        ];
        let window = station_window(&rows, "a", 3);
        assert_eq!(window.len(), 3);
        assert_eq!(window[1].uuid, "a");
    }

    #[test]
    fn search_matches_case_insensitively() {
        let mut jazz = station("a", None, None);
        jazz.tags = "Jazz, Blues".to_string();
        assert_eq!(search_stations(&[jazz], "jazz", 150).len(), 1);
    }
}
