# trayplay

Systray Jellyfin music player. The tray icon is the home; clicking it opens a popup
that is the entire UI. Playback is controllable over MPRIS (`playerctl`, media keys).

Scope: Jellyfin only, random play, go-to-artist, go-to-album, next/prev/play-pause.
No in-app volume control - the system mixer owns volume.

Status: **milestone 5**. Tray, popup UI (now playing + artist/album browsing),
theming, Jellyfin read path, audio playback, and MPRIS.

The popup opens on the now-playing view: cover art as a blurred backdrop, title,
artist and album (both clickable to navigate there), seek bar, transport with
**shuffle** at its right end, and a bottom row of **Settings**, **Library** and
**Queue**. Library goes artists → albums → tracks.

Activating a track plays it and shuffles the rest of its page's scope behind it:
on an album page that is the album, on an artist page it is everything by that
artist. Each page's **Play** button plays that page's tracks in listing order
instead.

There is no search page: every list page filters as you type, so the library
covers it. The gap that leaves is finding an album or track whose artist you
cannot remember - only the artist list is loaded, so the filter cannot see
further than that.

**Queue** lists what the player is holding, with the current track marked.
Activating a row plays from that point.

Tracks with no cover art collapse the reserved art space rather than showing an
empty gap, so a sparsely tagged library still looks deliberate.

Hiding the popup — auto-hide, Escape, the tray icon — resets it to now-playing, so
it always opens where you left it conceptually rather than halfway down a
discography.

## Keyboard

On the now-playing view only. Pushed pages type into their filter bar instead, and
Escape closes the popup anywhere.

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

`A` focuses rather than navigating because a track can credit several artists and
picking one is the point. While an artist is focused the arrows move between
artists instead of seeking, and `Enter` opens the focused one. That state ends on
`Enter`, on `Escape`, on any other shortcut, or by itself after five seconds — so
the arrows go back to seeking without needing to be told.

Anything with Ctrl or Alt is left to the window and the desktop.

On list pages (library, artist, album, queue):

| Key | Action |
|---|---|
| `↑` / `↓` | Move between rows, **across sections** — Albums into Other tracks and back |
| `→` | Activate the focused row, same as `Enter` |
| `←` | Back |
| `Shift`+`Enter` | The row's first menu entry: **Add to queue** on a library page, **Remove from queue** on the queue |
| any character | Opens the filter bar and types into it; the arrows then move the caret |

Right-clicking a row opens that menu, which is where the shortcut is discoverable. On
library pages (artists, albums, tracks, search hits) it offers **Add to queue** and
**Play next** — append to the end, or slot in directly after the current track. Either
way nothing starts playing, and a random queue keeps refilling behind the additions.
An artist row queues that artist's whole catalogue, an album row the album.

On the queue page it offers **Remove from queue**. The track playing right now cannot
be removed, and neither can one already handed to the audio thread for a gapless
transition (only ever the following track, in its last few seconds) — both say so with
a toast rather than doing nothing.

## Tray interactions

| Input | Action | X11 (XEmbed) | Wayland/KDE (SNI) |
|---|---|---|---|
| Left click | Show, raise, or hide the popup (see below) | yes | yes |
| Middle click | Play/pause | yes | yes |
| Scroll up / down | Next / previous track | yes (vendored fix - see below) | yes |
| Right click | Context menu | no menu at all - see below | yes (SNI `ContextMenu`, not rebindable) |

Left click is not a plain visible/hidden toggle. Hidden → shown; on screen **with** focus →
hidden; on screen **without** focus (behind another window, or on a tag you switched back
to) → raised and focused, because closing it there would just throw away what you wanted to
look at. The exception is a blur less than 1.2s old, which still counts as focused: under
focus-follows-mouse the popup is unfocused the moment the pointer reaches the tray, and
without that grace the click could never close it again.

Quit is **Ctrl+Q** on the popup window everywhere, not a tray menu item - X11's tray icon
has no menu to put one on (`tray::TrayIconEvent` has no concept of one, and its `tray-menu`
companion needs GTK3 alongside this GTK4 app to draw one; not worth linking two GTK
versions into one process for six menu items). See "Tray: two backends, one per display"
in `CLAUDE.md` for why X11 and Wayland/KDE behave differently here.

The scroll direction under SNI depends on the host: `delta`'s sign is not fixed by the
spec. Swap the arms in `sni::Tray::scroll` (`src/tray/sni.rs`) if it steps the wrong way.
X11 scroll goes through a locally vendored, patched copy of the `tray` crate
(`vendor/tray/`, see `PATCH.md` there) - upstream misreports scroll-wheel notches as left
clicks, which made scrolling toggle the popup open/shut instead of doing anything with the
scroll itself.

## MPRIS

Registers as `org.mpris.MediaPlayer2.trayplay`:

```sh
playerctl -p trayplay status
playerctl -p trayplay metadata
playerctl -p trayplay play-pause / next / previous / position 30
playerctl -p trayplay loop Track|Playlist|None
```

Transport, metadata and `LoopStatus`. `Volume` is absent — the system mixer owns
volume — and so is `Shuffle`: random play here is a way of *building* a queue, not a
switch on an existing one, so there is nothing for the property to toggle.
`LoopStatus` is the same setting as the repeat button (`Playlist` = repeat the queue,
`Track` = repeat this track), so changing it either way updates the other, and it
survives a restart. `Raise` shows the popup, `Quit` exits.

## Login

```sh
trayplay login --server https://jellyfin.example.org --username gurkan
trayplay dump-random --limit 20   # verifies auth and queries without the UI
trayplay logout
```

`--server`/`--username` fall back to `config.toml` when omitted. The password is read
from the terminal without echo and never stored.

## Build

```sh
nix develop          # devShell with the pinned toolchain, GTK4, playerctl
cargo run
```

or

```sh
nix build && ./result/bin/trayplay
```

Note: `Cargo.toml` version requirements were written without crates.io access. Run
`cargo update` on a networked host to generate `Cargo.lock` against current releases
before the first build.

### Bundled fonts

Drop `.ttf`/`.otf`/`.ttc` files into `data/fonts/` and they are compiled in on the next
build — the family name is read out of the font itself, so there is no list to update.
They are used by the no-art panel, which picks one per album. Empty is fine and is the
default. Everything bundled there is **SIL OFL 1.1**, with each font's notice shipped
beside it — see `data/fonts/README.md`, which also covers why the files are written to
`$XDG_DATA_HOME/fonts/trayplay/` at startup.

## Running under AwesomeWM (X11)

One extra piece is needed on X11: the tray icon itself docks natively (no bridge process
required, see `CLAUDE.md`), but popup placement still needs a WM rule.

### Popup placement

GTK4 removed window positioning API on X11, so the WM places the popup. trayplay
sets a stable `WM_CLASS` of `trayplay`:

```lua
-- rc.lua
ruled.client.append_rule {
  rule = { class = "trayplay" },
  properties = {
    floating     = true,
    ontop        = true,
    skip_taskbar = true,
    placement    = awful.placement.top_right + awful.placement.no_offscreen,
  },
}
```

Under Wayland (somewm) this isn't needed: the tray is native SNI and the popup uses
`gtk4-layer-shell`, anchored per the `anchor` config key.

## Configuration

`$XDG_CONFIG_HOME/trayplay/config.toml`, all keys optional:

```toml
server             = "https://jellyfin.example.org"
username           = "gurkan"
anchor             = "top-right"   # top-left|top-right|bottom-left|bottom-right
margin             = 8
width              = 380
height             = 520
hide_on_focus_loss = false         # initial value only; the settings page owns it
random_batch       = 100
cache_max_mb       = 2048
prefetch_next      = true
```

`anchor` and `margin` only take effect under Wayland/layer-shell. On X11 the WM rule
above decides placement.

The Jellyfin access token is never stored here. It lives in
`$XDG_CONFIG_HOME/trayplay/credentials.toml`, mode `0600` in a `0700` directory.
trayplay refuses to read it if the mode is looser than that. A secret-service
backend can be slotted in behind the `TokenStore` trait later.

`device_id` next to it is a generated UUID identifying this install to Jellyfin's
session list. Deleting it just creates a new server-side session entry.

### Settings page

What the popup's own settings page changes is written to
`$XDG_STATE_HOME/trayplay/settings.toml`, not to `config.toml` - a hand-edited
file should not be rewritten by a switch.

```toml
color_scheme       = "dark"   # light|dark; absent means follow the desktop
hide_on_focus_loss = false    # absent means take config.toml's value
```

**Dark mode** starts out matching the desktop's preference. Flipping it forces the
choice from then on, so a night-light schedule cannot flip the popup back. Delete
the key to follow the desktop again.

**Auto-hide when unfocused** closes the popup as soon as it loses focus. Off by
default, because under focus-follows-mouse it would close every time the pointer
crossed the window. It applies immediately, no restart. When the key is absent
`config.toml`'s `hide_on_focus_loss` is used, so an existing setting there still
means something; once the switch is touched, this file wins.

## Track cache

Tracks are downloaded to `$XDG_CACHE_HOME/trayplay/` before being decoded, not
streamed straight into the decoder. Decoders probe by seeking - an MP3 reader wants
the ID3v1 tag in the last 128 bytes, and an MP4/M4A `moov` atom may sit at the end
of the file - and rodio reports the stream length as unknown, so a seek past the
download head fails. rodio 0.20 also converts a seek error during initialisation
into a panic.

The cost is a short wait before the first track; the next one is prefetched while
the current plays, so transitions stay gapless. `cache_max_mb` bounds the directory,
pruned least-recently-used.

## Codecs

flac, mp3, m4a/aac, alac and wav are decoded locally. Everything else - Opus, WMA,
APE, DSD - is transcoded by the server, because playback goes through
`/Audio/<id>/universal` with a container whitelist rather than
`/stream?static=true`.

Decoding uses a symphonia-backed `rodio::Source` of our own
(`src/player/decoder.rs`) rather than `rodio::Decoder`, which forces gapless
handling that breaks on transcoded mp3, panics on seek errors, and hides the stream
length so transcoded tracks cannot be seeked.

Ogg is deliberately not on the direct-play list: it usually carries Opus, which
symphonia cannot decode, and a container name cannot tell Opus from Vorbis. That
costs a re-encode on Vorbis files and makes Opus play at all.

## Theming

CSS only. `data/default.css` is compiled into the binary and loaded at
`APPLICATION` priority; your `$XDG_CONFIG_HOME/trayplay/theme.css` is loaded at
`USER` priority, so you only need to state what you want to change. The file is
watched and reapplied without restarting.

Selectors are a stability contract - renames are treated as breaking changes.

| Selector | Widget |
|---|---|
| `#trayplay-popup` | the popup window. Also carries `light` or `dark`, whichever palette is in force — GTK CSS has no prefers-color-scheme query, and the two want different scrim strengths |
| `#trayplay-nav` | navigation stack inside the window |
| `.trayplay-body` | content box on every page |
| `#trayplay-art` | cover art backdrop. Custom widget — **do not apply `filter: blur()`**, it draws its own sharp-to-blurred gradient (radius and fade band are in `src/ui/artwork.rs`). The no-art panel is drawn here too and is **not themable**: its colours come from a hash of the album name, and its angle, tiling, slide and zoom from constants in the same file |
| `#trayplay-art-space` | space reserved at the top for the art; sized only under `.has-art` |
| `#trayplay-scrim` | gradient that fades the art out behind the text |
| `.has-art` | set on the now-playing root whenever a track is showing — real cover art *or* the generated no-art panel. Only "nothing playing" leaves it off and collapses the space |
| `#trayplay-tags` | box holding title, artists and album. Its `opacity` is animated on a track change — **do not set opacity here**, it will be overwritten |
| `#trayplay-title` | track title |
| `#trayplay-artists` | strip holding one button per credited artist; scrolls by wheel or drag and fades its own edges — **do not give it a background**, the fade is a mask over what it draws |
| `.trayplay-artist` | one artist button inside that strip (navigates to that artist). A class, not an id: there can be several |
| `#trayplay-album` | album button (navigates to the album) |
| `#trayplay-seek` | seek slider. Style the parts through its GTK nodes: `trough` (track), `trough highlight` (played portion), `slider` (handle — sized to nothing at rest, a translucent block on `:hover`/`:active`) |
| `#trayplay-seek.seeking` | set while a seek is issued but not yet in effect; pulses `trough highlight` |
| `#trayplay-seek value` | elapsed time, drawn above the handle. Always present so its height never shifts the layout; `#trayplay-seek.showing-value` is what makes it visible, on hover. There are no permanent elapsed/total labels — `#trayplay-position` and `#trayplay-duration` are gone |
| `#trayplay-transport` | centre box holding the transport buttons, with repeat at its left end and shuffle at its right |
| `.trayplay-glyph` | transport controls, drawn as bare glyphs — the rule that strips the button shape in every state. Adwaita's `.flat` is not enough, it still paints a hover background |
| `#trayplay-prev` / `#trayplay-play` / `#trayplay-next` | transport buttons. Glyph *sizes* are not themable: `GLYPH_SIZE` / `PLAY_GLYPH_SIZE` in `src/ui/nowplaying.rs` are set with `set_pixel_size`, which CSS cannot override |
| `#trayplay-random` | shuffle, at the right of the transport row |
| `#trayplay-repeat` | repeat, at the left of the transport row. `#trayplay-repeat.repeat-active` is set while repeat is on (either kind), since the three glyphs are close at this size |
| `#trayplay-actions` | bottom action row |
| `#trayplay-settings` | settings button, bottom left, square |
| `#trayplay-library` | library button |
| `#trayplay-queue` | queue button |
| `#trayplay-settings-page` | settings page body |
| `#trayplay-dark-switch` / `#trayplay-motion-switch` / `#trayplay-autohide-switch` | its three switches |
| `#trayplay-list` | list box on artist/album/track pages |
| `#trayplay-section` | section heading ("Albums", "Other tracks") |
| `#trayplay-filter` / `#trayplay-filter-entry` | type-to-filter bar on list pages |
| `.trayplay-row` | a row in those lists |
| `#trayplay-page-action` | header button on a list page ("Play") |
| `#trayplay-status` | signed-out text |
| `#trayplay-toast` | toast overlay wrapping the navigation stack; the banner itself is libadwaita's `.toast` node inside it |
| `#trayplay-row-menu` | a row's right-click menu; its entries are `button` nodes inside it |

`#trayplay-error` was removed: a failed library query is a toast now, not a page of its
own, so there is no error label left to style. Restyle `#trayplay-toast .toast` instead.

Debugging: `GTK_DEBUG=interactive cargo run` opens the GTK inspector, which shows the
live CSS node tree. Use it when a container stays opaque — it names the exact node.

### Transparency and blur

The baseline theme is translucent: `alpha(@window_bg_color, 0.55)` on
`#trayplay-popup`, with the nested containers that paint their own background
cleared so the tint applies once.

There is no CSS property for blur-behind. GTK only makes the window translucent;
the compositor blurs what shows through. On X11 that means picom with
`blur-background = true` and a compositing backend (`glx` or `egl`). Under Wayland
transparency works but blur depends on the compositor supporting it.

Everything that follows from being translucent is in `default.css` too: text
carries `text-shadow` and glyphs a `drop-shadow` instead of being dimmed, and list
rows and the action buttons keep a faint `@card_bg_color` fill so they do not
vanish into it.

`#trayplay-scrim` is what makes the lower half read as dark (or light) rather than
as album art: it ramps `@window_bg_color` from transparent at the top to `0.88`
near the bottom, stopping short of opaque so the compositor still has something to
blur. Raising the ceiling hides more of the art, lowering it lets a bright cover
haze the text.

Light mode uses about two thirds of that (`0.6`), and a thinner window tint. The
same alpha of white is far heavier than of a dark colour — it flattens the art
into a sheet, where dark reads as depth. Those overrides hang off
`#trayplay-popup.light`, and the text shadows flip from dark haloes to light ones
under it.

Tune `alpha(@window_bg_color, 0.55)` for the transparency/legibility trade-off.
With no compositor running, or to go back to a solid window:

```css
/* ~/.config/trayplay/theme.css */
#trayplay-popup {
  background-color: @window_bg_color;
}
```

**Do not give the transport buttons a background here.** `theme.css` is `USER`
priority and outranks everything in `default.css`, so a
`#trayplay-transport button { background-color: ... }` rule puts the button shape
back behind glyphs that are drawn without one - and no rule in the built-in theme
can win against it.

## Logging

`RUST_LOG=trayplay=debug cargo run`

## Icons

Transport and action icons come from [ionicons](https://github.com/ionic-team/ionicons)
and [Phosphor](https://github.com/phosphor-icons/core), both MIT. The files live in
`data/icons/scalable/actions/`, renamed to `trayplay-*-symbolic` so they neither
shadow icon theme names nor lose GTK's symbolic recolouring. Each set keeps its
own `LICENSE` and a `SOURCES.md` mapping every file back to its upstream name,
under `data/icons/ionicons/` and `data/icons/phosphor/`.

`build.rs` compiles them into a GResource bundle with `glib-compile-resources`,
which `src/icons.rs` registers on the display's icon theme at startup. To add
one: drop the file in, add a line to `data/icons/trayplay.gresource.xml`, and
refer to it by name.
