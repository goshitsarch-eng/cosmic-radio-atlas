# Usage guide

This guide covers everyday use of the Radio Atlas app and panel applet. For
installation see the [README](../README.md); for architecture and hacking see
[DEVELOPMENT.md](DEVELOPMENT.md).

## The globe

The globe opens on an Atlantic-centered view and loads the cached world
catalog instantly, then keeps adding stations in the background until it
holds up to 5000. Each dot is a station; brighter dots face you more
directly.

- **Drag** to rotate. A slow release stops where you left it.
- **Flick** to spin with momentum. When the spin settles, the most face-on
  station (other than the playing one) is highlighted as a landing
  suggestion — click it to play.
- **Scroll** over the globe to zoom, from the full disc down to 24x.
- **Click a signal** to play that station. If it isn't in the current list,
  the list switches to World first.
- **Click a country** to browse its stations (see below).
- **Hover** a signal to show its name in the status line under the globe,
  next to the live signal count.

The globe follows your COSMIC light/dark theme and accent color.

## Station lists

The sidebar shows one list at a time:

- **World**: the full session catalog, largest first and growing as
  background expansion adds pages. Expansion slows down when requests stop
  adding stations and gives up after three empty attempts.
- **Favorites / Recent**: your saved stations and listening history, up to
  500 favorites and 30 recent entries.

The playing station shows a play marker; the selected row is highlighted.
Long lists render the first 300 rows — search or a country view narrows them.

Click a row to play it. The heart button favorites or unfavorites that
station without disturbing playback.

## Search

Type in the header search field (or press `/`). Matches appear instantly
from the loaded catalog while a directory search runs 300 ms after you stop
typing, querying station names, countries, and tags in parallel. Clearing
the field returns to the World view.

## Country browsing

Clicking a country (or picking one implicitly through playback) shows its
stations: cached matches first, then a directory refresh. Stations without
published coordinates are placed at deterministic pseudo-locations inside
their country outline so they still appear on the globe. The globe header
names the active country until playback or another view changes it.

## Random tuning

`R`, the shuffle button in the header, or the applet's Random button tunes a
random playable station, skipping the playing station, the last random pick,
and recent history. The view switches to the fresh random page and starts
playing its first station.

## Playback and failures

Transport controls (previous, play/pause, next, stop) and the volume slider
sit below the list; the status line names the station, the current track
when the stream reports one, and the queue size.

If a stream disconnects or cannot be opened, Radio Atlas keeps the station
selected and shows the failure — it never auto-switches. Press play to retry
that exact station, or Next/Previous to move through the queue. A repeated
opening clip comes from the station's own server; Radio Browser supplies
listings, not audio.

With `mpv-mpris` installed, desktop media keys and player widgets control
playback too.

## Volume and outputs

The volume slider (0–100, committed on release) and the mute button affect
only the radio. The speaker button swaps the list for the audio-output
chooser:

- **System default** follows the desktop's current output.
- Every PipeWire sink is listed by its description; AirPlay speakers show
  their network address so identical names stay distinguishable.
- Switching while playing takes effect immediately and is saved with the
  volume.

If the chosen device disappears, mpv may pause and not resume on its own —
pick another output and press play. See the README's AirPlay section for
RAOP setup on Arch.

## The panel applet

Add **Radio Atlas Applet** from COSMIC Settings → Desktop → Panel. The panel
icon shows a speaker when idle and a play marker while playing. Its popup
offers transport controls, a volume slider with mute, random tuning, a
favorite toggle for the playing station, and one click to open the full app.

The applet and the app drive the same backend, so they never disagree about
what is playing.

## Keyboard reference

| Input     | Action                      |
| --------- | --------------------------- |
| `/`       | Focus search                |
| Up / Down | Move through stations       |
| Enter     | Play selected station       |
| Space     | Play or pause               |
| `R`       | Tune a random station       |
| `F`       | Favorite selected station   |
| `M`       | Mute or unmute              |
| `+` / `-` | Raise or lower radio volume |
| `?`       | Show or hide controls       |
| Escape    | Back or clear search      |

Shortcuts are ignored while typing in search or holding Ctrl/Super.

## Tips

- World and country catalogs cache for 24 hours and refresh in the
  background, so reopening the app is instant even offline.
- The `?` button (or key) opens the in-app controls page; the header's info
  button opens About.
- Window title tracks the current view: World, country, search, Random,
  Favorites, or Recent.
