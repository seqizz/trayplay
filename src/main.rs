mod cli;
mod config;
mod fonts;
mod fuzzy;
mod icons;
mod jellyfin;
mod mpris;
mod player;
mod report;
mod theme;
mod tray;
mod ui;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use anyhow::{Context, Result};
use clap::Parser;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use ksni::TrayMethods;

use crate::config::Config;
use crate::jellyfin::TokenStore;
use crate::tray::{sni, xembed, TrayBackend, UiRequest};
use crate::ui::Popup;

const APP_ID: &str = "dev.trayplay.Trayplay";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "trayplay=info".into()),
        )
        .init();

    let args = cli::Cli::parse();
    let cfg = Config::load()?;

    // Every subcommand needs async too, so the runtime is built before the split.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("trayplay-rt")
        .build()
        .context("building tokio runtime")?;

    if let Some(cmd) = args.command {
        return cli::run(cmd, &cfg, &rt);
    }

    // Before anything can initialise GTK: fontconfig reads its directories when
    // it builds its config, so a font written after that point is invisible until
    // the next launch. See `fonts` for why this has to go through the filesystem
    // at all.
    if let Err(err) = fonts::install() {
        tracing::warn!(%err, "cannot install bundled fonts");
    }

    // WM_CLASS is derived from prgname; pin it so window manager rules that
    // match on "trayplay" keep working regardless of how the binary is invoked.
    glib::set_prgname(Some("trayplay"));
    glib::set_application_name("trayplay");

    let app = adw::Application::builder().application_id(APP_ID).build();

    // GApplication is unique per application_id by default. Registering here,
    // before any of the heavy player/tray/mpris setup below, lets a second
    // `trayplay` invocation detect the already-running one and get out
    // immediately - `run_with_args` alone would still return quickly for a
    // remote instance, but only *after* all that setup below had already run
    // pointlessly first (a second player, a second tray icon trying to dock,
    // a second MPRIS name registration), which is what left a second process
    // sitting around instead of exiting.
    app.register(gio::Cancellable::NONE)
        .context("registering GApplication")?;
    if app.is_remote() {
        // Hands off to the running instance - its own `activate` handler
        // (`build()`'s early-return branch) does `window.present()`. Nothing
        // else in this process needs to run.
        app.activate();
        return Ok(());
    }

    // The tray and MPRIS both run off the GTK thread, so they cannot touch it
    // directly. Window requests are funnelled through this channel instead.
    let (tx, rx) = async_channel::unbounded::<UiRequest>();

    // Missing credentials or a broken audio device are not fatal: the tray
    // still comes up, and the popup shows the login form in that case. The
    // session (when one exists) lives in shared state: `build` installs its
    // view at `activate`, and the login form hot-starts a fresh one later.
    let state = ui::AppState::new();
    match start_session(&cfg, rt.handle(), tx.clone()) {
        Ok(Some(session)) => state.set_session(session),
        Ok(None) => tracing::info!("no stored credentials, sign-in form will show in the popup"),
        Err(err) => tracing::error!(%err, "player unavailable"),
    }

    // Which tray backend to use is a display-backend question (XEmbed needs a
    // real X11 surface, SNI doesn't care), and there is no `gdk::Display` this
    // early - GTK hasn't run its own startup yet. Filled in once `build()`
    // knows, and read back after the main loop returns so the right teardown
    // happens (SNI unregisters, XEmbed's window closes) before `rt` drops.
    let tray_backend: Rc<RefCell<Option<TrayBackend>>> = Rc::new(RefCell::new(None));

    let rt_handle = rt.handle().clone();
    let tray_backend_for_build = tray_backend.clone();
    let state_for_build = state.clone();
    app.connect_activate(move |app| {
        if let Err(err) = build(
            app,
            &cfg,
            &rx,
            &state_for_build,
            tx.clone(),
            rt_handle.clone(),
            &tray_backend_for_build,
        ) {
            tracing::error!(%err, "startup failed");
            app.quit();
        }
    });

    // No window is visible at startup, so without a hold the app would exit
    // as soon as the main loop finds nothing to do.
    let _hold = app.hold();

    app.run_with_args::<&str>(&[]);

    // Keeping these alive until after the main loop returns: dropping the
    // backend unregisters the tray item / closes the XEmbed window, dropping
    // the runtime kills its worker threads.
    drop(tray_backend.borrow_mut().take());
    drop(rt);
    Ok(())
}

/// Builds the Jellyfin client, track cache and audio sink, and starts the
/// player actor. Returns None when there are no stored credentials.
///
/// The client comes back alongside the handle because MPRIS needs it to build
/// cover art URLs.
#[allow(clippy::type_complexity)]
fn start_player(
    cfg: &Config,
    rt: &tokio::runtime::Handle,
) -> Result<Option<(player::PlayerHandle, Arc<jellyfin::Client>)>> {
    let Some(creds) = jellyfin::FileStore::new()?.load()? else {
        tracing::warn!("no stored credentials, run `trayplay login`");
        return Ok(None);
    };
    tracing::info!(server = %creds.server, user = %creds.username, "credentials loaded");

    let client = Arc::new(jellyfin::Client::authenticated(creds)?);

    // Verify the loaded credentials actually work before starting the player.
    // If the server rejects the token (expired, revoked, or rate-limited),
    // bail out so the UI can show the login form instead of failing at playback.
    rt.block_on(client.validate())
        .context("session token invalid, please sign in again")?;

    let token = client.creds_clone().map(|c| c.token).unwrap_or_default();
    let cache = Arc::new(player::cache::Cache::new(
        config::cache_dir()?,
        // The settings page's value wins; config.toml is the fallback for
        // anyone who set it there before the page existed.
        config::Settings::load()
            .cache_max_mb
            .unwrap_or(cfg.cache_max_mb)
            * 1024
            * 1024,
        client.http(),
        token,
    )?);
    if let Err(err) = cache.prune() {
        tracing::warn!(%err, "initial cache prune failed");
    }

    let sink = Box::new(player::rodio_sink::RodioSink::new()?);

    // Player::spawn calls tokio::spawn, so it needs a runtime in context.
    let _guard = rt.enter();
    // Repeat is remembered across restarts, like the queue it applies to.
    let repeat = config::Settings::load().repeat;
    let handle = player::Player::spawn(
        client.clone(),
        cache,
        sink,
        cfg.random_batch,
        cfg.prefetch_next,
        repeat,
    );
    Ok(Some((handle, client)))
}

/// Builds a full session on top of stored credentials: player, MPRIS, and the
/// playback reporter (when enabled).
///
/// Both the launch path and the login form's hot-start path funnel through
/// here, so a session signed in after startup looks exactly like one loaded
/// from disk. No `Command::Restore` is sent - `Popup::install_session` sends
/// it once every event subscriber, the tray's included, is attached.
fn start_session(
    cfg: &Config,
    rt: &tokio::runtime::Handle,
    tray_ui: async_channel::Sender<UiRequest>,
) -> Result<Option<ui::Session>> {
    let Some((handle, client)) = start_player(cfg, rt)? else {
        tracing::warn!("no stored credentials, run `trayplay login` or use the popup");
        return Ok(None);
    };

    mpris::spawn(handle.clone(), tray_ui, client.clone());
    // Before `Command::Restore`, like every other subscriber - though this one
    // has nothing to do with the restore event, since a restored session is
    // not playing.
    if cfg.report_playback {
        report::spawn(rt, &handle, client.clone());
    }

    Ok(Some(ui::Session {
        player: handle,
        browser: ui::Browser::new(
            rt.clone(),
            client,
            std::time::Duration::from_secs(cfg.library_cache_ttl_secs),
        ),
    }))
}

/// Mirrors player state onto the tray icon and tooltip. Only used for the SNI
/// backend - XEmbed updates itself directly on the GTK thread from the same
/// bridged event stream every other GTK-side listener uses (see
/// `tray::xembed::spawn`), since it never leaves that thread to begin with.
fn spawn_tray_updater(
    rt: &tokio::runtime::Handle,
    tray: ksni::Handle<sni::Tray>,
    player: &player::PlayerHandle,
) {
    let mut events = player.subscribe();
    let player = player.clone();
    rt.spawn(async move {
        // Same reason as in `xembed::spawn_updater`: the restored track's
        // TrackChanged predates this subscription, so the label is seeded by
        // asking the player rather than waiting for the next track change.
        if let Some(snapshot) = player.snapshot().await {
            if let Some(item) = snapshot.items.get(snapshot.cursor) {
                let label = format!("{} - {}", item.display_artist(), item.name);
                tray.update(move |t: &mut sni::Tray| t.now_playing = Some(label))
                    .await;
            }
        }

        loop {
            match events.recv().await {
                Ok(player::Event::TrackChanged(item)) => {
                    let label = item.map(|i| format!("{} - {}", i.display_artist(), i.name));
                    tray.update(move |t: &mut sni::Tray| t.now_playing = label).await;
                }
                Ok(player::Event::StateChanged(state)) => {
                    tray.update(move |t: &mut sni::Tray| t.state = state).await;
                }
                // Position fires four times a second; redrawing the tray for it
                // would be pointless traffic on the bus.
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(skipped = n, "tray updater fell behind");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Attaches the tray's player mirror once both halves exist: a session and a
/// registered tray backend. One half or the other is usually async (the
/// session may hot-start after startup, the SNI backend registers on the
/// runtime), so the caller that has its half retries on the slot.
///
/// The session is handed to the backends through the shared `tray_player`
/// cell as well, so click/scroll handlers forward commands to the live
/// session's player even when it was created after the backend.
fn maybe_start_tray_updater(
    rt: &tokio::runtime::Handle,
    state: &ui::AppState,
    tray_backend: &Rc<RefCell<Option<TrayBackend>>>,
    tray_player: &std::sync::Mutex<Option<player::PlayerHandle>>,
) {
    if state.claim_tray_updater() {
        return;
    }
    let Some(session) = state.session() else {
        state.release_tray_updater();
        return;
    };
    *tray_player.lock().unwrap() = Some(session.player.clone());
    match tray_backend.borrow().as_ref() {
        Some(TrayBackend::Sni(handle)) => {
            spawn_tray_updater(rt, handle.clone(), &session.player);
        }
        Some(TrayBackend::XEmbed(handle)) => handle.start_updater(rt, session.player.clone()),
        None => {
            // The SNI backend registers asynchronously; its callback calls
            // this again once the handle exists.
            state.release_tray_updater();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build(
    app: &adw::Application,
    cfg: &Config,
    rx: &async_channel::Receiver<UiRequest>,
    state: &ui::AppState,
    tray_ui: async_channel::Sender<UiRequest>,
    rt: tokio::runtime::Handle,
    tray_backend: &Rc<RefCell<Option<TrayBackend>>>,
) -> Result<()> {
    // `GApplication` is unique per `application_id` by default (no
    // `ApplicationFlags::NON_UNIQUE` is set), so launching a second `trayplay`
    // hands off to the running one over D-Bus instead of starting its own
    // main loop - that second process's `activate` fires *here*, on the
    // already-running instance, which is what detects "already running" for
    // free. `windows()` is non-empty in exactly that case (the window still
    // exists even if hidden - `set_hide_on_close(true)` never destroys it),
    // so this is also where a relaunch has to actually show it, or running
    // the binary again would look like doing nothing.
    //
    // Routed through `UiRequest::TogglePopup` rather than presenting the window
    // directly, so a relaunch means exactly what a tray click means: show when
    // hidden, raise when it is up but unfocused, hide when it has focus. A bare
    // `present()` could only ever show, which made re-running the binary a
    // one-way door and forced whatever launched it (a wibar button, a
    // keybinding) to implement hiding itself.
    if !app.windows().is_empty() {
        if let Err(err) = tray_ui.try_send(UiRequest::TogglePopup) {
            tracing::warn!(%err, "cannot forward a relaunch to the popup");
        }
        return Ok(());
    }

    let display = gdk::Display::default().context("no display")?;
    // Before any widget asks for an icon by name.
    icons::install(&display)?;
    theme::install(&display)?;

    // The player the tray backends forward commands to. Filled at startup
    // when a session was loaded; the login hot-start path fills it later. A
    // mutex because both backends read it from their own threads.
    let tray_player = std::sync::Arc::new(std::sync::Mutex::new(
        state.session().map(|s| s.player),
    ));

    // XEmbed needs a real X11 surface to dock, which is the same reason the
    // popup itself waits for `display` before deciding layer-shell vs
    // `awful.rules` placement - this is the earliest point either backend can
    // be built. Everywhere else in the world (Wayland/somewm, KDE, GNOME)
    // gets SNI, same as always.
    if display.backend().is_wayland() {
        // assume_sni_available means a missing watcher is reported to
        // sni::Tray::watcher_offline rather than failing startup outright.
        let updater_rt = rt.clone();
        let updater_state = state.clone();
        let updater_backend = tray_backend.clone();
        let updater_player = tray_player.clone();
        let sni_ui = tray_ui.clone();
        // A separate clone for the coroutine: the async block captures by
        // move, and the result callback below needs the Arc as well.
        let tray_for_spawn = updater_player.clone();
        ui::on_runtime(
            &rt,
            async move {
                sni::Tray::new(sni_ui, tray_for_spawn)
                    .assume_sni_available(true)
                    .spawn()
                    .await
            },
            move |result| match result {
                Ok(handle) => {
                    *updater_backend.borrow_mut() = Some(TrayBackend::Sni(handle));
                    // With a session already loaded (startup credentials) this
                    // attaches the tray updater now; otherwise the login
                    // form's hot-start path does it once a player exists.
                    maybe_start_tray_updater(
                        &updater_rt,
                        &updater_state,
                        &updater_backend,
                        &updater_player,
                    );
                }
                Err(err) => tracing::error!(?err, "registering StatusNotifierItem failed"),
            },
        );
    } else {
        // Not SNI: no host to describe the icon/menu to, so this docks a raw
        // X11 window by hand instead (see tray::xembed's module docs for why
        // a gtk::Window can't be used). No right-click menu, and the icon
        // sits on a solid black square - `tray` 0.1.2 limitations, not
        // something fixable from here (see "Tray: two backends, one per
        // display" in CLAUDE.md). Scroll works (next/previous), unlike
        // upstream `tray` - see vendor/tray/PATCH.md.
        tracing::info!("no SNI host on X11, docking a plain XEmbed tray icon instead (no menu, no transparency)");
        match xembed::spawn(&display, tray_ui.clone(), tray_player.clone()) {
            Ok(handle) => {
                *tray_backend.borrow_mut() = Some(TrayBackend::XEmbed(handle));
                maybe_start_tray_updater(&rt, state, tray_backend, &tray_player);
            }
            Err(err) => tracing::error!(?err, "setting up XEmbed tray icon failed"),
        }
    }
    // StyleManager is only meaningful once adw is initialised, which activate
    // guarantees. Applied before the first window so nothing is built light and
    // then restyled.
    ui::settings::apply(&config::Settings::load());

    let popup = Popup::new(
        app,
        cfg,
        &display,
        state,
        rt.clone(),
        tray_ui,
        tray_backend,
        &tray_player,
    );

    // Start the session's view when one was loaded at startup. No-op when
    // there is none - the login form hot-starts instead. Same code path as the
    // login flow, so installing later cannot differ from installing now.
    popup.install_session(&rt);

    let app = app.clone();
    let rx = rx.clone();
    glib::spawn_future_local(async move {
        while let Ok(req) = rx.recv().await {
            match req {
                UiRequest::TogglePopup => popup.toggle(),
                UiRequest::ShowPopup => popup.show(),
                UiRequest::HidePopup => popup.hide(),
                UiRequest::Quit => {
                    app.quit();
                    break;
                }
            }
        }
    });

    tracing::info!("trayplay started, waiting on tray");
    Ok(())
}
