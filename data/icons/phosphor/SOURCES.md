# Icons taken from Phosphor

Upstream: <https://github.com/phosphor-icons/core> at v2.0.8, MIT, see `LICENSE`.

Copied unmodified from `assets/`, only renamed. `assets/` rather than `raw/`:
the raw files are the design sources, the assets are what upstream ships.

| Upstream file | Installed icon name |
|---|---|
| `assets/bold/queue-bold.svg` | `trayplay-queue-symbolic` |

Phosphor draws with filled paths and `fill="currentColor"`. GTK does not resolve
`currentColor` - there is no CSS context - so it renders as black, which is
exactly what a symbolic icon needs: GTK masks the artwork and paints with the
theme foreground. Same as the ionicons files, which hardcode `#000`.

Weights are `thin` / `light` / `regular` / `bold` / `fill` (plus `duotone`). Use
**`bold`**: it is 12px on a 256 canvas, which is the closest match to the
ionicons `-outline` weight (32 on 512). `regular` at 8px visibly thinner than
everything around it, which is what ruled it out.
