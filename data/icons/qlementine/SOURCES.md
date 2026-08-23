# Qlementine icons

From the `qlementine-icons` checkout the operator placed at the repository root,
at commit `e7cf96d` ("Merge pull request #24 from oclero/dev"). MIT licensed;
the upstream text is reproduced verbatim in `LICENSE` here.

The 16px set, not 24px: `24/media` has no repeat glyph at all (only `loop.svg`),
and taking two of the three from one size and one from another would have made
the three states of a single button visibly inconsistent.

Copied unchanged - no edits were needed. They already have the structure GTK's
symbolic recolouring wants (`fill` directly on `<path>`, no wrapping `<g>`, no
decorative sibling path - see `../mynaui/SOURCES.md` for what happens
otherwise), and no `width`/`height` at all, which is fine: `viewBox` alone
renders, unlike the meaningless `1em` that had to be fixed on the MynaUI one.

| Upstream | Bundled as |
|---|---|
| `16/action/forward.svg` | `trayplay-repeat-off-symbolic` |
| `16/media/repeat.svg` | `trayplay-repeat-all-symbolic` |
| `16/media/repeat-one.svg` | `trayplay-repeat-one-symbolic` |

The three states of the repeat button (`ui/nowplaying.rs`), operator's mapping:
a plain forward arrow means "no repeat", so the button always shows what is
happening now rather than what the next click would do.
