# Fonts

Drop `.ttf`, `.otf` or `.ttc` files in here and they are compiled into the binary
on the next build. Nothing else to edit: `build.rs` scans this directory, reads
each font's family name out of its `name` table, and generates the list the
no-art panel picks from (`src/fonts.rs`, `src/ui/artwork.rs`).

**Only this directory itself is scanned, not subdirectories.** Unpacking a font
archive here leaves its `ttf/`, `otf/`, `webfont/` and friends untouched and
unused, so the files that are meant to ship have to sit at the top level. That is
deliberate rather than a limitation: a downloaded archive holds a dozen weights
and web formats, and all of them would otherwise be embedded in the binary.

Empty is fine. With no fonts here the panel uses the default sans family, which
is what it did before this directory existed.

## Licence

**Everything bundled here is under the SIL Open Font License** (OFL 1.1). That is
the operator's decision for this directory, not a limitation of the loader - keep
it that way rather than mixing licences into one directory, so the terms covering
the fonts inside the binary are a single known answer.

What the OFL requires of us, since these bytes are redistributed inside
`trayplay`:

- The licence text must travel with the fonts. One shared `OFL.txt` covers this
  directory, since every font in it is under the same licence - a per-font copy
  would be the same bytes eight times.
- The **copyright notice** is per font, though, and the licence text alone does
  not carry it. The table below is where those live: one line per file, with the
  foundry's copyright as it appears in the font's own metadata.
- The reserved font name, if the font declares one, must not be used for a
  modified version. Nothing here modifies fonts - they are embedded verbatim - so
  this only matters if that ever changes.
- The fonts must not be sold on their own. Bundling them in an application is
  explicitly allowed.

## Rules for what goes in

- Add a line to the table below saying where it came from, so provenance is not
  guesswork later. Same convention as `../icons/*/SOURCES.md`.
- **Bold or heavy weights read best here.** The panel draws the album name at low
  opacity behind a scrim; a light weight disappears into the gradient.
- One file per family is enough. The panel asks for a family by name and lets
  Pango pick, and it always asks for bold - extra weights are dead bytes in the
  binary.
- Nothing enforces the size of what lands in here, and every byte is embedded, so
  a dozen families is a decision, not an accident.

## What is here

All OFL 1.1 under the shared `OFL.txt`. Families are as `build.rs` read them out
of each font, which is the name Pango resolves - not necessarily the file name.

| File | Family | Copyright / source |
|---|---|---|
| `Asap-Regular.otf` | `Asap` | _to fill in_ |
| `Cafe24PROUP.otf` | `Cafe24 PRO UP` | _to fill in_ |
| `Chicoree Em. Bold.otf` | `Chicoree Em.` | _to fill in_ |
| `ChillSide.otf` | `ChillSide` | _to fill in_ |
| `Pixelbasel.ttf` | `Pixelbasel` | _to fill in_ |
| `Shehroz.ttf` | `Shehroz` | _to fill in_ |
| `Troubleside.ttf` | `TroubleSide` | _to fill in_ |
| `westernic.otf` | `Westernic` | _to fill in_ |

Eight families, about 1.1 MB, all of it embedded in the binary.

## How they reach Pango

Pango finds fonts through fontconfig, which only looks in the directories it is
configured with - it cannot be handed bytes from memory. So on startup the
bundled fonts are written to `$XDG_DATA_HOME/fonts/trayplay/` (a standard user
font directory) if they are not already there, before GTK initialises. Files are
only rewritten when the size differs, so this is a no-op on every launch after
the first.

The visible consequence, and the reason it is documented rather than buried:
**these fonts become available to every other application too**, exactly as if
they had been installed by hand. Deleting that directory is harmless - it is
recreated on the next launch.
