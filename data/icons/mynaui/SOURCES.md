# MynaUI icons

Single icon, supplied directly by the operator (downloaded from mynaui.com /
its Iconify listing) rather than pulled from a checkout, so there is no
upstream ref to pin. MIT licensed, no attribution required (operator-confirmed).

Content changes from the raw download:

- `width`/`height` changed from `1em` to match the `viewBox` - `1em` is
  meaningless outside a CSS/font-size context (this was exported for inline
  web/font-icon use) and rendered as nothing standalone.
- `fill="currentColor"` moved from a wrapping `<g>` down onto `<svg>` directly
  (fill is inherited either way, so this changes nothing for a normal SVG
  renderer), and a decorative invisible `<path d="M0 0h24v24H0z" fill="none"/>`
  sibling removed. Neither mattered to a plain rasterizer (confirmed by
  rendering the original standalone with ImageMagick - it displayed fine) but
  both silently defeated GTK's symbolic-icon recolour pass: every rendered
  pixel came back fully opaque, i.e. a flat same-colour block with the note
  detail invisible, not because of anything about the icon's shape. Every
  other bundled icon that already worked has `fill` set directly on
  `<svg>`/`<path>` with no wrapping `<g>` and no decorative sibling path -
  flattening this one to match fixed it. (`fill-rule="evenodd"` was also
  tried, on a hunch about the note glyph being a compound-path cutout - it
  turned out to make no difference either way once the structure above was
  fixed, so it's not in the current file.)

| Upstream | Bundled as |
|---|---|
| `mynaui--music-square-solid.svg` | `trayplay-music-symbolic` |

Used as the XEmbed tray icon's "nothing playing" state (`tray::xembed::icon_name_for_state`).
