//! Player process tracking and sandboxed spawning.
//!
//! Ports the `radio-player` PID protocol: the mpv session leader's pid plus
//! its `/proc` start time are recorded so a recycled pid is never mistaken
//! for the player, and shutdown signals the whole process group.

use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::Duration;

use radio_core::error::{Error, Result};
use radio_core::paths::Paths;

/// Start time (field 22) and state (field 3) from `/proc/<pid>/stat`.
fn proc_stat(pid: u32) -> Option<(String, String)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let end = text.rfind(')')?;
    let after = text.get(end + 1..)?;
    let mut fields = after.split_whitespace();
    let state = fields.next()?.to_string();
    // Fields 4..=21 precede the start time.
    let start = fields.nth(18)?.to_string();
    if state == "Z" || !start.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((state, start))
}

fn process_start_time(pid: u32) -> Option<String> {
    proc_stat(pid).map(|(_, start)| start)
}

fn signal_process_group(pid: u32, signal: libc::c_int) -> bool {
    // SAFETY: kill with a signal number is async-signal-safe; negative pid
    // targets the process group, matching `kill -- -pid`.
    unsafe { libc::kill(-(pid as libc::pid_t), signal) == 0 }
}

fn signal_alive(pid: u32) -> bool {
    // SAFETY: signal 0 performs error checking only.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Whether the tracked player process is still the recorded one.
pub fn player_alive(paths: &Paths) -> bool {
    let Ok(text) = std::fs::read_to_string(paths.player_pid_file()) else {
        return false;
    };
    let mut fields = text.split_whitespace();
    let (Some(pid_text), Some(expected)) = (fields.next(), fields.next()) else {
        return false;
    };
    let Ok(pid) = pid_text.parse::<u32>() else {
        return false;
    };
    if !expected.bytes().all(|b| b.is_ascii_digit()) || !signal_alive(pid) {
        return false;
    }
    process_start_time(pid).as_deref() == Some(expected)
}

pub fn tracked_pid(paths: &Paths) -> Option<u32> {
    std::fs::read_to_string(paths.player_pid_file())
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

pub fn write_player_pid(paths: &Paths, pid: u32) -> Result<()> {
    let start = process_start_time(pid)
        .ok_or_else(|| Error::Unavailable("Radio player could not start".to_string()))?;
    radio_core::paths::atomic_write(
        paths.player_pid_file().as_path(),
        format!("{pid} {start}\n").as_bytes(),
    )?;
    Ok(())
}

/// Remove pid/socket files when the tracked player is gone.
pub fn cleanup_dead_player(paths: &Paths) {
    if player_alive(paths) {
        return;
    }
    for path in [
        paths.player_pid_file(),
        paths.proxy_socket(),
        paths.mpv_socket(),
    ] {
        let _ = std::fs::remove_file(path);
    }
}

/// SIGTERM the player group, escalating to SIGKILL; true when fully stopped.
pub fn terminate_tracked_player(paths: &Paths) -> bool {
    if !player_alive(paths) {
        cleanup_dead_player(paths);
        return true;
    }
    let Some(pid) = tracked_pid(paths) else {
        return false;
    };
    signal_process_group(pid, libc::SIGTERM);
    for _ in 0..100 {
        if !player_alive(paths) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if player_alive(paths) {
        signal_process_group(pid, libc::SIGKILL);
        for _ in 0..50 {
            if !player_alive(paths) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    if player_alive(paths) {
        return false;
    }
    cleanup_dead_player(paths);
    true
}

/// Monotonic generation bumped by `stop` to cancel in-flight `play` calls.
pub fn player_generation(paths: &Paths) -> u64 {
    std::fs::read_to_string(paths.generation_file())
        .ok()
        .and_then(|text| text.split_whitespace().next().map(str::to_string))
        .and_then(|text| text.parse().ok())
        .unwrap_or(0)
}

pub fn cancel_pending_plays(paths: &Paths) -> Result<()> {
    let next = player_generation(paths).wrapping_add(1);
    radio_core::paths::atomic_write(
        paths.generation_file().as_path(),
        format!("{next}\n").as_bytes(),
    )?;
    Ok(())
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// Directory holding the sandbox helpers (`radio-session`, …), if installed.
pub fn helpers_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("COSMIC_RADIO_ATLAS_LIBEXEC") {
        let dir = std::path::PathBuf::from(dir);
        if dir.join("radio-session").is_file() {
            return Some(dir);
        }
    }
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent().and_then(|dir| dir.parent()) {
            candidates.push(dir.join("lib").join("cosmic-radio-atlas"));
        }
        if let Some(dir) = exe.parent() {
            candidates.push(dir.to_path_buf());
        }
    }
    candidates.push("/usr/lib/cosmic-radio-atlas".into());
    candidates.push("/usr/local/lib/cosmic-radio-atlas".into());
    candidates.into_iter().find(|dir| {
        dir.join("radio-session").is_file()
            && dir.join("radio-sandbox").is_file()
            && dir.join("radio-proxy").is_file()
            && dir.join("radio-status.lua").is_file()
    })
}

/// Whether the network-namespace sandbox can be used on this machine.
pub fn sandbox_available() -> bool {
    helpers_dir().is_some() && on_path("bwrap") && on_path("socat") && on_path("python3")
}

/// Path of the mpv status script, preferring the installed helper copy.
pub fn status_script_path() -> Option<std::path::PathBuf> {
    helpers_dir().map(|dir| dir.join("radio-status.lua"))
}

pub fn mpv_program() -> String {
    std::env::var("COSMIC_RADIO_ATLAS_MPV").unwrap_or_else(|_| "mpv".to_string())
}

/// Spawn mpv detached as a session leader, logging to the runtime log file.
///
/// Uses the `radio-session` sandbox when available, otherwise runs mpv
/// directly. Returns the session leader pid on success.
pub fn spawn_player(paths: &Paths, program: &str, mpv_args: &[String]) -> Result<u32> {
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(paths.mpv_log_file())?;
    // Truncate any stale proxy log; a new session replaces both logs.
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(paths.runtime_dir.join("proxy.log"));

    let mut command = if sandbox_available() {
        let helpers = helpers_dir().expect("sandbox checked above");
        let mut command = std::process::Command::new(helpers.join("radio-session"));
        command.arg(program);
        command.args(mpv_args);
        command.env("RADIO_ATLAS_STATUS_FILE", paths.status_file());
        command.env("RADIO_ATLAS_QUEUE_FILE", paths.queue_file());
        command
    } else {
        let mut command = std::process::Command::new(program);
        command.args(mpv_args);
        command.env("RADIO_ATLAS_STATUS_FILE", paths.status_file());
        command.env("RADIO_ATLAS_QUEUE_FILE", paths.queue_file());
        command
    };
    // SAFETY: pre_exec runs after fork; setsid takes no locks or heap.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .current_dir(paths.runtime_dir.clone())
        .spawn()
        .map_err(|_| Error::Unavailable("Radio player could not start".to_string()))?;
    let pid = child.id();
    // The session leader outlives this call; a reaper thread collects it on
    // exit so it never lingers as a zombie inside the long-lived UI process.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    write_player_pid(paths, pid)?;
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use radio_core::paths::Paths;

    #[test]
    fn detects_dead_player() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::resolve_in(
            root.path(),
            &root.path().join("cache"),
            &root.path().join("data"),
        )
        .unwrap();
        assert!(!player_alive(&paths));
        std::fs::write(paths.player_pid_file(), "99999999 1\n").unwrap();
        assert!(!player_alive(&paths));
        cleanup_dead_player(&paths);
        assert!(!paths.player_pid_file().exists());
    }

    #[test]
    fn bumps_generation() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::resolve_in(
            root.path(),
            &root.path().join("cache"),
            &root.path().join("data"),
        )
        .unwrap();
        assert_eq!(player_generation(&paths), 0);
        cancel_pending_plays(&paths).unwrap();
        assert_eq!(player_generation(&paths), 1);
    }
}
