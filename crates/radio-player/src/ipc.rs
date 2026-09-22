//! mpv JSON IPC over its unix socket.
//!
//! Ports the `send_command`/`status` helpers: every command carries a fixed
//! request id, responses are size-bounded, and only an explicit
//! `"error": "success"` reply counts — a dead or replaced socket must fail
//! loudly instead of reporting phantom success.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use radio_core::error::{Error, Result};

pub const MAX_IPC_BYTES: usize = 65536;
const COMMAND_REQUEST_ID: i64 = 9001;
const IO_TIMEOUT: Duration = Duration::from_secs(1);

fn connect(socket: &std::path::Path) -> Result<UnixStream> {
    if !is_socket(socket) {
        return Err(Error::Unavailable(
            "Radio player is unavailable".to_string(),
        ));
    }
    let stream = UnixStream::connect(socket)
        .map_err(|_| Error::Unavailable("Radio player is unavailable".to_string()))?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

fn is_socket(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_socket())
        .unwrap_or(false)
}

/// Read newline-delimited JSON messages until `keep_going` is satisfied,
/// the socket closes or times out, or the byte cap is hit.
fn read_while(
    stream: &UnixStream,
    mut keep_going: impl FnMut(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    let mut messages = Vec::new();
    let mut total = 0usize;
    // Clone shares the timeouts; BufReader scopes the borrow.
    let Ok(clone) = stream.try_clone() else {
        return messages;
    };
    let mut reader = BufReader::new(clone);
    let mut line = Vec::new();
    while keep_going(&messages) {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {
                total += line.len();
                if total > MAX_IPC_BYTES {
                    break;
                }
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line) {
                    messages.push(value);
                }
                if line.last() != Some(&b'\n') {
                    break;
                }
            }
            Err(_) => break, // timeout or disconnect: work with what arrived
        }
    }
    messages
}

fn has_request_id(messages: &[serde_json::Value], id: i64) -> bool {
    messages
        .iter()
        .any(|message| message.get("request_id").and_then(|r| r.as_i64()) == Some(id))
}

/// Send one command object, succeeding only on an explicit success reply.
pub fn send_command(socket: &std::path::Path, mut payload: serde_json::Value) -> Result<()> {
    let mut stream = connect(socket)?;
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "request_id".to_string(),
            serde_json::Value::from(COMMAND_REQUEST_ID),
        );
    }
    let mut bytes = serde_json::to_vec(&payload).map_err(Error::Json)?;
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .map_err(|_| Error::Unavailable("Radio player is unavailable".to_string()))?;
    let messages = read_while(&stream, |messages| {
        !has_request_id(messages, COMMAND_REQUEST_ID)
    });
    let success = messages.iter().any(|message| {
        message.get("request_id").and_then(|id| id.as_i64()) == Some(COMMAND_REQUEST_ID)
            && message.get("error").and_then(|e| e.as_str()) == Some("success")
    });
    if success {
        Ok(())
    } else {
        Err(Error::Unavailable(
            "Radio player is unavailable".to_string(),
        ))
    }
}

/// Property ids for the batched status query, mirroring the shell script.
pub const PROP_PAUSE: i64 = 1;
pub const PROP_TITLE: i64 = 2;
pub const PROP_POSITION: i64 = 3;
pub const PROP_COUNT: i64 = 4;
pub const PROP_VOLUME: i64 = 5;
pub const PROP_MUTE: i64 = 6;
pub const PROP_DEVICE: i64 = 7;
pub const PROP_FAILURE: i64 = 8;

const STATUS_BATCH: &str = concat!(
    "{\"command\":[\"get_property\",\"pause\"],\"request_id\":1}\n",
    "{\"command\":[\"get_property\",\"media-title\"],\"request_id\":2}\n",
    "{\"command\":[\"get_property\",\"playlist-pos\"],\"request_id\":3}\n",
    "{\"command\":[\"get_property\",\"playlist-count\"],\"request_id\":4}\n",
    "{\"command\":[\"get_property\",\"volume\"],\"request_id\":5}\n",
    "{\"command\":[\"get_property\",\"mute\"],\"request_id\":6}\n",
    "{\"command\":[\"get_property\",\"audio-device\"],\"request_id\":7}\n",
    "{\"command\":[\"get_property\",\"user-data/radio-atlas-failure\"],\"request_id\":8}\n",
);

/// Query the eight status properties in one round trip.
pub fn query_status_properties(socket: &std::path::Path) -> Result<Vec<serde_json::Value>> {
    let mut stream = connect(socket)?;
    stream
        .write_all(STATUS_BATCH.as_bytes())
        .map_err(|_| Error::Unavailable("Radio player is unavailable".to_string()))?;
    let messages = read_while(&stream, |messages| {
        !(PROP_PAUSE..=PROP_FAILURE).all(|id| has_request_id(messages, id))
    });
    if !has_request_id(&messages, PROP_PAUSE) {
        return Err(Error::Unavailable(
            "Radio player is unavailable".to_string(),
        ));
    }
    Ok(messages)
}

/// Extract the `data` payload for one property id.
pub fn property(messages: &[serde_json::Value], id: i64) -> Option<&serde_json::Value> {
    messages
        .iter()
        .find(|message| message.get("request_id").and_then(|r| r.as_i64()) == Some(id))
        .and_then(|message| message.get("data"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn fake_mpv(responses: Vec<String>) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mpv.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let mut lines = 0;
            // Read the command batch, then answer with canned responses.
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        lines += 1;
                        if line.contains("9001") || lines >= 8 || line.trim().is_empty() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let mut stream = stream;
            for response in responses {
                let _ = writeln!(stream, "{response}");
            }
        });
        (dir, socket)
    }

    #[test]
    fn send_command_requires_success_reply() {
        let (_dir, socket) = fake_mpv(vec![r#"{"request_id":9001,"error":"success"}"#.to_string()]);
        std::thread::sleep(Duration::from_millis(50));
        assert!(send_command(&socket, serde_json::json!({"command":["cycle","mute"]})).is_ok());

        let (_dir, socket) = fake_mpv(vec![
            r#"{"request_id":9001,"error":"invalid parameter"}"#.to_string(),
        ]);
        std::thread::sleep(Duration::from_millis(50));
        assert!(send_command(&socket, serde_json::json!({"command":["cycle","mute"]})).is_err());
    }

    #[test]
    fn status_query_needs_pause_reply() {
        let (_dir, socket) = fake_mpv(vec![
            r#"{"request_id":2,"error":"success","data":"x"}"#.to_string(),
        ]);
        std::thread::sleep(Duration::from_millis(50));
        assert!(query_status_properties(&socket).is_err());

        let (_dir, socket) = fake_mpv(vec![
            r#"{"request_id":1,"error":"success","data":false}"#.to_string(),
            r#"{"request_id":5,"error":"success","data":70}"#.to_string(),
        ]);
        std::thread::sleep(Duration::from_millis(50));
        let messages = query_status_properties(&socket).unwrap();
        assert_eq!(property(&messages, 5).and_then(|v| v.as_i64()), Some(70));
    }
}
