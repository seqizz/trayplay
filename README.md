# trayplay

A systray-focused Jellyfin music player for Linux.

What:

- Jellyfin only: Browse artists, albums, tracks, play, queue, repeat.
- MPRIS support.
- Random play that refills itself, or an album, or one artist's catalogue.
- Server-side search over artists, albums and tracks, with some fuzziness.
- Instant mix: Jellyfin's own "more like this" queued behind the current track.
- Queue that survives a restart, with add-to-queue and play-next features.
- Gapless transitions, seeking, and a local track cache.
- Generated cover panel for tracks that have no art.
- X11 (native XEmbed tray) and Wayland (SNI, `gtk4-layer-shell`) support.
- Themable through CSS with a documented, stable set of selectors.

What NOT:

- Volume control (the system mixer owns volume)
- Playlists
- Library management
- Any backend other than Jellyfin

Further reading: **[DESIGN.md](DESIGN.md)** for how it behaves and why,
**[THEMING.md](THEMING.md)** for the CSS contract.

## Requirements

- A Jellyfin server, and an account on it, duh.
- GTK4 and libadwaita. On NixOS the flake handles this; elsewhere you need the GTK4,
  libadwaita, `gtk4-layer-shell` and ALSA development packages.
- On X11, a window manager rule to place the popup: see
  [Running under X11](#running-under-x11).
- Optional: a compositor with blur, if you want better transparency.

## Run / Install

Since I am using NixOS, it's all dark magic. Feel free to build for your own distro. I'm not going to provide packages but you can find pre-built binaries under [releases](https://git.gurkan.in/gurkan/trayplay/releases) section for Linux.

If you have nix, you can test by running it straight from the flake:

```sh
nix run 'git+https://git@git.gurkan.in/gurkan/trayplay.git'
```

## Login

```sh
trayplay login --server https://${SERVER_URL} --username ${USERNAME}
trayplay dump-random --limit 20  # verifies auth and queries without the UI
```

## Using it

Tries to be keyboard-first, with OK mouse support.

### Keyboard

Main window:

| Key | Action |
|---|---|
| `Space` | Play/pause |
| `n` / `p` | Next / previous track |
| `←` / `→` | Seek back / forward 10% of the track |
| `r` | Repeat: off, whole queue, this track, off |
| `l` | Library |
| `q` | Queue |
| `s` | Settings |
| `a` | The current track's album |
| `A` | Focus the first artist |
| `Ctrl`+`Q` | Quit |

`A` focuses rather than navigating, because a track can credit several artists and picking
one is the point. While an artist is focused the arrows move between artists instead of
seeking, and `Enter` opens the focused one.

Anything with Ctrl or Alt is left to the window and the desktop, apart from Ctrl+Q.

On list pages (library, artist, album, queue):

| Key | Action |
|---|---|
| `↑` / `↓` | Move between rows, **across sections**, Albums into Other tracks and back |
| `→` | Activate the focused row, same as `Enter` |
| `←` | Back |
| `Shift`+`Enter` | The row's first menu entry: **Add to queue**, or **Remove from queue** on the queue page |
| any text | Opens the filter bar and types into it; the arrows then move the caret |

List pages also scroll by dragging, with a flick glide on release.

### Tray

| Input | Action | X11 (XEmbed) | Wayland/KDE (SNI) |
|---|---|---|---|
| Left click | Show, raise, or hide the popup | yes | yes |
| Middle click | Play/pause | yes | yes |
| Scroll up / down | Next / previous track | yes | yes |
| Right click | Context menu | no menu, toggles the popup | yes (SNI `ContextMenu`) |

### MPRIS

Registers as `org.mpris.MediaPlayer2.trayplay`:

```sh
playerctl -p trayplay status
playerctl -p trayplay metadata
playerctl -p trayplay play-pause / next / previous / position 30
playerctl -p trayplay loop Track|Playlist|None
```

## Configuration

`$XDG_CONFIG_HOME/trayplay/config.toml`, hand-edited, never written by trayplay. All keys
are optional:

```toml
server                  = "https://jellyfin.example.org"
username                = "you"
width                   = 380
height                  = 520
hide_on_focus_loss      = false         # initial value only; the settings page owns it
hide_delay_ms           = 0             # wait this long after losing focus before hiding
random_batch            = 100
cache_max_mb            = 500           # initial value only; the settings page owns it
prefetch_next           = true
library_cache_ttl_secs  = 300           # reuse a browse query for this long; 0 disables
report_playback         = true          # tell the server what is played
# This one is X11-only
x11_window_type         = "utility"     # This is the default
# Below are Wayland-only
anchor                  = "top-right"   # top-left|top-right|bottom-left|bottom-right
margin                  = 8
```

See [DESIGN.md](DESIGN.md) for details of how all of these work.

### Settings page

The popup's own settings page writes to `$XDG_STATE_HOME/trayplay/settings.toml`, which
overrides `config.toml`:

```toml
color_scheme       = "dark"    # light|dark; absent means follow the desktop
reduce_motion      = false
hide_on_focus_loss = false     # absent means take config.toml's value
cache_max_mb       = 500       # absent means take config.toml's value
repeat             = "off"     # off|all|one
```

## Running under X11

The tray icon docks natively, no bridge process. Placement is the window manager's job,
and trayplay sets a stable `WM_CLASS` of `trayplay` to match on. For AwesomeWM:

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

Do not raise, unminimise or otherwise poke the window from the window manager side.
Running `trayplay` again is the supported way to reach a running instance: the second
process hands off over D-Bus and the popup toggles exactly as a tray click does.

Under Wayland none of this is needed: the tray is native SNI and the popup is a
`gtk4-layer-shell` surface anchored per the `anchor` key.

## Theming

CSS only, with a stable set of selectors and hot reload from
`$XDG_CONFIG_HOME/trayplay/theme.css`. See **[THEMING.md](THEMING.md)** for the selector
contract, the transparency and blur setup, and the handful of properties that must be left
alone because they are animated in code.

## Icons and fonts

Icons come from [ionicons](https://github.com/ionic-team/ionicons),
[Phosphor](https://github.com/phosphor-icons/core),
[Qlementine](https://github.com/oclero/qlementine-icons), one MynaUI icon (all MIT) and
one Font Awesome 4 icon (OFL), compiled into a GResource bundle at build time. Fonts are a
drop-in directory: anything in `data/fonts/` is compiled in on the next build and used by
the no-art panel. Both are described in [DESIGN.md](DESIGN.md).

## Licence

Anything declared above as MIT is MIT, fonts and Font Awesome 4 are SIL OFL 1.1, and
everything else is WTFPL, see [LICENSE](LICENSE).
