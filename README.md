# trayplay

A systray-focused Jellyfin music player for Linux.

Features:
- Jellyfin only, read-only: browse artists → albums → tracks, play, queue.
- Random play that refills itself, or an album, or one artist's catalogue.
- Queue that survives a restart, with add-to-queue and play-next from any row.
- Repeat off / whole queue / one track, mirrored on MPRIS `LoopStatus`.
- Gapless transitions, seeking, and a local track cache.
- Works on X11 (native XEmbed tray) and Wayland (SNI, `gtk4-layer-shell`).
- Themable through CSS with a documented, stable set of selectors (see
  [THEMING.md](THEMING.md)).

Not in scope:
- Volume control (the system mixer owns volume)
- Playlists
- Library management
- Any backend other than Jellyfin

## Requirements

- A Jellyfin server, and an account on it, duh.
- GTK4 and libadwaita. On NixOS the flake handles this; elsewhere you need the
  GTK4, libadwaita, `gtk4-layer-shell` and ALSA development packages.
- For the popup to be *placed* on X11, a window manager rule — see
  [Running under X11](#running-under-x11).
- Optional: a compositor with blur, if you want the translucent look to blur what
  is behind it.

## Install

```sh
nix build && ./result/bin/trayplay
```

Or run it straight from the flake:

```sh
nix run 'git+https://git@git.gurkan.in/gurkan/trayplay.git'
```

## Login

```sh
trayplay login --server https://${SERVER_URL} --username ${USERNAME}
trayplay dump-random --limit 20  # verifies auth and queries without the UI
```

`--server`/`--username` fall back to `config.toml` when omitted.

The access token lives in `$XDG_CONFIG_HOME/trayplay/credentials.toml`, mode `0600`
in a `0700` directory; trayplay refuses to read it if the mode is looser than that.
`device_id` beside it is a generated UUID identifying this install in Jellyfin's
session list, deleting it just creates a new session entry.

## Hints

There is no separate search page, every list page filters as you type. On the
Library page that filter goes to the server and searches artists, albums *and*
tracks, so you can find a record without remembering who made it.

**Queue** shows what the player is holding, with the current track marked; it
updates live and is restored when trayplay starts again. Activating a row plays
from that point. You can add whatever you want to the queue; either at the end or
as next-to-play.

Tracks with no cover art get a generated panel instead of an empty space: a colour
derived from the album name, with that name tiled across it on an angle, drifting
slowly. To reduce motion, see [Settings](#settings-page).

### Keyboard

Main window:

| Key | Action |
|---|---|
| `Space` | Play/pause |
| `n` / `p` | Next / previous track |
| `←` / `→` | Seek back / forward 10% of the track |
| `r` | Repeat: off → whole queue → this track → off |
| `l` | Library |
| `q` | Queue |
| `s` | Settings |
| `a` | The current track's album |
| `A` | Focus the first artist |
| `Ctrl`+`Q` | Quit |

`A` focuses rather than navigating, because a track can credit several artists and
picking one is the point. While an artist is focused the arrows move between artists
instead of seeking, and `Enter` opens the focused one.

Anything with Ctrl or Alt is left to the window and the desktop, apart from Ctrl+Q.

On list pages (library, artist, album, queue):

| Key | Action |
|---|---|
| `↑` / `↓` | Move between rows, **across sections**, Albums into Other tracks and back |
| `→` | Activate the focused row, same as `Enter` |
| `←` | Back |
| `Shift`+`Enter` | The row's first menu entry: **Add to queue**, or **Remove from queue** on the queue page |
| any text | Opens the filter bar and types into it; the arrows then move the caret |

### Tray

| Input | Action | X11 (XEmbed) | Wayland/KDE (SNI) |
|---|---|---|---|
| Left click | Show, raise, or hide the popup (see below) | yes | yes |
| Middle click | Play/pause | yes | yes |
| Scroll up / down | Next / previous track | yes | yes |
| Right click | Context menu | no menu | yes (SNI `ContextMenu`) |

Quit is **Ctrl+Q** on the popup window everywhere rather than a tray menu item:
X11's tray icon has no menu to put one on.

### MPRIS

Registers as `org.mpris.MediaPlayer2.trayplay`:

```sh
playerctl -p trayplay status
playerctl -p trayplay metadata
playerctl -p trayplay play-pause / next / previous / position 30
playerctl -p trayplay loop Track|Playlist|None
```

Transport, metadata and `LoopStatus`. `Volume` is absent — the system mixer owns
volume, and so is `Shuffle`: random play here is a way of *building* a queue rather
than a switch on an existing one, so there would be nothing to toggle. `LoopStatus`
is the same setting as the repeat button (`Playlist` = repeat the queue, `Track` =
repeat this track), so changing either updates the other. `Raise` shows the popup,
`Quit` exits.

## Configuration

`$XDG_CONFIG_HOME/trayplay/config.toml`, all keys optional:

```toml
server             = "https://jellyfin.example.org"
username           = "you"
anchor             = "top-right"   # top-left|top-right|bottom-left|bottom-right
margin             = 8
width              = 380
height             = 520
hide_on_focus_loss = false         # initial value only; the settings page owns it
hide_delay_ms      = 0             # wait this long after losing focus before hiding
x11_window_type    = "utility"     # X11 only; this is the default
random_batch       = 100
cache_max_mb       = 500           # initial value only; the settings page owns it
prefetch_next      = true
```

`anchor` and `margin` only take effect under Wayland/layer-shell. On X11 the window
manager decides placement — see below.

`x11_window_type` sets `_NET_WM_WINDOW_TYPE` on the popup — the EWMH name without its
`_NET_WM_WINDOW_TYPE_` prefix. It defaults to **`utility`**, which is what this window is
by EWMH's own definition (a persistent auxiliary window, not a document window), and it
keeps window manager and compositor rules written for ordinary windows from applying to
it. GTK4 removed `set_type_hint` with no replacement, so trayplay sets the property itself,
before the window is first mapped. Set `"normal"` for GTK's own behaviour.

`hide_delay_ms` is a trade rather than an improvement, which is why it defaults to 0
(hide on the next main-loop turn). A delay swallows focus that bounces straight back,
but whatever the window manager and compositor do with a window that is about to
disappear — restacking it, fading it, re-redirecting the screen around a fullscreen
window — then happens in plain sight for that long. If the popup looks like it flashes
or jumps as it hides, that is the compositor, not the delay: with picom, try
`unredir-if-possible = false` or `fade-exclude = [ "class_g = 'trayplay'" ]`.

### Settings page

What the popup's own settings page changes is written to
`$XDG_STATE_HOME/trayplay/settings.toml`, never to `config.toml`: a hand-edited file
should not be rewritten by a switch.

```toml
color_scheme       = "dark"    # light|dark; absent means follow the desktop
reduce_motion      = false
hide_on_focus_loss = false     # absent means take config.toml's value
cache_max_mb       = 500       # absent means take config.toml's value
repeat             = "off"     # off|all|one
```

**Dark mode** starts out matching the desktop. Flipping it forces the choice from
then on, so a night-light schedule cannot flip the popup back. Delete the key to
follow the desktop again if you don't know what you want.

**Reduce motion** holds the no-art panel still. The panel is still drawn, this is
about movement.

**Auto-hide when unfocused** closes the popup as soon as it loses focus. Off by
default, because under focus-follows-mouse it would close every time the pointer
crossed the window.

**Cache limit** caps `$XDG_CACHE_HOME/trayplay/`, in megabytes, and the row shows how
much is in use. Lowering it prunes immediately rather than waiting for the next
download; entries go oldest-download-first. 500 MB by default.

## Running under X11

The tray icon docks natively — no bridge process — but GTK4 removed window
positioning on X11, so the window manager places the popup. trayplay sets a stable
`WM_CLASS` of `trayplay` to match on. For AwesomeWM:

```lua
-- rc.lua
ruled.client.append_rule {
  rule = { class = "trayplay" },
  properties = {
    floating     = true,
    skip_taskbar = true,
    -- Worth having if you run a compositor: a bordered, custom-shaped window
    -- with a shadow made picom flicker the popup on every focus change. None of
    -- the three does anything useful for a window that has no titlebar anyway.
    border_width = 0,
    shape        = gears.shape.rectangle,
    shadow       = false,
    placement    = awful.placement.top_right + awful.placement.no_offscreen,
  },
}
```

Under Wayland none of this is needed: the tray is native SNI and the popup is a
`gtk4-layer-shell` surface anchored per the `anchor` config key.

## Theming

CSS only, with a stable set of selectors and hot reload from
`$XDG_CONFIG_HOME/trayplay/theme.css`. See **[THEMING.md](THEMING.md)** for the
selector contract, the transparency/blur setup, and the handful of properties that
must be left alone because they are animated in code.

## How playback works

Tracks are downloaded to `$XDG_CACHE_HOME/trayplay/` before being decoded rather
than streamed straight into the decoder: decoders probe by seeking, and a seek past
the download head fails. The cost is a short wait before the first track; the next
one is prefetched while the current plays, so transitions stay gapless.
The cache is bounded by the limit in Settings (500 MB by default) and pruned
oldest-download-first, both at startup and after every completed download.

flac, mp3, m4a/aac, alac and wav are decoded locally. Everything else (Opus, WMA,
APE, DSD, whatever) is transcoded by the server, because playback goes through
`/Audio/<id>/universal` with a container whitelist rather than `/stream?static=true`.
Ogg is deliberately not on the direct-play list: it usually carries Opus, which the
decoder cannot handle, and a container name cannot tell Opus from Vorbis.

## Icons and fonts

Transport and action icons come from [ionicons](https://github.com/ionic-team/ionicons),
[Phosphor](https://github.com/phosphor-icons/core) and
[Qlementine](https://github.com/oclero/qlementine-icons) — all MIT — plus one MynaUI
icon. They live in `data/icons/scalable/actions/`, renamed to `trayplay-*-symbolic`
so they neither shadow icon theme names nor lose GTK's symbolic recolouring; each set
keeps its own `LICENSE` and a `SOURCES.md` mapping every file to its upstream name.
`build.rs` compiles them into a GResource bundle. To add one: drop the file in, add a
line to `data/icons/trayplay.gresource.xml`, and refer to it by name.

Fonts are a drop-in directory: put `.ttf`/`.otf`/`.ttc` files in `data/fonts/` and
they are compiled in on the next build, with the family name read out of the font
itself. They are used by the no-art panel, one per album. Empty is fine and is the
default; everything already bundled there uses SIL OFL 1.1. See `data/fonts/README.md`.

## Licence

Anything declared above as MIT is MIT, fonts are SIL OFL 1.1, and everything else is
WTFPL, see [LICENSE](LICENSE).
