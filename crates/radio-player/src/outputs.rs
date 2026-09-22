//! PipeWire audio-output listing via `pactl`.
//!
//! Ports `list_outputs`: every sink becomes an `{id, label}` choice, with
//! AirPlay (RAOP) sinks annotated by their speaker address so identical
//! speaker names stay distinguishable.

use radio_core::error::{Error, Result};
use radio_core::state::is_valid_output_name;

const MAX_PACTL_BYTES: usize = 4_194_304;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AudioOutput {
    pub id: String,
    pub label: String,
}

/// List available output sinks, most capable first (server order kept).
pub fn list_outputs() -> Result<Vec<AudioOutput>> {
    let output = std::process::Command::new("pactl")
        .args(["-f", "json", "list", "sinks"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|_| Error::Unavailable("Audio outputs are unavailable".to_string()))?;
    if !output.status.success() {
        return Err(Error::Unavailable(
            "Audio outputs are unavailable".to_string(),
        ));
    }
    parse_outputs(&output.stdout)
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

fn raop_label(id: &str, description: &str) -> String {
    let parts: Vec<&str> = id.split('.').collect();
    let address = if parts.len() >= 5 {
        parts[parts.len() - 5..parts.len() - 1].join(".")
    } else {
        String::new()
    };
    if address.is_empty() {
        single_line(description, 160)
    } else {
        single_line(&format!("{description} · {address}"), 160)
    }
}

fn parse_outputs(bytes: &[u8]) -> Result<Vec<AudioOutput>> {
    if bytes.len() > MAX_PACTL_BYTES {
        return Err(Error::Unavailable(
            "Audio outputs are unavailable".to_string(),
        ));
    }
    let sinks: Vec<serde_json::Value> = serde_json::from_slice(bytes)
        .map_err(|_| Error::Unavailable("Audio outputs are unavailable".to_string()))?;
    let mut outputs = Vec::new();
    for sink in &sinks {
        let Some(id) = sink.get("name").and_then(|name| name.as_str()) else {
            continue;
        };
        if id.is_empty() || !is_valid_output_name(id) {
            continue;
        }
        let description = sink
            .get("description")
            .and_then(|value| value.as_str())
            .unwrap_or(id);
        let label = if id.starts_with("raop_sink.") {
            raop_label(id, description)
        } else {
            single_line(description, 160)
        };
        outputs.push(AudioOutput {
            id: id.to_string(),
            label,
        });
    }
    Ok(outputs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sink_list() {
        let json = br#"[
            {"name":"alsa_output.pci.stereo","description":"Built-in Audio"},
            {"name":"raop_sink.Living-Room.local.192.168.1.49.7000","description":"Living Room"},
            {"name":"bad sink!","description":"Rejected"},
            {"description":"Missing name"}
        ]"#;
        let outputs = parse_outputs(json).unwrap();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].label, "Built-in Audio");
        assert_eq!(outputs[1].label, "Living Room · 192.168.1.49");
    }
}
