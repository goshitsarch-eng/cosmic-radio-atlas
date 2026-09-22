# Cosmic Radio Atlas

Explore live radio on a rotatable globe, now as a native COSMIC app and panel
applet. Click a station signal to play it, or click a country to browse its
stations. Playback runs in `mpv`, so the usual media keys and
`playerctl`-style controls keep working (with `mpv-mpris` installed).

This is a Rust/[libcosmic](https://github.com/pop-os/libcosmic) rewrite of
[omarchy-radio-atlas](https://github.com/AksharP5/omarchy-radio-atlas). The
original Omarchy/QML implementation is preserved under
[`legacy/`](legacy/) for reference.

## Features

- Kinetic drag rotation with landing highlight, plus deep wheel zoom on a
  theme-aware globe rendered with an iced canvas
- Fast cached world view that progressively adds thousands of stations
- Country browsing with map estimates when a station has no coordinates
- Automatic country focus for the station that is actually playing
- Current station identity, track metadata, and one-click favoriting
- Instant cached results while search and country browsing refresh
- Random tuning that avoids recent stations, plus favorites and history
- Independent volume slider, mute, and an audio-output picker (PipeWire
  sinks, including AirPlay speakers exposed as RAOP sinks)
- Panel applet with transport controls, volume, random tuning, and favorites
- Persistent world and country caches with background refresh
- Sandboxed playback: mpv runs in a network namespace and reaches stations
  through a filtering proxy (when `bwrap`, `socat`, and `python3` are
  installed); otherwise it runs directly

## Install

Dependencies: `mpv`, `pactl` (PipeWire), `curl`, plus the COSMIC desktop.
Optional: `bwrap`, `socat`, `python3` for sandboxed playback, and
`mpv-mpris` for desktop media integration.

```bash
just build-release
sudo just install
```

Then launch **Radio Atlas** from the app library, and add the **Radio Atlas
Applet** to the panel from COSMIC Settings → Desktop → Panel.

Favorites, history, volume, and output live in
`~/.local/share/radio-atlas/state.json`, shared with the legacy release, so
existing data carries over.

## Layout

| Path                    | Contents                                                     |
| ----------------------- | ------------------------------------------------------------ |
| `app/`                  | COSMIC application (globe, lists, search, player)            |
| `applet/`               | COSMIC panel applet with popup controls                      |
| `crates/radio-core/`    | Station model, globe math, Radio Browser client, saved state |
| `crates/radio-player/`  | mpv sessions, JSON IPC, PipeWire outputs, sandbox helpers    |
| `crates/radio-tasks/`   | Async background tasks shared by the app and applet          |
| `legacy/`               | Original Omarchy/QML implementation and its tests            |

## Development

```bash
cargo test -p radio-core -p radio-player -p radio-tasks
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo run -p cosmic-radio-atlas
```

`just test`, `just check`, `just fmt`, and `just run` wrap the same commands.

## Data and privacy

Station data comes from the community-run
[Radio Browser](https://www.radio-browser.info/). The client sends its name
and version as the HTTP user agent, and starting a station calls Radio
Browser's click-count endpoint. Favorites and history stay local.

Station metadata and stream URLs are community supplied. Sandboxed playback
rejects private and effectively local destinations, including after
redirects. Radio Atlas still connects to third-party stations, and HTTP
streams are unencrypted — only play stations you trust.

Map geometry comes from public-domain Natural Earth data (see
`crates/radio-core/assets/NOTICE.md`).
