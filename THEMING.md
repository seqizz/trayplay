# Theming trayplay

CSS only. `data/default.css` is compiled into the binary and loaded at
`APPLICATION` priority; your `$XDG_CONFIG_HOME/trayplay/theme.css` is loaded at
`USER` priority, so you only need to state what you want to change. The file is
watched and reapplied without restarting.

Selectors are a stability contract — renames are treated as breaking changes.

## Selectors

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
| `#trayplay-loading` | thin bar overlaid on the bottom edge of the seek slider while a track is being loaded (download, or a server-side transcode). Style through its GTK nodes: `trough` and `progress`. Hidden entirely for a load shorter than the grace period, and it *pulses* rather than filling when the server sends no `Content-Length` — the timings are constants in `src/ui/nowplaying.rs` and are not themable |
| `#trayplay-seek value` | elapsed time, drawn above the handle. Always present so its height never shifts the layout; `#trayplay-seek.showing-value` is what makes it visible, on hover. There are no permanent elapsed/total labels |
| `#trayplay-transport` | centre box holding the transport buttons, with repeat at its left end and shuffle + instant mix at its right |
| `.trayplay-glyph` | transport controls, drawn as bare glyphs — the rule that strips the button shape in every state. Adwaita's `.flat` is not enough, it still paints a hover background |
| `#trayplay-prev` / `#trayplay-play` / `#trayplay-next` | transport buttons. Glyph *sizes* are not themable: `GLYPH_SIZE` / `PLAY_GLYPH_SIZE` in `src/ui/nowplaying.rs` are set with `set_pixel_size`, which CSS cannot override |
| `#trayplay-random` | shuffle, at the right of the transport row |
| `#trayplay-mix` | instant mix, beside shuffle. Insensitive while nothing is playing, since the mix is seeded from the current track. Drawn at `MIX_GLYPH_SIZE` rather than `GLYPH_SIZE`, correcting for artwork that fills more of its canvas than its neighbours — not themable either, same as the rest |
| `#trayplay-repeat` | repeat, at the left of the transport row. `#trayplay-repeat.repeat-active` is set while repeat is on (either kind), since the three glyphs are close at this size |
| `#trayplay-actions` | bottom action row |
| `#trayplay-settings` | settings button, bottom left, square |
| `#trayplay-library` | library button |
| `#trayplay-queue` | queue button |
| `#trayplay-settings-page` | settings page body |
| `#trayplay-dark-switch` / `#trayplay-motion-switch` / `#trayplay-autohide-switch` | its three switches |
| `#trayplay-cache-row` | the cache-limit spin row |
| `#trayplay-list` | list box on artist/album/track pages |
| `#trayplay-section` | section heading ("Albums", "Other tracks") |
| `#trayplay-filter` / `#trayplay-filter-entry` | type-to-filter bar on list pages |
| `.trayplay-row` | a row in those lists |
| `#trayplay-page-action` | header button on a list page ("Play") |
| `#trayplay-loading` | spinner on a list page whose query has not answered yet |
| `#trayplay-status` | signed-out text |
| `#trayplay-toast` | toast overlay wrapping the navigation stack; the banner itself is libadwaita's `.toast` node inside it |
| `#trayplay-row-menu` | a row's right-click menu; its entries are `button` nodes inside it |

A failed library query is a toast rather than a page of its own, so there is no
error label to style — restyle `#trayplay-toast .toast` instead.

Debugging: `GTK_DEBUG=interactive cargo run` opens the GTK inspector, which shows
the live CSS node tree. Use it when a container stays opaque — it names the exact
node.

## Transparency and blur

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
same alpha of white is far heavier than of a dark colour — it flattens the art into
a sheet, where dark reads as depth. Those overrides hang off `#trayplay-popup.light`,
and the text shadows flip from dark haloes to light ones under it.

Tune `alpha(@window_bg_color, 0.55)` for the transparency/legibility trade-off.
With no compositor running, or to go back to a solid window:

```css
/* ~/.config/trayplay/theme.css */
#trayplay-popup {
  background-color: @window_bg_color;
}
```

## Two things not to do

**Do not give the transport buttons a background.** `theme.css` is `USER` priority
and outranks everything in `default.css`, so a
`#trayplay-transport button { background-color: ... }` rule puts the button shape
back behind glyphs that are drawn without one — and no rule in the built-in theme
can win against it.

**Do not set `opacity` on `#trayplay-tags` or a `filter` on `#trayplay-art`.** Both
are animated or drawn in code, and your value will either be overwritten on the
next track change or applied to parts of the art that are meant to stay sharp.
