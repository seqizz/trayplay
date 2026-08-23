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

## The second functional change: surviving a tray restart (patch2)

Upstream docks **once**, in `TrayIconImpl::new`: it reads the owner of
`_NET_SYSTEM_TRAY_S0`, sends one `SYSTEM_TRAY_REQUEST_DOCK`, and never considers docking
again. It also never selects for events on the root window, so it cannot hear a tray
announcing itself, and it ignores `ReparentNotify` on its own icon window.

That breaks the moment the tray goes away and comes back. AwesomeWM rebuilds its wibars
when the screen layout changes - plug in a monitor, or move the systray to another screen -
and every embedded icon is **reparented back to the root window**. A mapped
`InputOutput` window sitting on the root is just an ordinary client, so the window manager
picks it up and decorates it: the icon becomes a stray 24x24 window with `WM_STATE: Normal`,
`_NET_WM_DESKTOP` and frame extents, still carrying `_XEMBED_INFO` from its former life,
while the tray itself shows nothing. Reported against trayplay exactly that way.

Three additions, all in `linux.rs` and all marked `PATCH (trayplay)`:

1. `new` selects `StructureNotify` on the **root** window. ICCCM has a manager broadcast
   its `MANAGER` ClientMessage there, and this is the only way to hear it. Per-client event
   masks, so the window manager's own selection is untouched.
2. The event loop handles three more events:
   - `ReparentNotify` to the root: **unmap immediately** (so the WM never manages it) and
     start trying to dock again.
   - `ReparentNotify` to anything else: embedded again, stop trying.
   - `ClientMessage` of type `MANAGER` for `_NET_SYSTEM_TRAY_S0`: a tray started or
     restarted, so try to dock.
3. `try_dock` re-reads the selection owner every time rather than reusing the one from
   construction - a restarted tray owns a *different* window, and a request sent to the old
   one goes nowhere - and re-asserts `_XEMBED_INFO` before asking.

The retry loop matters as much as the trigger. One request at the moment of eviction is
not enough: the bar being rebuilt usually has no systray widget yet, the request lands
nowhere, and no further event ever arrives to prompt another attempt. So while undocked,
`try_dock` runs about once a second from the poll loop's idle branch.

## Re-syncing with a newer upstream version

Re-apply all three pieces to the new source: the `Scroll` variant in `tray.rs` (including
its `id()` match arm), the `handle_event` changes in `linux.rs` (search for
`_ => MouseButton::Left`), and the re-docking work (`try_dock`, the root event mask, the
`ReparentNotify`/`MANAGER` arms and the idle retry). Re-diff `Cargo.toml.orig` against this
directory's `Cargo.toml` for any new Linux-relevant dependencies. If a future upstream
release grows real scroll support *and* handles re-docking, drop this vendored copy and go
back to the crates.io dependency.
