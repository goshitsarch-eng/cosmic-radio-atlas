# Development guide

Architecture, backend protocols, and packaging for Cosmic Radio Atlas. For
day-to-day commands see the [README](../README.md); for behavior from a
user's perspective see [USAGE.md](USAGE.md).

## Architecture

The workspace has three library crates and two binaries:

| Crate             | Role                                                              |
| ----------------- | ----------------------------------------------------------------- |
| `radio-core`      | Station model, globe math, Radio Browser client, saved state      |
| `radio-player`    | mpv sessions, JSON IPC, PipeWire outputs, sandbox helpers         |
| `radio-tasks`     | `spawn_blocking` wrappers shared by the app and the applet        |
| `cosmic-radio-atlas` | Main window: globe canvas, lists, search, player               |
| `cosmic-applet-radio-atlas` | Panel icon with transport/volume popup               |

Design rule: the backend crates are **synchronous** (plain `std`, files,
processes, sockets) and fully unit-testable without a display. Async exists
only at the UI edge, where `radio-tasks` moves blocking calls onto worker
threads and reports back as messages — the same shape as the legacy UI
shelling out to processes.

`unsafe` is forbidden workspace-wide except in `radio-player`, which needs
three audited calls (`kill` for liveness/group signaling, `setsid` before
`exec`).

## Backend protocols

All UI surfaces (app, applet) coordinate through files and the mpv socket —
there is no daemon, so the panel and the window cannot disagree.

### Directories

- Runtime `$XDG_RUNTIME_DIR/cosmic-radio-atlas/`: catalogs (`world.json`,
  `results.json`), queue (`playlist.json`, `playlist.m3u`),
  `play-selection.json`, `favorite-selection.json`, `status.json`,
  `mpv.sock`, `proxy.sock`, `player.pid`, `player.lock`,
  `player.generation`, `mpv.log`, `proxy.log`.
- Cache `$XDG_CACHE_HOME/cosmic-radio-atlas/`: `world.json` + `world.lock`,
  `countries/<CODE>.json` + `.lock` files.
- Saved `$XDG_DATA_HOME/radio-atlas/` (default `~/.local/share/`):
  `state.json` + `state.lock`, shared with the legacy release.

Writes are atomic (temp file + rename). Mutations take `flock`-style
exclusive locks; background cache refreshes use non-blocking locks so at
most one refresh runs per cache.

### Player

`Player::play` prepares queue files **before** taking the player lock, so a
slow lookup never blocks `stop`. A `player.generation` counter, bumped by
`stop`, cancels stale in-flight plays. The mpv session leader's pid plus
its `/proc` start time are recorded in `player.pid`, so a recycled pid is
never mistaken for the player; shutdown signals the whole process group
(`SIGTERM`, escalating to `SIGKILL`).

mpv speaks JSON IPC over `mpv.sock`. Commands carry request id `9001` and
succeed only on an explicit `"error": "success"` reply; status is one batch
of eight `get_property` queries (pause, title, position, count, volume,
mute, audio device, stream failure). The bundled `radio-status.lua` (env:
`RADIO_ATLAS_STATUS_FILE`, `RADIO_ATLAS_QUEUE_FILE`) publishes `status.json`
on playback events and handles `radio-atlas-toggle` (play/pause, or retry
after a failure) and `radio-atlas-reload` (queue refresh).

`status.json` schema: `running`, `paused`, `muted`, `title`,
`playlistPosition`, `playlistCount`, `volume`, `output`, `loaded`, `error`,
`errorDetail`, and `station` (`uuid`, `name`, `country`, `countryCode`,
`latitude`, `longitude`).

### Sandbox

When `bwrap`, `socat`, `python3`, and the installed helpers are present,
mpv runs inside `radio-session`: a filtering proxy on a unix socket plus a
network namespace whose loopback proxy env routes station traffic through
it. Otherwise mpv spawns directly. Helpers resolve from
`COSMIC_RADIO_ATLAS_LIBEXEC`, then exe-relative and `/usr[/local]/lib`
paths; without them the embedded Lua script is staged into the runtime dir
so development builds behave identically. The proxy omits URLs, hostnames,
and raw errors from its logs.

### Directory client

Radio Browser mirrors are discovered via `getent` (`ahostsv4` +
reverse-resolve), shuffled, with `https://all.api.radio-browser.info` as
fallback (`COSMIC_RADIO_ATLAS_API_BASES` overrides for tests). Requests are
HTTPS-only with 4s connect / 12–45s total timeouts, 4 MiB bodies, and
single-document validation (at most 500 rows). Responses are normalized:
control characters stripped, fields truncated, coordinates range-checked,
rows without id or `http(s)` URL dropped, uuids deduplicated.

Caches are stale-while-revalidate (24h): fresh caches serve instantly, stale
ones serve while a background refresh runs. Fresh country caches short-
circuit the network entirely.

### Saved state

`state.json` holds `favorites` (≤500), `recent` (≤30), `volume` (0–100),
and `output` (validated sink name). It is validated before every read and
write; an invalid file is reported and **never overwritten**. Favorite and
history updates resolve the station against the live runtime files first.

## UI structure

Both frontends are `libcosmic` applications pinned to the `libcosmic` git
HEAD in `Cargo.lock`.

- **App** (`app/src/app.rs`): one `Message` enum drives fetching (with a
  single pending slot), world expansion, selection, playback, volume, and
  outputs. Status polls every 500 ms; a 16 ms ticker runs only while kinetic
  globe motion is active; `keyboard::listen` provides shortcuts without
  fighting text input.
- **Globe** (`app/src/globe.rs`): an iced `canvas::Program`. Press/drag
  tracking lives in canvas-local state; the app owns the camera, kinetic
  velocities, and highlights. Merged geo-stations are cached in the model —
  `view` only reads, so drags never recompute them.
- **Applet** (`applet/src/app.rs`): polls the same player status every
  second; Random chains fetch → play with a generation token so Stop cancels
  cleanly; Open launches `cosmic-radio-atlas` (single-instance focuses it).
- **i18n**: Fluent catalogs under `app/i18n` and `applet/i18n` via the
  `fl!` macro; desktop and metainfo files are expanded by `xdgen` in each
  `build.rs` into `<package>/target/xdgen/`.

## Testing

```bash
cargo test -p radio-core -p radio-player -p radio-tasks
```

Suites run offline: mpv IPC and `pactl` are faked in-process, and a
fake-mpv lifecycle test covers play queue preparation through
stop-and-cleanup. Globe projection, kinetic physics, normalization, caches,
and state validation all have unit tests. No test touches the network.

Legacy Omarchy tests (Qt, mpv, shell fixtures) still run from their new
home and in CI:

```bash
cd legacy && ./tests/run
```

## Packaging

```bash
just build-release
sudo just install
```

Install layout (under `$prefix`, default `/usr`):

| Installed file(s)                                              | Destination                                    |
| -------------------------------------------------------------- | ---------------------------------------------- |
| `cosmic-radio-atlas`, `cosmic-applet-radio-atlas`              | `bin/`                                         |
| `io.github.goshitsarcheng.RadioAtlas[.desktop, .metainfo.xml]` | `share/applications`, `share/appdata`          |
| `io.github.goshitsarcheng.RadioAtlasApplet[.desktop, .metainfo.xml]` | same                                       |
| `io.github.goshitsarcheng.RadioAtlas.svg`                      | `share/icons/hicolor/scalable/apps/`           |
| `radio-session`, `radio-sandbox`, `radio-proxy`, `radio-status.lua` | `lib/cosmic-radio-atlas/`                  |

`Cargo.lock` is committed and all recipes build `--locked`. CI installs
`libwayland-dev libxkbcommon-dev libegl-dev libdbus-1-dev libexpat1-dev`
plus the Rust stable toolchain with clippy and rustfmt, then runs
formatting, backend tests, `clippy -D warnings`, and a full workspace
check. `just uninstall` removes everything above; user data under
`~/.local/share/radio-atlas/` is left alone.
