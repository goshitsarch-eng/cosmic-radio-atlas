//! Station records and Radio Browser normalization.
//!
//! Mirrors the `normalize` filter in the legacy `radio-fetch` script: every
//! remote string is stripped of control characters and truncated, coordinates
//! are range-checked, and rows without a usable id or stream URL are dropped.

use serde::{Deserialize, Serialize};

/// Maximum stations accepted from a single directory response.
pub const MAX_STATIONS_PER_RESPONSE: usize = 500;

/// A normalized radio station.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Station {
    pub uuid: String,
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub favicon: String,
    #[serde(default)]
    pub country: String,
    #[serde(rename = "countryCode", default)]
    pub country_code: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub tags: String,
    #[serde(default)]
    pub codec: String,
    #[serde(default)]
    pub bitrate: i64,
    #[serde(default)]
    pub votes: i64,
    #[serde(default)]
    pub clicks: i64,
    #[serde(default)]
    pub latitude: Option<f64>,
    #[serde(default)]
    pub longitude: Option<f64>,
    /// True when the coordinates were estimated from the country outline
    /// because the directory published none.
    #[serde(default)]
    pub estimated_location: bool,
}

impl Station {
    /// Whether this station can be placed on the globe or in a country list.
    pub fn is_mappable(&self) -> bool {
        (self.latitude.is_some() && self.longitude.is_some()) || is_country_code(&self.country_code)
    }

    /// Whether the stream URL is safe to hand to the player.
    pub fn has_playable_url(&self) -> bool {
        is_http_url(&self.url)
    }
}

/// Raw directory row; every field is optional because community data is messy.
#[derive(Debug, Clone, Default, Deserialize)]
struct RawStation {
    #[serde(default)]
    stationuuid: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    url_resolved: Option<String>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    favicon: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    countrycode: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    tags: Option<String>,
    #[serde(default)]
    codec: Option<String>,
    #[serde(default)]
    bitrate: Option<FlexNumber>,
    #[serde(default)]
    votes: Option<FlexNumber>,
    #[serde(default)]
    clickcount: Option<FlexNumber>,
    #[serde(default)]
    geo_lat: Option<FlexNumber>,
    #[serde(default)]
    geo_long: Option<FlexNumber>,
}

/// A number that also tolerates numeric strings, like jq's `tonumber`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum FlexNumber {
    Number(f64),
    Text(String),
}

impl FlexNumber {
    fn as_f64(&self) -> Option<f64> {
        match self {
            FlexNumber::Number(value) if value.is_finite() => Some(*value),
            FlexNumber::Text(text) => text.trim().parse::<f64>().ok().filter(|v| v.is_finite()),
            _ => None,
        }
    }
}

/// Collapse `[\r\n\t]` runs to a single space and truncate to `limit` chars.
fn clean(value: Option<&str>, limit: usize) -> String {
    let raw = value.unwrap_or("");
    let mut out = String::with_capacity(raw.len().min(limit));
    let mut chars = raw.chars().peekable();
    let mut pending_space = false;
    let mut count = 0;
    while let Some(ch) = chars.next() {
        if count >= limit {
            break;
        }
        if ch == '\r' || ch == '\n' || ch == '\t' {
            pending_space = true;
            continue;
        }
        // Collapse runs of plain spaces the way `gsub("  +"; " ")` does.
        if ch == ' ' {
            if chars.peek() == Some(&' ') {
                continue;
            }
            if pending_space {
                pending_space = false;
            }
            out.push(' ');
            count += 1;
            continue;
        }
        if pending_space {
            if count + 1 >= limit && out.is_empty() {
                break;
            }
            if !out.is_empty() {
                out.push(' ');
                count += 1;
                if count >= limit {
                    break;
                }
            }
            pending_space = false;
        }
        out.push(ch);
        count += 1;
    }
    out
}

/// Strip `[\r\n\t]` and truncate to `limit` chars.
fn compact(value: Option<&str>, limit: usize) -> String {
    value
        .unwrap_or("")
        .chars()
        .filter(|ch| *ch != '\r' && *ch != '\n' && *ch != '\t')
        .take(limit)
        .collect()
}

fn is_http_url(url: &str) -> bool {
    url.len() >= 7
        && (url[..7].eq_ignore_ascii_case("http://")
            || (url.len() >= 8 && url[..8].eq_ignore_ascii_case("https://")))
}

fn is_country_code(code: &str) -> bool {
    code.len() == 2 && code.bytes().all(|b| b.is_ascii_uppercase())
}

fn normalize_row(raw: &RawStation) -> Option<Station> {
    let uuid = compact(raw.stationuuid.as_deref(), 64);
    let name = clean(raw.name.as_deref(), 160);
    let name = if name.trim().is_empty() {
        "Unknown station".to_string()
    } else {
        name
    };
    let url = compact(
        raw.url_resolved
            .as_deref()
            .filter(|u| !u.is_empty())
            .or(raw.url.as_deref()),
        2048,
    );
    let latitude = raw.geo_lat.as_ref().and_then(FlexNumber::as_f64);
    let longitude = raw.geo_long.as_ref().and_then(FlexNumber::as_f64);

    if uuid.is_empty() || !is_http_url(&url) {
        return None;
    }
    if let Some(lat) = latitude
        && !(-90.0..=90.0).contains(&lat)
    {
        return None;
    }
    if let Some(lon) = longitude
        && !(-180.0..=180.0).contains(&lon)
    {
        return None;
    }

    Some(Station {
        uuid,
        name,
        url,
        homepage: compact(raw.homepage.as_deref(), 2048),
        favicon: compact(raw.favicon.as_deref(), 2048),
        country: clean(raw.country.as_deref(), 100),
        country_code: clean(raw.countrycode.as_deref(), 2).to_ascii_uppercase(),
        state: clean(raw.state.as_deref(), 100),
        language: clean(raw.language.as_deref(), 120),
        tags: clean(raw.tags.as_deref(), 500),
        codec: clean(raw.codec.as_deref(), 32),
        bitrate: raw
            .bitrate
            .as_ref()
            .and_then(FlexNumber::as_f64)
            .map(|v| v.round() as i64)
            .unwrap_or(0),
        votes: raw
            .votes
            .as_ref()
            .and_then(FlexNumber::as_f64)
            .map(|v| v.round() as i64)
            .unwrap_or(0),
        clicks: raw
            .clickcount
            .as_ref()
            .and_then(FlexNumber::as_f64)
            .map(|v| v.round() as i64)
            .unwrap_or(0),
        latitude,
        longitude,
        estimated_location: false,
    })
}

/// Normalize raw directory JSON into validated stations.
///
/// Accepts a JSON array, deduplicates by uuid (first wins), and caps the
/// result at [`MAX_STATIONS_PER_RESPONSE`].
pub fn normalize_stations(json: &str) -> Result<Vec<Station>, serde_json::Error> {
    let raw: Vec<RawStation> = serde_json::from_str(json)?;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in raw.iter().take(MAX_STATIONS_PER_RESPONSE) {
        let Some(station) = normalize_row(row) else {
            continue;
        };
        if !seen.insert(station.uuid.clone()) {
            continue;
        }
        out.push(station);
        if out.len() >= MAX_STATIONS_PER_RESPONSE {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_filters_rows() {
        let json = r#"[
            {"stationuuid":"a","name":"  Good  ","url":"https://example.com/a","countrycode":"us","geo_lat":40,"geo_long":-74,"bitrate":128},
            {"stationuuid":"","name":"No id","url":"https://example.com/b"},
            {"stationuuid":"c","name":"Bad url","url":"ftp://example.com/c"},
            {"stationuuid":"d","name":"Bad lat","url":"https://example.com/d","geo_lat":999,"geo_long":0},
            {"stationuuid":"a","name":"Dupe","url":"https://example.com/dupe"}
        ]"#;
        let stations = normalize_stations(json).unwrap();
        assert_eq!(stations.len(), 1);
        assert_eq!(stations[0].uuid, "a");
        assert_eq!(stations[0].name, " Good ");
        assert_eq!(stations[0].country_code, "US");
        assert_eq!(stations[0].bitrate, 128);
    }

    #[test]
    fn cleans_control_characters() {
        let json = r#"[{"stationuuid":"a","name":"A\r\n\tB  C","url":"https://example.com/a"}]"#;
        let stations = normalize_stations(json).unwrap();
        assert_eq!(stations[0].name, "A B C");
    }

    #[test]
    fn defaults_unknown_name() {
        let json = r#"[{"stationuuid":"a","url":"https://example.com/a"}]"#;
        let stations = normalize_stations(json).unwrap();
        assert_eq!(stations[0].name, "Unknown station");
    }
}
