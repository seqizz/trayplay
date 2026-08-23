# Icons taken from ionicons

Not the only set in use - see `../phosphor/SOURCES.md`. Each set keeps its own
licence notice next to its own mapping.

Upstream: <https://github.com/ionic-team/ionicons> at v8.1.0, MIT, see `LICENSE`.

Files are copied unmodified and only renamed. The `trayplay-` prefix keeps them
from shadowing an icon theme's own names, and the `-symbolic` suffix is what
makes GTK recolour them with the current foreground colour - the artwork is
black on transparent, which GTK uses as a mask.

The `ios-`/`md-` prefixes are ionicons v2 and were dropped in v4, so v8 has no
`ios-shuffle-strong` - and the v8 `shuffle` is different artwork, not a rename.
Iconify's `ion:` set is still the v2 icons, which is where those names come from.
The v2 icons used here were taken out of the upstream repository's own history
(`git show v2.0.1:src/<name>.svg`), so they are the same MIT licence and needed
no separate download.

v2's solid artwork also survives being scaled down better than v8's thin strokes:
v8 `search-outline` lost its handle entirely at 16px and rendered as a bare
circle.

| Upstream file | Installed icon name |
|---|---|
| `src/shuffle.svg` **at tag v2.0.1** | `trayplay-shuffle-symbolic` |
| `src/svg/library-outline.svg` | `trayplay-library-symbolic` |
| `src/svg/settings-sharp.svg` | `trayplay-settings-symbolic` |
| `src/svg/play-skip-back.svg` | `trayplay-prev-symbolic` |
| `src/svg/play-outline.svg` | `trayplay-play-symbolic` |
| `src/svg/pause-outline.svg` | `trayplay-pause-symbolic` |
| `src/svg/play-skip-forward.svg` | `trayplay-next-symbolic` |

Mostly the `-outline` (stroked) artwork, which is what `shuffle` is - it has no
filled variant upstream. The two skip buttons are the filled variants on purpose:
at transport size the stroked ones read as arrows rather than as skip.

Upstream also ships a `-sharp` variant of everything, which is the same artwork
with square joins.
