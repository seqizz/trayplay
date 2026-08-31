# trayplay: how it behaves, and why

The [README](README.md) says what trayplay does and how to drive it. This file explains
the behaviour that is not obvious from the outside, and the reasoning behind the choices
that could plausibly have gone the other way. For the internals (thread model, module
layout, the constraints the code is built around) see `CLAUDE.md`; for the CSS selector
contract see [THEMING.md](THEMING.md).

## The shape of the thing

The tray icon is the application. It is always there, it shows playback state, and it
answers left click, middle click and the scroll wheel on its own. The popup is not a
window you keep open: it opens over whatever you were doing, you do one thing in it, and
it goes away. Hiding it resets it back to now-playing, so it always opens where it
started rather than halfway down a discography.

That is also why there is no titlebar, no window controls and no volume slider. The
window manager owns window decoration, the system mixer owns volume, and neither is worth
reimplementing badly inside a popup.

Anything outside the process reaches a running instance by **running the binary again**.
The second process hands off over D-Bus and toggles the popup exactly as a tray click
does. This matters on X11: raising or unminimising the window from the window manager side
tends to clear properties the placement rule sets (`skip_taskbar`, in particular), and
since the window is unmanaged on every hide, nothing restores them.

## Browsing and search

Navigation is one consistent drill-down: artist, then albums, then tracks. The links on
the now-playing view jump into the middle of it. Every list page filters as you type, so
there is no separate search destination to navigate to.

Activating a **track row** plays that track and shuffles the rest of the page's scope
behind it: the artist's whole catalogue on an artist page, that one album on an album
page. It is a choice about that track, not about listing order. If you want listing
order, the page header has a Play button. Row menus (right click, or Shift+Enter) never
start playback and never navigate.

### The Library filter is the only one that reaches the server

Everywhere else the filter is a plain match over rows already on screen, which is enough
because the whole scope is loaded: one artist's albums, one album's tracks. The Library
page's loaded scope is *only* the artist list, so it cannot answer "is there an album or
a track called this". A non-empty filter there therefore queries the server for artists,
albums and tracks at once, debounced so a burst of typing is one request rather than one
per keystroke.

Two queries make that up, not one, and both are needed. Jellyfin will not return an
artist from a recursive item query at all (an artist is not a child of a library folder),
and its search term matches item *names* rather than performer credits, so an artist's
name matches neither their albums nor their tracks either. Artists come from the artist
endpoint, everything else from the item endpoint, and the two are merged. One of them
failing does not fail the search.

### Typo tolerance, and its limits

Artist names tolerate one edit: a wrong, missing or extra character, from four characters
up. `kimela` finds *Kimera Candela*. Terms shorter than four characters stay exact,
because one edit inside three characters reaches a large share of any library.

It is artists only, and that is a deliberate stopping point rather than a half-finished
feature:

- The Library page already holds the entire artist list in memory, so matching over it
  costs nothing and works with the server unreachable.
- Albums and tracks are never loaded in full. Making them fuzzy would mean either
  prefetching the library in the background (minutes of server work and tens of megabytes
  held, for a filter box) or sending the server a truncated prefix and ranking locally
  (which needs a much larger row limit to be correct, since a short prefix's real match
  falls off the end of the result set). Both were designed and dropped.

Jellyfin has no fuzzy search itself and no setting for one, so this is local or nowhere.
Exact matches are always listed first and guesses follow, so correct spelling is never
pushed down the page by a suggestion. A transposition (`kimrea`) is two edits and does not
match.

### Ranked destinations

Above the artist list, the Library page carries three rows you can enter: Recently added,
Most played and Recently played. They are lists the *server* ranked, which is the whole
point of them, and they cost a query only when entered.

Recently added is albums rather than tracks, because a rip lands as one album's worth of
files at once and a track-level version is one album repeated twelve times. The other two
are tracks, because play counts only exist per track. Both play-history lists are built
from playback reports, so they are legitimately empty on a server that has never been
sent one.

## Playing

### Instant mix

The button beside shuffle asks Jellyfin for tracks like the one playing now (its own
similarity scoring over genres, artists and play history) and puts them in the queue
*behind* the current track. The song you pressed it on keeps playing; only what comes
next changes. The first version played the mix from its start, where the server puts the
seed track, so pressing "more like this one" took the song away in order to hand it back
from the top.

The result is finite on purpose: unlike random play it does not refill, so a mix stays a
mix and runs out. History behind the cursor is kept, so Previous walks back out of the mix
into whatever was playing before it. A toast confirms it, because nothing on screen would
otherwise change.

### The queue

The queue page shows what the player is holding with the current track marked, updates
live, and is restored when trayplay starts again. Activating a row plays from there;
tracks it skipped stay behind the cursor as history rather than being thrown away, which
is also why there is no reordering.

Adding to the queue never replaces it and never silently ends random play: a random queue
keeps refilling behind whatever you added. Adding to an empty queue does not start
playback either, it just makes Play do the obvious thing.

Two removals are refused. The track that is playing, because the player would then be
playing something the queue does not contain, and a track that has already been handed to
the audio sink, because it will be heard whatever the queue says. The same limit is why
"Play next" inserts *after* an already-committed track rather than in front of it. The
hand-over happens in the last few seconds of a track, so this is rare.

The restored queue holds whole tracks rather than references, so it works with the server
down, and it is kept as a window around the cursor rather than truncated from the front:
the future of a long queue (including anything you enqueued by hand, which lands at the
end) is the part worth keeping. Playback position is not restored, a restored track starts
from zero.

### Downloading, not streaming

Tracks are downloaded to `$XDG_CACHE_HOME/trayplay/` before being decoded rather than fed
into the decoder as they arrive. Decoders probe a file by seeking, and a seek past the
download head fails. The cost is a short wait before the first track; the next track is
fetched while the current one plays, so transitions stay gapless. Where the format allows
it (mp3), decoding starts on the partial download rather than waiting.

flac, mp3, m4a/aac, alac and wav are decoded locally. Everything else (Opus, WMA, APE,
DSD, whatever) is transcoded by the server, because playback goes through
`/Audio/<id>/universal` with a container whitelist rather than a static stream that
forbids transcoding. Ogg is deliberately not on the direct-play list: it usually carries
Opus, which the decoder cannot handle at all, and a container name cannot distinguish
Opus from Vorbis.

The cache is bounded by the limit in Settings and pruned oldest-first, at startup and
after every completed download. Lowering the limit prunes immediately. "Oldest" is by
last open, so replaying an old favourite protects it.

### Seeking is not instant, and says so

A seek may have to finish a download and rebuild a decoder, which is long enough to see.
The slider therefore pins to where you asked for and pulses until playback actually
arrives there, rather than snapping back to real position and then jumping forward. Drags
are debounced, since every seek is a decoder rebuild.

Not every track can be seeked by its own container. Where the format has no usable index,
the position is estimated from duration and file size and the decoder is started at that
byte offset. That is exact for constant bitrate, which is precisely the case that lacks an
index, and drifts on variable bitrate without a header, which is rare.

### An unreachable server should cost as little as possible

Cached tracks play with no network at all, so a restored queue of cached tracks plays
through offline. A track that cannot be fetched stops the player on that track with a
toast and Play retries *that* track; it does not march the cursor through the rest of the
queue. Only a 404 drops a track and moves on, since that one cannot succeed later.

## The no-art panel

A track with no cover art gets a generated panel rather than an empty space: a colour
derived from the album name, with the name tiled across it on an angle, drifting slowly
and zooming very slightly.

The colour and the font are derived from the album name by hash, not chosen at random, so
one record always looks the same and the panel becomes a weak identity for it. The seed is
the album, falling back to all credited artists and then the title, so every song on a
record does not get a different colour. It is not themable, because the colours come from
the hash rather than from CSS.

The animation runs only while the popup is actually on screen, which is a small fraction
of the time, and Reduce motion in Settings stops it. The panel is still drawn: the setting
is about movement, not decoration.

## Playback reporting

`report_playback` (on by default) tells the server what is being played: start, progress,
stop. Without it the server does not know trayplay exists as a player, so there are no
play counts, no "last played" dates, nothing in its dashboard, and the Library page's Most
played and Recently played stay empty, because they are built from that same data.

Progress is reported every ten seconds rather than on the player's own tick, and
immediately on pause, resume and seek, since a pause state and a jumped position are the
whole point of a report. Set it to `false` to keep listening off the server's record.

## Placement, and X11 in particular

Under Wayland the popup is a layer-shell surface and trayplay places it itself, per the
`anchor` and `margin` config keys.

Under X11 it cannot: GTK4 removed window positioning. The window manager places the
window, and trayplay's part is a stable `WM_CLASS` of `trayplay` to match a rule on, plus
`_NET_WM_WINDOW_TYPE`.

`x11_window_type` defaults to `utility`, which is what this window is by EWMH's own
definition: a persistent auxiliary window, not a document window. It keeps window manager
and compositor rules written for ordinary windows from applying to it. GTK4 removed the
API for this with no replacement, so trayplay sets the property by hand before the window
is first mapped. Set `normal` for GTK's own behaviour.

The AwesomeWM rule in the README turns off borders, custom corner shapes and shadows, and
that is worth keeping if you run a compositor. A bordered, custom-shaped, shadowed window
is what made picom flicker the popup on every focus change, and none of the three does
anything useful for a window with no titlebar.

`hide_delay_ms` is a trade rather than an improvement, which is why it defaults to 0
(hide on the next main-loop turn). A delay swallows focus that bounces straight back, but
whatever the window manager and compositor do with a window that is about to disappear
(restacking it, fading it, re-redirecting the screen around a fullscreen window) then
happens in plain sight for that long. If the popup looks like it flashes or jumps as it
hides, that is the compositor rather than the delay: with picom, try
`unredir-if-possible = false` or `fade-exclude = [ "class_g = 'trayplay'" ]`.

Auto-hide when unfocused is off by default because under focus-follows-mouse it would
close the popup every time the pointer crossed the window. It is also why the popup resets
to now-playing on hide: a browse in progress is lost when auto-hide fires, which is
intended but unpleasant enough that the two together are opt-in.

## Tray: two backends

X11 gets a native XEmbed icon, Wayland and KDE get StatusNotifierItem. The backend is
picked once at startup from the display in use, and no bridge process is needed either
way.

The X11 icon has no right-click menu, so right click toggles the popup like left click,
and Quit is Ctrl+Q on the popup window everywhere. The tray protocol on X11 has no notion
of a menu (an icon there is just a window), and the library that could add one is GTK3,
which would mean linking a second incompatible GTK into a GTK4 process for six menu
items. The icon also sits in a small opaque box rather than blending into the tray
background, which is a limitation of the docking library rather than a setting.

## Settings versus config

`config.toml` is hand-edited and trayplay never writes to it. Anything the popup's own
settings page changes goes to `settings.toml` under `$XDG_STATE_HOME` instead, and
overrides `config.toml` where both have an opinion. A file with comments in it should not
be rewritten by a switch.

Dark mode starts out matching the desktop, because nothing is stored until you touch it.
Flipping it forces the choice from then on, so a night-light schedule cannot flip the
popup back. Delete the key to follow the desktop again.

## Icons and fonts

Icons live in `data/icons/scalable/actions/`, renamed to `trayplay-*-symbolic` so they
neither shadow an icon theme's own names nor lose GTK's symbolic recolouring, and are
compiled into a GResource bundle at build time. Each set keeps its own `LICENSE` and a
`SOURCES.md` mapping every file to its upstream name. To add one: drop the file in, add a
line to `data/icons/trayplay.gresource.xml`, and refer to it by name.

Fonts are a drop-in directory. `.ttf`/`.otf`/`.ttc` files in `data/fonts/` are compiled in
on the next build with the family name read out of the font itself, and are used by the
no-art panel, one per album. Empty is fine and is the default; everything bundled there is
SIL OFL 1.1.

One consequence worth knowing: fontconfig cannot be handed a font from memory, so
trayplay writes the bundled fonts into `$XDG_DATA_HOME/fonts/trayplay/` at startup. They
become visible to every application on the machine, as if you had installed them by hand.
See `data/fonts/README.md`.
