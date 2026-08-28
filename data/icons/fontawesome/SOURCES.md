# Icons taken from Font Awesome

Upstream: <https://fontawesome.com>, **Font Awesome 4** by Dave Gandy, SIL Open
Font License 1.1, see `LICENSE`.

| Upstream | Bundled as |
|---|---|
| `magic` | `trayplay-mix-symbolic` |

Supplied by the operator as a download rather than from a checkout, so there is
no upstream commit to pin.

## Which Font Awesome, and therefore which licence

**Version 4, which is why this is OFL and not CC BY 4.0.** The distinction
matters: Font Awesome 4 shipped its icons only as a font, so the project's "Font:
SIL OFL 1.1" covered the artwork. From version 5 onwards the icons are licensed
CC BY 4.0 instead, with the OFL applying only to the packaged font files.

The file is v4 on two pieces of evidence: a `viewBox` of `0 0 1664 1664`, which is
v4's em geometry rather than v5/v6's 512-based canvas, and the name `magic`, which
v6 renamed to `wand-magic-sparkles`. Iconify's `fa:` prefix is Font Awesome 4 and
it labels that set OFL-1.1, which is presumably where the download came from.
About 80% confident; if it turns out to be v5, the licence is CC BY 4.0 and this
file and `LICENSE` need replacing (the icon itself stays usable either way, and
the attribution above is what CC BY would ask for anyway).

`LICENSE` is the canonical OFL 1.1 text, copied verbatim from `data/fonts/OFL.txt`
which was already in the tree, with that file's fonts-specific opening sentence
replaced by an attribution line. Font Awesome 4 declared no formal
`Copyright (c) <year>, <holder>` notice of its own, so the attribution is an
attribution and not a reproduced notice, and no Reserved Font Name is claimed.

## Content changes from the download

Both were required, and both are the failure modes already documented in
`../mynaui/SOURCES.md` - which is the only reason they were caught before the icon
rendered as a flat block:

- `width`/`height` of `1em` removed. Meaningless outside a CSS/font-size context;
  the `viewBox` alone renders, as the Qlementine files here already show.
- A decorative `<path d="M0 0h1664v1664H0z" fill="none" />` sibling removed. It
  is invisible to an ordinary SVG rasterizer and silently defeats GTK's symbolic
  recolour pass.

`fill="currentColor"` is left where it was, directly on the artwork `<path>`. GTK
does not resolve it (there is no CSS context), so it renders black, which is
exactly what a mask wants - the same as Phosphor's files.

## Why this glyph for instant mix

A wand throwing sparkles. It says "something was generated for you", which is what
the button does, without being a second shuffle glyph beside the first.

It replaced MDI's `mixcloud`, which was **rejected for being a trademark**: Apache
2.0 §6 grants no trademark rights, so redistributing Mixcloud's brand mark as a
UI control was a licence-clean but rights-unclear choice the operator did not want.
Do not reintroduce a brand icon here.
