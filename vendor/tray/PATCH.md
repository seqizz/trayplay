# Vendored `tray` 0.1.2, patched

Source: <https://github.com/nobane/tray-rs> / crates.io `tray = "0.1.2"` (MIT). Copied
verbatim from the crates.io package (`lib.rs`, `error.rs`, `icon.rs`, `tray.rs`,
`linux.rs`), `windows.rs`/`macos.rs` dropped (unused - this project is Linux-only, and
`lib.rs`'s `mod windows;`/`mod macos;` are already `#[cfg(target_os = ...)]`-gated, so
omitting the files is safe). `Cargo.toml` trimmed to the Linux-relevant dependency subset
only (drops `windows-sys`, the `objc2*` family, `libxdo`, `common-controls-v6`, none of
which anything here uses).

## The functional change

X11 has no dedicated scroll-wheel event - a wheel notch arrives as an ordinary
`ButtonPress`+`ButtonRelease` pair with `detail` 4 (up) or 5 (down), same as any other
button. Upstream's `src/linux.rs::handle_event` had `_ => MouseButton::Left` as its
fallback for unrecognized button numbers, which bucketed *both* into an ordinary left
click - so scrolling over the tray icon fired a spurious click, toggling trayplay's popup
open then immediately shut.

`TrayIconEvent` (`src/tray.rs`) gained a new variant to carry this properly instead of
dropping or misreporting it:

```rust
Scroll { id: TrayIconId, delta: i32 },  // -1 = one notch up, +1 = one notch down
```

`handle_event` emits one `Scroll` per notch on the `ButtonPress` (detail 4/5) and drops the
matching `ButtonRelease` for the same detail, so a single notch does not fire twice.
`TrayIconEvent` is `#[non_exhaustive]` upstream, so this is additive - nothing upstream
already matching on it needed a wildcard arm added, that discipline was already required.

## Re-syncing with a newer upstream version

Re-apply both pieces to the new source: the `Scroll` variant in `tray.rs` (including its
`id()` match arm), and the `handle_event` changes in `linux.rs` (search for
`_ => MouseButton::Left`). Re-diff `Cargo.toml.orig` against this directory's `Cargo.toml`
for any new Linux-relevant dependencies. If a future upstream release adds real scroll
support itself, drop this vendored copy entirely and go back to the crates.io dependency.
