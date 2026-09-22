# Cosmic Radio Atlas

Explore live radio on a rotatable globe. Click a station signal to play it,
or click a country to browse its stations — as a native COSMIC app and panel
applet, written in Rust with [libcosmic](https://github.com/pop-os/libcosmic).

Playback runs in `mpv`, so media keys and desktop player controls keep
working when [`mpv-mpris`](https://github.com/hoyon/mpv-mpris) is installed.
This project is a rewrite of
[omarchy-radio-atlas](https://github.com/AksharP5/omarchy-radio-atlas); the
original Omarchy/QML implementation is preserved under
[`legacy/`](legacy/) for reference.

## Features

- Kinetic globe: drag or flick to spin with a landing highlight, wheel to
  zoom from full-disc down to street-level detail, theme-aware colors
- Fast cached world view that progressively loads thousands of stations
- Country browsing with map estimates for stations lacking coordinates
- Automatic globe focus on the station that is actually playing
- Live search across names, countries, languages, tags, and codecs
- Random tuning that avoids recently played stations
- Favorites and listening history, saved locally
- Independent volume, mute, and a PipeWire output picker (including AirPlay
  speakers exposed as RAOP sinks)
- Panel applet with transport controls, volume, random tuning, and favorites
- Stream-failure handling that keeps your station selected with explicit
  retry, instead of silently switching away
- Sandboxed playback: mpv runs in a network namespace behind a filtering
  proxy when `bwrap`, `socat`, and `python3` are available

## Install

Requires the COSMIC desktop plus `mpv`, `pactl` (PipeWire), and `curl`.
Optional: `bwrap`, `socat`, and `python3` for sandboxed playback, and
`mpv-mpris` for desktop media integration.

```bash
just build-release
sudo just install
```

This installs the app, the applet, and the playback helpers. Then:

- Launch **Radio Atlas** from the app library.
- Add the **Radio Atlas Applet** to the panel from COSMIC Settings →
  Desktop → Panel.

To remove:

```bash
sudo just uninstall
```

Favorites, history, volume, and output stay in
`~/.local/share/radio-atlas/state.json`, shared with the legacy release, so
existing data carries over. Delete that directory to remove them too.

## Usage

See [docs/USAGE.md](docs/USAGE.md) for the full guide. The short version:

- **Globe**: drag or flick to spin, scroll to zoom, click a signal to play,
  click a country to browse it. Hover details appear in the status line below.
- **Sidebar**: World, Favorites, and Recent tabs; search in the header;
  `R` or the shuffle button tunes randomly.
- **Player**: transport buttons, volume slider, and the speaker button to
  pick an audio output. `?` shows the in-app controls page.
- **Applet**: the panel icon reflects playback state; its popup offers
  transport, volume, random tuning, favorites, and one click to open the app.

| Input              | Action                        |
| ------------------ | ----------------------------- |
| Drag / flick globe | Rotate; a flick coasts        |
| Wheel over globe   | Zoom                          |
| Click signal       | Play station                  |
| Click country      | Browse country                |
| `/`                | Focus search                  |
| Up / Down          | Move through stations         |
| Enter              | Play selected station         |
| Space              | Play or pause                 |
| `R`                | Tune a random station         |
| `F`                | Favorite selected station     |
| `M`                | Mute or unmute                |
| `+` / `-`          | Raise or lower radio volume   |
| `?`                | Show or hide controls         |
| Escape             | Back or clear search          |

## Audio outputs and AirPlay

The speaker button lists every PipeWire sink via `pactl`. "System default"
follows the desktop's current output, the choice is saved with the volume,
and switching while playing takes effect immediately. If the chosen device
disappears, mpv may pause; pick another output and resume.

AirPlay speakers appear once PipeWire exposes them as RAOP sinks. On Arch:

```bash
sudo pacman -S pipewire-zeroconf
mkdir -p ~/.config/pipewire/pipewire.conf.d
cp /usr/share/pipewire/pipewire.conf.avail/50-raop.conf \
  ~/.config/pipewire/pipewire.conf.d/
systemctl --user restart pipewire wireplumber
```

Behind a firewall, allow the timing feedback speakers send back (UDP
6001–6002) or the session connects but stays silent. If a speaker vanishes
from the list, restart `pipewire`/`wireplumber` to re-discover it. RAOP
streams uncompressed PCM: AirPort Express, Apple TV, many receivers, and
HomePods accept it, but AirPlay 2-only features such as HomePod stereo pairs
are not supported.

## Files and data

- Runtime (`$XDG_RUNTIME_DIR/cosmic-radio-atlas/`): station catalogs,
  player queue, `status.json`, mpv socket, `mpv.log`, `proxy.log`
- Cache (`~/.cache/cosmic-radio-atlas/`): world and per-country catalogs
  with background refresh
- Saved (`~/.local/share/radio-atlas/state.json`): favorites, history,
  volume, audio output — never overwritten when invalid

See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for the full layout and the
protocols between the UI, the player, and mpv.

## Troubleshooting

- **Nothing plays**: install `mpv`; check `mpv.log` in the runtime directory
  for station or network errors.
- **"Audio outputs are unavailable"**: install `pactl` (PipeWire).
- **A station fails**: the app keeps it selected and shows the error — press
  play to retry, or Next/Previous for another queued station. It never
  auto-switches. Repeated opening clips come from the station's own server.
- **Saved state errors**: back up `~/.local/share/radio-atlas/state.json`
  before repairing or deleting it.
- **Sandbox check**: while playing, `pgrep -af radio-session` shows the
  sandbox supervisor when the network namespace is active.

## Development

```bash
cargo test -p radio-core -p radio-player -p radio-tasks
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo run -p cosmic-radio-atlas
```

`just test`, `just check`, `just fmt`, and `just run` wrap the same commands.
[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) covers the architecture, the
backend protocols, and packaging.

| Path                 | Contents                                                        |
| -------------------- | --------------------------------------------------------------- |
| `app/`               | COSMIC application: globe, lists, search, player                |
| `applet/`            | COSMIC panel applet with popup controls                         |
| `crates/radio-core/` | Station model, globe math, Radio Browser client, saved state    |
| `crates/radio-player/` | mpv sessions, JSON IPC, PipeWire outputs, sandbox helpers     |
| `crates/radio-tasks/` | Async background tasks shared by the app and applet            |
| `docs/`              | Usage and development guides                                    |
| `legacy/`            | Original Omarchy/QML implementation and its tests               |

## Data and privacy

Station data comes from the community-run
[Radio Browser](https://www.radio-browser.info/). The client sends its name
and version as the HTTP user agent, and starting a station calls Radio
Browser's click-count endpoint. Favorites and history stay local.

Station metadata and stream URLs are community supplied and rendered as
plain text. Sandboxed playback rejects private and effectively local
destinations, including after redirects; remote payloads are size- and
record-limited before parsing. Radio Atlas still connects to third-party
stations, and HTTP streams are unencrypted — only play stations you trust.

Map geometry is public-domain Natural Earth data (see
`crates/radio-core/assets/NOTICE.md`).

## License

MIT — see [LICENSE](LICENSE).
