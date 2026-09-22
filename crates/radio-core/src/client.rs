//! Radio Browser directory client with file caches.
//!
//! Ports `radio-fetch`: mirror discovery over `getent`, HTTPS-only requests
//! with bounded responses, single-document validation, persistent world and
//! country caches with stale-while-revalidate refresh, and atomic publishes.

use std::collections::HashSet;
use std::io::Read;
use std::time::{Duration, SystemTime};

use fs2::FileExt;

use crate::error::{Error, Result};
use crate::model::{MAX_STATIONS_PER_RESPONSE, Station, normalize_stations};
use crate::paths::{Paths, atomic_copy, atomic_write};
use crate::state;

pub const MAX_RESPONSE_BYTES: u64 = 4_194_304;
pub const WORLD_CACHE_MINUTES: u64 = 1440;
pub const COUNTRY_CACHE_MINUTES: u64 = 1440;
const FALLBACK_BASE: &str = "https://all.api.radio-browser.info";
const DISCOVERY_HOST: &str = "all.api.radio-browser.info";

fn user_agent() -> String {
    format!(
        "Cosmic Radio Atlas/{} (https://github.com/goshitsarch-eng/cosmic-radio-atlas)",
        env!("CARGO_PKG_VERSION")
    )
}

#[derive(Debug, Clone)]
pub struct Client {
    inner: reqwest::blocking::Client,
    user_agent: String,
}

impl Client {
    pub fn new() -> Result<Self> {
        let inner = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(4))
            .build()?;
        Ok(Client {
            inner,
            user_agent: user_agent(),
        })
    }

    fn timeout_for(max_records: usize) -> Duration {
        Duration::from_secs(if max_records >= MAX_STATIONS_PER_RESPONSE {
            45
        } else {
            12
        })
    }

    /// GET `path` from the first healthy mirror; validates one JSON array
    /// document with at most `max_records` rows.
    pub fn request(
        &self,
        bases: &[String],
        max_records: usize,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<Vec<serde_json::Value>> {
        let mut last_error = String::from("Station service is unavailable");
        for base in bases {
            match self.try_base(base, max_records, path, params) {
                Ok(rows) => return Ok(rows),
                Err(error) => last_error = error.to_string(),
            }
        }
        Err(Error::Unavailable(last_error))
    }

    fn try_base(
        &self,
        base: &str,
        max_records: usize,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<Vec<serde_json::Value>> {
        let mut url = reqwest::Url::parse(base).map_err(|_| invalid_data(base))?;
        url.set_path(path);
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in params {
                query.append_pair(key, value);
            }
        }
        if url.scheme() != "https" {
            return Err(invalid_data(base));
        }
        let mut response = self
            .inner
            .get(url)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .timeout(Self::timeout_for(max_records))
            .send()
            .map_err(|error| Error::Unavailable(trim_error(&error)))?;
        if !response.status().is_success() {
            return Err(invalid_data(base));
        }
        let mut body = Vec::new();
        response
            .by_ref()
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut body)?;
        validate_response_body(&body, max_records).map_err(|_| invalid_data(base))
    }

    // --- High-level fetches -------------------------------------------------

    /// World stations: fresh cache served instantly, stale cache revalidated
    /// in the background, network only when no valid cache exists.
    pub fn fetch_world(&self, paths: &Paths) -> Result<Vec<Station>> {
        let mut cache_valid = valid_world_data(&paths.world_cache_file());
        if !cache_valid && valid_world_data(&paths.world_file()) {
            atomic_copy(&paths.world_file(), &paths.world_cache_file())?;
            cache_valid = true;
        }
        if cache_valid {
            publish_world_cache(paths)?;
            if cache_is_stale(&paths.world_cache_file(), WORLD_CACHE_MINUTES) {
                self.refresh_world_in_background(paths.clone());
            }
            return read_stations(&paths.world_cache_file());
        }
        self.refresh_world_cache(paths)?;
        publish_world_cache(paths)?;
        read_stations(&paths.world_cache_file())
    }

    /// Country stations: fresh cache, else world-cache fallback, else network.
    pub fn fetch_country(&self, paths: &Paths, code: &str) -> Result<Vec<Station>> {
        let code = code.to_ascii_uppercase();
        if code.len() != 2 || !code.bytes().all(|b| b.is_ascii_alphabetic()) {
            return Err(Error::InvalidInput(
                "Country code must contain two letters".to_string(),
            ));
        }
        let cache_file = paths.country_cache_file(&code);
        if valid_country_data(&cache_file, &code) {
            publish_country_cache(paths, &cache_file)?;
            if cache_is_stale(&cache_file, COUNTRY_CACHE_MINUTES) {
                self.refresh_country_in_background(paths.clone(), code.clone());
            }
            return read_stations(&cache_file);
        }
        if let Ok(stations) = publish_world_country(paths, &code) {
            self.refresh_country_in_background(paths.clone(), code.clone());
            return Ok(stations);
        }
        self.refresh_country_cache(paths, &code)?;
        publish_country_cache(paths, &cache_file)?;
        read_stations(&cache_file)
    }

    /// Search by name, country, and tag in parallel; any success wins.
    pub fn fetch_search(&self, paths: &Paths, query: &str) -> Result<Vec<Station>> {
        if query.chars().all(|ch| ch.is_whitespace()) || query.is_empty() {
            return Err(Error::InvalidInput("Search text is empty".to_string()));
        }
        if query.len() > 128 {
            return Err(Error::InvalidInput("Search text is too long".to_string()));
        }
        let bases = load_bases();
        let kinds = ["name", "country", "tag"];
        let results: Vec<Result<Vec<serde_json::Value>>> = std::thread::scope(|scope| {
            kinds
                .iter()
                .map(|kind| {
                    scope.spawn(|| {
                        self.request(
                            &bases,
                            80,
                            "/json/stations/search",
                            &[
                                (*kind, query),
                                ("hidebroken", "true"),
                                ("order", "clickcount"),
                                ("reverse", "true"),
                                ("limit", "80"),
                            ],
                        )
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|_| Err(Error::Unavailable("Search failed".to_string())))
                })
                .collect()
        });
        let mut combined = Vec::new();
        let mut successes = 0;
        for rows in results.iter().flatten() {
            successes += 1;
            combined.extend(rows.clone());
        }
        if successes == 0 {
            return Err(Error::Unavailable(
                "Station service is unavailable".to_string(),
            ));
        }
        let stations = normalize_stations(&serde_json::to_string(&combined)?)?;
        store_results(paths, &paths.results_file(), &stations)?;
        Ok(stations)
    }

    /// Random world expansion page; merges into the in-memory globe only.
    pub fn fetch_world_more(&self, _paths: &Paths) -> Result<Vec<Station>> {
        let bases = load_bases();
        let rows = self.request(
            &bases,
            MAX_STATIONS_PER_RESPONSE,
            "/json/stations/search",
            &[
                ("hidebroken", "true"),
                ("order", "random"),
                ("limit", "500"),
            ],
        )?;
        normalize_stations(&serde_json::to_string(&rows)?).map_err(Error::from)
    }

    /// Random mappable stations, avoiding recent and excluded uuids.
    pub fn fetch_random(&self, paths: &Paths, exclusions: &str) -> Result<Vec<Station>> {
        validate_exclusions(exclusions)?;
        let saved = state::load_state(paths)
            .map(|state| {
                state
                    .recent
                    .iter()
                    .filter_map(|station| {
                        let uuid = station.uuid.trim();
                        if uuid.len() >= 20
                            && uuid.len() <= 64
                            && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
                        {
                            Some(uuid.to_string())
                        } else {
                            None
                        }
                    })
                    .take(30)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut merged: Vec<String> = exclusions
            .split(',')
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .chain(saved)
            .collect();
        merged.sort();
        merged.dedup();
        let exclusions = merged.join(",");
        if exclusions.len() > 4096 {
            return Err(Error::InvalidInput(
                "Too many excluded station ids".to_string(),
            ));
        }

        let bases = load_bases();
        let rows = self.request(
            &bases,
            60,
            "/json/stations/search",
            &[("hidebroken", "true"), ("order", "random"), ("limit", "60")],
        )?;
        let stations = normalize_stations(&serde_json::to_string(&rows)?)?;
        let mappable: Vec<Station> = stations.into_iter().filter(|s| s.is_mappable()).collect();
        let excluded: HashSet<&str> = exclusions.split(',').filter(|s| !s.is_empty()).collect();
        let fresh: Vec<Station> = mappable
            .iter()
            .filter(|s| !excluded.contains(s.uuid.as_str()))
            .cloned()
            .collect();
        let mut picked = if fresh.is_empty() { mappable } else { fresh };
        shuffle(&mut picked);
        store_results(paths, &paths.results_file(), &picked)?;
        Ok(picked)
    }

    /// Re-resolve saved stations by uuid to refresh stale stream URLs.
    pub fn resolve_stations(&self, uuids: &str) -> Result<Vec<Station>> {
        if uuids.len() > 4096 {
            return Err(Error::InvalidInput("Too many station ids".to_string()));
        }
        let parts: Vec<&str> = uuids.split(',').collect();
        if parts.is_empty()
            || parts.iter().any(|part| {
                part.len() < 20 || !part.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
            })
        {
            return Err(Error::InvalidInput("Invalid station ids".to_string()));
        }
        let bases = load_bases();
        let rows = self.request(
            &bases,
            parts.len(),
            "/json/stations/byuuid",
            &[("uuids", uuids)],
        )?;
        normalize_stations(&serde_json::to_string(&rows)?).map_err(Error::from)
    }

    // --- Cache refresh ------------------------------------------------------

    fn refresh_world_cache(&self, paths: &Paths) -> Result<()> {
        let bases = load_bases();
        let rows = self.request(
            &bases,
            MAX_STATIONS_PER_RESPONSE,
            "/json/stations/search",
            &[
                ("has_geo_info", "true"),
                ("hidebroken", "true"),
                ("order", "clickcount"),
                ("reverse", "true"),
                ("limit", "500"),
            ],
        )?;
        let stations = normalize_stations(&serde_json::to_string(&rows)?)?;
        store_results(paths, &paths.world_cache_file(), &stations)?;
        Ok(())
    }

    fn refresh_world_in_background(&self, paths: Paths) {
        let client = self.clone();
        std::thread::spawn(move || {
            let Ok(lock) = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(paths.world_lock_file())
            else {
                return;
            };
            if lock.try_lock_exclusive().is_err() {
                return;
            }
            let _ = client.refresh_world_cache(&paths);
        });
    }

    fn refresh_country_cache(&self, paths: &Paths, code: &str) -> Result<()> {
        let bases = load_bases();
        let rows = self.request(
            &bases,
            25,
            &format!("/json/stations/bycountrycodeexact/{code}"),
            &[
                ("hidebroken", "true"),
                ("order", "clickcount"),
                ("reverse", "true"),
                ("limit", "25"),
            ],
        )?;
        let stations = normalize_stations(&serde_json::to_string(&rows)?)?;
        let target = paths.country_cache_file(code);
        let temporary = target.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let bytes = serde_json::to_vec(&stations).map_err(Error::Json)?;
        atomic_write(&temporary, &bytes)?;
        if !valid_country_data(&temporary, code) {
            let _ = std::fs::remove_file(&temporary);
            return Err(Error::Unavailable(
                "Station service is unavailable".to_string(),
            ));
        }
        std::fs::rename(&temporary, &target)?;
        Ok(())
    }

    fn refresh_country_in_background(&self, paths: Paths, code: String) {
        let client = self.clone();
        std::thread::spawn(move || {
            let Ok(lock) = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(paths.country_lock_file(&code))
            else {
                return;
            };
            if lock.try_lock_exclusive().is_err() {
                return;
            }
            let _ = client.refresh_country_cache(&paths, &code);
        });
    }
}

fn invalid_data(base: &str) -> Error {
    Error::Unavailable(format!(
        "Radio Browser returned invalid station data from {base}"
    ))
}

fn trim_error(error: &reqwest::Error) -> String {
    format!("Radio Browser request failed: {error}")
}

/// Validate a bounded body: exactly one JSON array document, capped length.
pub fn validate_response_body(body: &[u8], max_records: usize) -> Result<Vec<serde_json::Value>> {
    if body.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(Error::Unavailable(
            "Station service is unavailable".to_string(),
        ));
    }
    // `from_slice` rejects trailing bytes after the first document.
    let rows: Vec<serde_json::Value> = serde_json::from_slice(body)
        .map_err(|_| Error::Unavailable("Station service is unavailable".to_string()))?;
    if rows.len() > max_records {
        return Err(Error::Unavailable(
            "Station service is unavailable".to_string(),
        ));
    }
    Ok(rows)
}

fn store_results(_paths: &Paths, target: &std::path::Path, stations: &[Station]) -> Result<()> {
    let bytes = serde_json::to_vec(stations).map_err(Error::Json)?;
    atomic_write(target, &bytes)
}

fn read_stations(path: &std::path::Path) -> Result<Vec<Station>> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(Error::Json)
}

fn cache_is_stale(path: &std::path::Path, fresh_minutes: u64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return true;
    };
    let Ok(modified) = meta.modified() else {
        return true;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age > Duration::from_secs(fresh_minutes * 60))
        .unwrap_or(true)
}

fn valid_world_data(path: &std::path::Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    if bytes.is_empty() || bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return false;
    }
    serde_json::from_slice::<Vec<serde_json::Value>>(&bytes)
        .map(|rows| !rows.is_empty() && rows.len() <= MAX_STATIONS_PER_RESPONSE)
        .unwrap_or(false)
}

fn valid_country_data(path: &std::path::Path, code: &str) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    if bytes.is_empty() || bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return false;
    }
    serde_json::from_slice::<Vec<Station>>(&bytes)
        .map(|rows| rows.len() <= 25 && rows.iter().all(|row| row.country_code == code))
        .unwrap_or(false)
}

fn publish_world_cache(paths: &Paths) -> Result<()> {
    atomic_copy(&paths.world_cache_file(), &paths.world_file())?;
    atomic_copy(&paths.world_cache_file(), &paths.results_file())
}

fn publish_country_cache(paths: &Paths, source: &std::path::Path) -> Result<()> {
    atomic_copy(source, &paths.results_file())
}

fn publish_world_country(paths: &Paths, code: &str) -> Result<Vec<Station>> {
    let source = if valid_world_data(&paths.world_cache_file()) {
        paths.world_cache_file()
    } else if valid_world_data(&paths.world_file()) {
        paths.world_file()
    } else {
        return Err(Error::Unavailable(
            "Station service is unavailable".to_string(),
        ));
    };
    let stations = read_stations(&source)?;
    let filtered: Vec<Station> = stations
        .into_iter()
        .filter(|station| station.country_code == code)
        .take(100)
        .collect();
    if filtered.is_empty() {
        return Err(Error::Unavailable(
            "Station service is unavailable".to_string(),
        ));
    }
    store_results(paths, &paths.results_file(), &filtered)?;
    Ok(filtered)
}

fn validate_exclusions(exclusions: &str) -> Result<()> {
    if exclusions.is_empty() {
        return Ok(());
    }
    if exclusions.len() > 4096 {
        return Err(Error::InvalidInput(
            "Too many excluded station ids".to_string(),
        ));
    }
    let valid = exclusions.split(',').all(|part| {
        (20..=64).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
    });
    if !valid {
        return Err(Error::InvalidInput(
            "Invalid excluded station ids".to_string(),
        ));
    }
    Ok(())
}

// --- Mirror discovery ---------------------------------------------------------

fn shuffle<T>(rows: &mut [T]) {
    let mut seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    if seed == 0 {
        seed = 0x9E37_79B9_7F4A_7C15;
    }
    let mut state = seed;
    let mut next = move || {
        // xorshift64star
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };
    for i in (1..rows.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        rows.swap(i, j);
    }
}

fn run_command(program: &str, args: &[&str]) -> Option<String> {
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
}

fn is_mirror_host(host: &str) -> bool {
    const SUFFIX: &str = ".api.radio-browser.info";
    host.len() > SUFFIX.len()
        && host.ends_with(SUFFIX)
        && host[..host.len() - SUFFIX.len()]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

/// Discover Radio Browser mirrors, shuffled; falls back to the round-robin host.
pub fn load_bases() -> Vec<String> {
    if let Ok(overrides) = std::env::var("COSMIC_RADIO_ATLAS_API_BASES") {
        let bases: Vec<String> = overrides
            .split(',')
            .map(str::trim)
            .filter(|base| base.starts_with("https://"))
            .map(str::to_string)
            .collect();
        if !bases.is_empty() {
            return bases;
        }
    }
    let mut bases = Vec::new();
    let mut seen = HashSet::new();
    if let Some(output) = run_command("getent", &["ahostsv4", DISCOVERY_HOST]) {
        let mut ips = Vec::new();
        for line in output.lines() {
            if let Some(ip) = line.split_whitespace().next()
                && seen.insert(ip.to_string())
            {
                ips.push(ip.to_string());
            }
        }
        for ip in ips {
            let Some(hosts) = run_command("getent", &["hosts", &ip]) else {
                continue;
            };
            let Some(host) = hosts.lines().next().and_then(|line| {
                let mut fields = line.split_whitespace();
                fields.next();
                fields.next()
            }) else {
                continue;
            };
            if is_mirror_host(host) {
                bases.push(format!("https://{host}"));
            }
        }
    }
    bases.sort();
    bases.dedup();
    shuffle(&mut bases);
    if bases.is_empty() {
        bases.push(FALLBACK_BASE.to_string());
    }
    bases
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::test_paths_in;

    #[test]
    fn validates_response_bodies() {
        assert!(validate_response_body(br#"[{"a":1}]"#, 500).is_ok());
        assert!(validate_response_body(b"[] []", 500).is_err());
        assert!(validate_response_body(b"{}", 500).is_err());
        assert!(validate_response_body(br#"[{},{},{}]"#, 2).is_err());
        let big = vec![b'x'; MAX_RESPONSE_BYTES as usize + 1];
        assert!(validate_response_body(&big, 500).is_err());
    }

    #[test]
    fn validates_exclusions() {
        assert!(validate_exclusions("").is_ok());
        assert!(validate_exclusions("aaaaaaaaaaaaaaaaaaaa").is_ok());
        assert!(validate_exclusions("short").is_err());
        assert!(validate_exclusions("aaaaaaaaaaaaaaaaaaaa,bad!").is_err());
    }

    #[test]
    fn checks_mirror_hosts() {
        assert!(is_mirror_host("de1.api.radio-browser.info"));
        // The round-robin host matches the legacy pattern too; accepting it
        // is harmless because it is also the fallback base.
        assert!(is_mirror_host("all.api.radio-browser.info"));
        assert!(!is_mirror_host("evil.com"));
        assert!(!is_mirror_host("x.api.radio-browser.info.evil.com"));
    }

    #[test]
    fn serves_fresh_country_cache_without_network() {
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths_in(root.path());
        let cached = r#"[{"uuid":"bbbbbbbb-1111-1111-1111-bbbbbbbbbbbb","name":"Cached","url":"https://example.com/c","countryCode":"US"}]"#;
        std::fs::write(paths.country_cache_file("US"), cached).unwrap();

        let client = Client::new().unwrap();
        let stations = client.fetch_country(&paths, "us").unwrap();
        assert_eq!(stations.len(), 1);
        assert_eq!(stations[0].name, "Cached");
        // Published to the live results file.
        let results = std::fs::read_to_string(paths.results_file()).unwrap();
        assert!(results.contains("Cached"));
    }
}
