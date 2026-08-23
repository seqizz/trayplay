use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use gtk::gdk;
use gtk::glib;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::config::{Anchor, Config, Settings};
use crate::jellyfin::models::{Item, Kind};
use crate::player::{Command, Event, PlayerHandle};

use super::browse::{ListPage, RowAction, Section};
use super::nowplaying::{Navigate, NowPlaying};
use super::Session;

/// How long a playback error stays on screen. Long enough to read a sentence,
/// short enough that it is gone by the next interaction.
const TOAST_TIMEOUT_SECS: u32 = 5;

/// How often auto-hide reconsiders a blur it skipped because a row menu was
/// open. Only runs while such a menu is up, and stops at the first decision.
const MENU_HIDE_RECHECK: std::time::Duration = std::time::Duration::from_millis(250);

/// How long after losing focus a tray click still counts as "the user was using
/// this", and therefore closes the popup rather than raising it.
///
/// Exists because of focus-follows-mouse: there the popup is unfocused the
/// instant the pointer leaves it for the tray, so without a grace period the tray
/// click could never close it again. Long enough to cover moving the pointer to
/// the tray and clicking; short enough that a popup left sitting unfocused while
/// you work elsewhere is treated as something to raise.
const TRAY_HIDE_GRACE: std::time::Duration = std::time::Duration::from_millis(1200);

/// The popup window. Under Wayland it is a layer-shell surface anchored to a
/// monitor corner. Under X11 it is an ordinary toplevel with a stable WM_CLASS,
/// because GTK4 removed window positioning API on X11 - placement there is the
/// window manager's job (see README for the AwesomeWM rule).
pub struct Popup {
    window: adw::ApplicationWindow,
    /// When the window last lost focus, so a tray click can tell "the user was
    /// just using this" from "this has been sitting there unfocused". See
    /// `toggle`.
    unfocused_since: Rc<Cell<Option<Instant>>>,
}

impl Popup {
    pub fn new(
        app: &adw::Application,
        cfg: &Config,
        display: &gdk::Display,
        session: Option<Session>,
        events: Option<async_channel::Receiver<Event>>,
    ) -> Self {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .default_width(cfg.width)
            .default_height(cfg.height)
            .resizable(false)
            .title("trayplay")
            .build();

        // The stored choice wins over config.toml, which is where this used to
        // live; shared with the settings switch so flipping it applies at once.
        let hide_on_blur = Rc::new(Cell::new(
            Settings::load()
                .hide_on_focus_loss
                .unwrap_or(cfg.hide_on_focus_loss),
        ));

        // Stable selector root for themes, carrying a `light`/`dark` class so a
        // stylesheet can tell the two palettes apart.
        window.set_widget_name("trayplay-popup");
        super::settings::track_scheme(&window);
        // Closing must not tear down the app; the tray owns the lifetime.
        window.set_hide_on_close(true);

        // Independent of `wire_shortcuts` below: Ctrl+Q must quit even
        // signed out (no session, no now-playing view to attach shortcuts
        // to) or from a pushed page, not just the root. Wired before
        // `wire_shortcuts` adds its own controller to the same window, so it
        // sees (and stops) Ctrl+Q first - plain `q` on the root page still
        // reaches `NowPlaying::handle_key` and opens the queue as usual,
        // since that has no modifier check of its own.
        let quit_player = session.as_ref().map(|s| s.player.clone());
        wire_quit(&window, app, quit_player);

        match session {
            Some(session) => {
                let nav = adw::NavigationView::new();
                nav.set_widget_name("trayplay-nav");

                let now_playing = build_now_playing(&nav, session, hide_on_blur.clone());
                nav.add(&now_playing.0);
                wire_shortcuts(&window, &nav, &now_playing.1);

                // Toasts live outside the navigation stack so an error raised
                // from a browse page is still visible after it pops back to
                // now-playing.
                let toaster = Toaster::new(&nav);
                window.set_content(Some(&toaster.overlay));

                // Opening the popup should always land on now-playing, never
                // halfway down someone else's discography. Hooked to the hide
                // signal rather than to each caller, so auto-hide, Escape, the
                // tray toggle and MPRIS all reset alike.
                let nav_weak = nav.downgrade();
                window.connect_hide(move |_| {
                    if let Some(nav) = nav_weak.upgrade() {
                        nav.pop_to_tag("now-playing");
                    }
                });

                if let Some(events) = events {
                    spawn_event_loop(now_playing.1, toaster, events);
                }
            }
            None => window.set_content(Some(&signed_out_page())),
        }

        // Layer-shell setup must happen before the surface is realized.
        // gtk4_layer_shell::is_supported() asserts GDK_IS_WAYLAND_DISPLAY
        // internally and logs a CRITICAL on X11, so check the backend first.
        if display.backend().is_wayland() && gtk4_layer_shell::is_supported() {
            window.init_layer_shell();
            window.set_layer(Layer::Overlay);
            window.set_keyboard_mode(KeyboardMode::OnDemand);
            apply_layer_anchor(&window, cfg);
        } else {
            // Bound outside the macro: inside it, `display` would resolve to
            // tracing::field::display rather than the local.
            let backend = display.backend();
            tracing::info!(
                ?backend,
                "layer-shell not available, falling back to basic window manager placement"
            );
        }

        let unfocused_since = Rc::new(Cell::new(None));
        Self::wire_dismiss(&window, hide_on_blur, unfocused_since.clone());

        Self {
            window,
            unfocused_since,
        }
    }

    fn wire_dismiss(
        window: &adw::ApplicationWindow,
        hide_on_blur: Rc<Cell<bool>>,
        unfocused_since: Rc<Cell<Option<Instant>>>,
    ) {
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(|controller, key, _code, _mods| {
            if key == gdk::Key::Escape {
                if let Some(win) = controller.widget().and_downcast::<gtk::Window>() {
                    win.set_visible(false);
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        window.add_controller(keys);

        // Connected unconditionally and gated on the flag, so the settings switch
        // takes effect immediately instead of at the next start.
        window.connect_is_active_notify(move |win| {
            // Recorded whatever auto-hide is set to: `toggle` needs it either way,
            // so it cannot live behind the gate below.
            unfocused_since.set(if win.is_active() {
                None
            } else {
                Some(Instant::now())
            });

            if !hide_on_blur.get() || win.is_active() {
                return;
            }
            // A row menu is a surface of its own, so opening one deactivates this
            // window. Hiding then would take the popup away under the menu the
            // user just opened.
            if super::menu_is_open() {
                // Deferred rather than dropped: if the menu is dismissed by a
                // click on another window, focus never returns here and no
                // further is-active notify arrives to reconsider this.
                let win_weak = win.downgrade();
                let hide_on_blur = hide_on_blur.clone();
                glib::timeout_add_local(MENU_HIDE_RECHECK, move || {
                    let Some(win) = win_weak.upgrade() else {
                        return glib::ControlFlow::Break;
                    };
                    if super::menu_is_open() {
                        return glib::ControlFlow::Continue;
                    }
                    if hide_on_blur.get() && !win.is_active() {
                        win.set_visible(false);
                    }
                    glib::ControlFlow::Break
                });
                return;
            }
            win.set_visible(false);
        });
    }

    /// What a tray click does.
    ///
    /// Not a plain visible/hidden toggle: a popup that is on screen but does not
    /// have focus - behind another window, on a tag that was switched away from
    /// and back - should be *raised* by a tray click, not closed. Closing it was
    /// the old behaviour and reads as the click having done nothing except lose
    /// what was on screen.
    ///
    /// The grace period is what keeps this usable under focus-follows-mouse,
    /// where the popup is unfocused the moment the pointer reaches the tray: a
    /// blur that recent still counts as focused, so the click closes. See
    /// `TRAY_HIDE_GRACE`.
    pub fn toggle(&self) {
        if !self.window.is_visible() {
            self.show();
            return;
        }

        let just_left = self
            .unfocused_since
            .get()
            .is_some_and(|at| at.elapsed() < TRAY_HIDE_GRACE);

        if self.window.is_active() || just_left {
            self.window.set_visible(false);
        } else {
            // present() on a visible window raises it and takes focus, which is
            // the whole point here.
            self.show();
        }
    }

    pub fn show(&self) {
        self.window.present();
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
    }
}

/// Routes single-key shortcuts to the root view.
///
/// On the window, because key events only reach widgets in the focus chain and
/// the root view is not in it unless something inside it happens to be focused.
/// Capture phase so Space is play/pause rather than "activate the focused
/// button", and gated on the visible page so typing into a filter box on a
/// pushed page is just typing.
/// Ctrl+Q quits the app - the tray menu's "Quit" already does this
/// (`Command::Shutdown` then `app.quit()`), but that's inaccessible on X11
/// now that XEmbed has no right-click menu (see `tray::xembed`'s module
/// docs), so the window needs its own way out. Capture phase, and wired on
/// the window itself rather than through `wire_shortcuts`, so it fires from
/// any page and even when there is no session to attach a now-playing view
/// to.
fn wire_quit(window: &adw::ApplicationWindow, app: &adw::Application, player: Option<PlayerHandle>) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);

    let app = app.clone();
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        // `.contains`, not `==`: a Caps Lock state bit riding along in
        // `modifiers` would otherwise silently break this while it's on.
        if key == gdk::Key::q && modifiers.contains(gdk::ModifierType::CONTROL_MASK) {
            if let Some(player) = &player {
                player.send(Command::Shutdown);
            }
            app.quit();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });

    window.add_controller(keys);
}

fn wire_shortcuts(
    window: &adw::ApplicationWindow,
    nav: &adw::NavigationView,
    now_playing: &Rc<NowPlaying>,
) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);

    let nav = nav.downgrade();
    let now_playing = Rc::downgrade(now_playing);
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        let (Some(nav), Some(now_playing)) = (nav.upgrade(), now_playing.upgrade()) else {
            return glib::Propagation::Proceed;
        };
        let on_root = nav
            .visible_page()
            .and_then(|page| page.tag())
            .is_some_and(|tag| tag == "now-playing");
        if !on_root {
            return glib::Propagation::Proceed;
        }
        now_playing.handle_key(key, modifiers)
    });

    window.add_controller(keys);
}

/// Builds the root navigation page and returns it with the view that needs
/// player events.
fn build_now_playing(
    nav: &adw::NavigationView,
    session: Session,
    hide_on_blur: Rc<Cell<bool>>,
) -> (adw::NavigationPage, Rc<NowPlaying>) {
    let nav_weak = nav.downgrade();
    let session_nav = session.clone();
    let session_queue = session.clone();

    // The navigation closure is built before the view it navigates *from* exists,
    // so the settings page cannot capture it directly. It is filled in below and
    // only read when the page is actually opened, which is always later.
    let view: Rc<RefCell<Option<std::rc::Weak<NowPlaying>>>> = Rc::new(RefCell::new(None));
    let view_for_settings = view.clone();

    let now_playing = NowPlaying::new(session, move |target| {
        let Some(nav) = nav_weak.upgrade() else {
            return;
        };
        match target {
            Navigate::Library => push_artists(&nav, &session_nav),
            Navigate::Queue => super::queue::push(&nav, &session_queue),
            Navigate::Settings => {
                let view = view_for_settings.borrow().clone();
                let on_reduce_motion: Rc<dyn Fn(bool)> = Rc::new(move |reduce| {
                    if let Some(view) = view.as_ref().and_then(std::rc::Weak::upgrade) {
                        view.set_reduce_motion(reduce);
                    }
                });
                nav.push(&super::settings::page(
                    hide_on_blur.clone(),
                    on_reduce_motion,
                ));
            }
            Navigate::Artist { id, name } => push_albums(&nav, &session_nav, &id, &name),
            Navigate::Album { id, name } => push_tracks(&nav, &session_nav, &id, &name),
        }
    });

    // Weak, so the closure above cannot keep the view alive through the Rc it
    // itself lives in.
    *view.borrow_mut() = Some(Rc::downgrade(&now_playing));

    // No header bar on the root page: a tray popup has no use for a titlebar or
    // window controls, and search lives in the action row instead.
    let page = adw::NavigationPage::new(&now_playing.root, "trayplay");
    page.set_tag(Some("now-playing"));
    (page, now_playing)
}

/// Server results per Library search, capped well past anything a scroll
/// would realistically reach.
const LIBRARY_SEARCH_LIMIT: u32 = 60;

/// Library search is debounced like a seek: a network round trip per
/// keystroke would both hammer the server and race itself, so this waits for
/// typing to pause before asking.
const LIBRARY_SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);

fn push_artists(nav: &adw::NavigationView, session: &Session) {
    let nav_weak = nav.downgrade();
    let session = session.clone();
    session.browser.clone().artists(move |result| {
        let Some(nav) = nav_weak.upgrade() else { return };
        let artists = match result {
            Ok(items) => items,
            Err(err) => return toast_error("Library", &err),
        };

        // Bumped on every keystroke so a slow search that lands after a newer
        // one (or after the box was cleared) is dropped instead of clobbering
        // fresher content.
        let generation = Rc::new(Cell::new(0u64));

        let nav_for_query = nav.downgrade();
        let session_query = session.clone();
        let artists_for_query = artists.clone();

        let page = ListPage::build_dynamic("Library", "Artists", vec![artist_section(artists, &nav, &session)], {
            move |text, render| {
                let gen = generation.get().wrapping_add(1);
                generation.set(gen);

                if text.is_empty() {
                    let Some(nav) = nav_for_query.upgrade() else { return };
                    render(vec![artist_section(artists_for_query.clone(), &nav, &session_query)]);
                    return;
                }

                let generation = generation.clone();
                let browser = session_query.browser.clone();
                let session_search = session_query.clone();
                let nav_weak = nav_for_query.clone();
                glib::timeout_add_local_once(LIBRARY_SEARCH_DEBOUNCE, move || {
                    if generation.get() != gen {
                        return;
                    }
                    browser.search(&text, LIBRARY_SEARCH_LIMIT, move |result| {
                        if generation.get() != gen {
                            return;
                        }
                        let Some(nav) = nav_weak.upgrade() else { return };
                        match result {
                            Ok(items) => render(vec![search_section(items, &nav, &session_search)]),
                            // Toast, not a page: the search page stays up with
                            // its current rows, so the next keystroke can retry.
                            Err(err) => toast_error("Search", &err),
                        }
                    });
                });
            }
        });
        nav.push(&page);
    });
}

/// Turns a row index into the tracks that row stands for, and hands them to
/// `done`.
///
/// Behind an `Rc<dyn Fn>` and callback-shaped because most rows stand for tracks
/// the page never fetched - an album row means that album's tracks, an artist row
/// means their whole catalogue - so resolving one is a server round trip. Only
/// the page knows what its own rows are, which is why each builds its own.
type Resolve = Rc<dyn Fn(usize, Box<dyn FnOnce(Vec<Item>)>)>;

/// The two enqueue entries every library row gets.
///
/// "Add to queue" is first because the first entry is also what Shift+Enter runs
/// (see `wire_arrow_keys`). Neither plays anything now: they append to the end of
/// the queue, or insert directly after the current track.
fn enqueue_actions(player: &PlayerHandle, resolve: Resolve) -> Vec<RowAction> {
    let last = {
        let player = player.clone();
        let resolve = resolve.clone();
        RowAction::new("Add to queue", move |index| {
            let player = player.clone();
            resolve(
                index,
                Box::new(move |items| {
                    let count = items.len();
                    if !send_enqueue(&player, Command::QueueLast { items }) {
                        return;
                    }
                    toast_info(format!("{} added to the queue", tracks(count)));
                }),
            );
        })
    };
    let next = {
        let player = player.clone();
        RowAction::new("Play next", move |index| {
            let player = player.clone();
            resolve(
                index,
                Box::new(move |items| {
                    let count = items.len();
                    if !send_enqueue(&player, Command::QueueNext { items }) {
                        return;
                    }
                    toast_info(format!("{} playing next", tracks(count)));
                }),
            );
        })
    };
    vec![last, next]
}

/// Sends an enqueue command unless it would be empty. False means nothing was
/// sent, so the caller must not claim otherwise.
///
/// An empty resolution is worth saying out loud rather than swallowing: it means
/// the server returned no tracks for that album or artist, which looks exactly
/// like a broken menu entry from the outside.
fn send_enqueue(player: &PlayerHandle, command: Command) -> bool {
    let empty = match &command {
        Command::QueueLast { items } | Command::QueueNext { items } => items.is_empty(),
        _ => false,
    };
    if empty {
        tracing::warn!("nothing to enqueue: the server returned no tracks");
        toast_info("Nothing to queue".to_string());
        return false;
    }
    player.send(command);
    true
}

fn tracks(count: usize) -> String {
    if count == 1 {
        "1 track".to_string()
    } else {
        format!("{count} tracks")
    }
}

/// Rows that are already the tracks themselves.
fn resolve_tracks(items: Vec<Item>) -> Resolve {
    Rc::new(move |index, done| {
        if let Some(item) = items.get(index) {
            done(vec![item.clone()]);
        }
    })
}

/// Album rows: the album's tracks, in listing order.
fn resolve_albums(session: &Session, albums: Vec<Item>) -> Resolve {
    let session = session.clone();
    Rc::new(move |index, done| {
        let Some(album) = albums.get(index) else { return };
        let title = album.name.clone();
        session
            .browser
            .clone()
            .album_tracks(&album.id, move |result| match result {
                Ok(tracks) => done(tracks),
                Err(err) => toast_error(&title, &err),
            });
    })
}

/// Artist rows: everything by that artist, album by album.
fn resolve_artists(session: &Session, artists: Vec<Item>) -> Resolve {
    let session = session.clone();
    Rc::new(move |index, done| {
        let Some(artist) = artists.get(index) else { return };
        let title = artist.name.clone();
        session
            .browser
            .clone()
            .artist_tracks(&artist.id, move |result| match result {
                Ok(tracks) => done(tracks),
                Err(err) => toast_error(&title, &err),
            });
    })
}

/// Search hits: whatever the row happens to be, resolved by its own kind - the
/// same branch its activation takes.
fn resolve_search(session: &Session, items: Vec<Item>) -> Resolve {
    let session = session.clone();
    Rc::new(move |index, done| {
        let Some(item) = items.get(index) else { return };
        let title = item.name.clone();
        match item.kind() {
            Kind::Artist => session
                .browser
                .clone()
                .artist_tracks(&item.id, move |result| match result {
                    Ok(tracks) => done(tracks),
                    Err(err) => toast_error(&title, &err),
                }),
            Kind::Album => session
                .browser
                .clone()
                .album_tracks(&item.id, move |result| match result {
                    Ok(tracks) => done(tracks),
                    Err(err) => toast_error(&title, &err),
                }),
            Kind::Track | Kind::Other => done(vec![item.clone()]),
        }
    })
}

/// The default Library content: every artist, activating into their albums.
fn artist_section(artists: Vec<Item>, nav: &adw::NavigationView, session: &Session) -> Section {
    let nav_weak = nav.downgrade();
    let menu = enqueue_actions(&session.player, resolve_artists(session, artists.clone()));
    let session = session.clone();
    let rows = artists.clone();
    Section::new(artists, |_| None, move |index| {
        let Some(nav) = nav_weak.upgrade() else { return };
        let Some(artist) = rows.get(index) else { return };
        push_albums(&nav, &session, &artist.id, &artist.name);
    })
    .with_menu(menu)
}

/// A Library search hit: mixed artists, albums and tracks, each activating
/// according to its own kind rather than one shared meaning for the row.
/// Artists and albums are still just navigation, in keeping with the rest of
/// the browse model; a track plays like a track row anywhere else - itself
/// first, the rest of its scope shuffled behind it - except a search hit has
/// no page to take that scope from, so `play_search_track` builds one: its
/// album, or failing that its artist, or failing that a fresh random queue.
fn search_section(items: Vec<Item>, nav: &adw::NavigationView, session: &Session) -> Section {
    let nav_weak = nav.downgrade();
    let menu = enqueue_actions(&session.player, resolve_search(session, items.clone()));
    let session = session.clone();
    let rows = items.clone();
    Section::new(items, ListPage::search_subtitle, move |index| {
        let Some(nav) = nav_weak.upgrade() else { return };
        let Some(item) = rows.get(index) else { return };
        match item.kind() {
            Kind::Artist => push_albums(&nav, &session, &item.id, &item.name),
            Kind::Album => push_tracks(&nav, &session, &item.id, &item.name),
            Kind::Track | Kind::Other => play_search_track(&nav, &session, item.clone()),
        }
    })
    .with_menu(menu)
}

/// Queues a Library search hit's scope, chosen track first: its album if it
/// has one, else everything by its (first credited) artist, else a fresh
/// random queue - a search hit should never leave the queue at just one
/// track. Navigation pops back to now-playing immediately; the fetch behind
/// it (if any) finishes after, same as any other track row.
fn play_search_track(nav: &adw::NavigationView, session: &Session, item: Item) {
    nav.pop_to_tag("now-playing");

    if let Some(album_id) = item.album_id.clone() {
        let player = session.player.clone();
        session.browser.clone().album_tracks(&album_id, move |result| {
            let tracks = result.unwrap_or_default();
            play_shuffled_or_random(&player, tracks, &item);
        });
        return;
    }

    if let Some(artist) = item.artist_items.first().cloned() {
        let player = session.player.clone();
        session.browser.clone().artist_tracks(&artist.id, move |result| {
            let tracks = result.unwrap_or_default();
            play_shuffled_or_random(&player, tracks, &item);
        });
        return;
    }

    session.player.send(Command::PlayRandom);
}

/// Shuffles `tracks` behind `chosen`, or falls back to `PlayRandom` if the
/// fetch behind them came back empty (or failed) - the point of the fallback
/// chain in `play_search_track` is that the queue is never left with just the
/// one clicked track.
fn play_shuffled_or_random(player: &PlayerHandle, tracks: Vec<Item>, chosen: &Item) {
    if tracks.is_empty() {
        player.send(Command::PlayRandom);
        return;
    }
    let first = tracks.iter().position(|t| t.id == chosen.id).unwrap_or(0);
    player.send(Command::PlayShuffled {
        items: tracks,
        first,
    });
}

/// Artist page: albums, plus a second section for tracks that belong to no
/// album. Badly tagged libraries do have those, and an album list alone hides
/// them completely.
fn push_albums(nav: &adw::NavigationView, session: &Session, artist_id: &str, artist_name: &str) {
    let nav_weak = nav.downgrade();
    let session = session.clone();
    let title = artist_name.to_string();

    session
        .browser
        .clone()
        .artist_page(artist_id, move |result| {
            let Some(nav) = nav_weak.upgrade() else { return };
            let (albums, tracks) = match result {
                Ok(pair) => pair,
                Err(err) => return toast_error(&title, &err),
            };

            // Only the album-less tracks are listed - the rest are reachable
            // through their albums - but playback covers everything by the
            // artist, so both lists are needed.
            let loose: Vec<_> = tracks
                .iter()
                .filter(|t| t.album_id.is_none())
                .cloned()
                .collect();

            let mut sections = Vec::new();

            let nav_for_rows = nav.downgrade();
            let session_rows = session.clone();
            let album_items = albums.clone();
            let albums_menu =
                enqueue_actions(&session.player, resolve_albums(&session, albums.clone()));
            let mut albums_section = Section::new(albums, ListPage::album_subtitle, move |index| {
                let Some(nav) = nav_for_rows.upgrade() else {
                    return;
                };
                let Some(album) = album_items.get(index) else {
                    return;
                };
                push_tracks(&nav, &session_rows, &album.id, &album.name);
            })
            .with_menu(albums_menu);

            if !loose.is_empty() {
                // Headings only earn their space once there are two sections.
                albums_section = albums_section.with_heading("Albums");
            }
            sections.push(albums_section);

            if !loose.is_empty() {
                let player = session.player.clone();
                let nav_loose = nav.downgrade();
                let shown = loose.clone();
                let loose_menu = enqueue_actions(&session.player, resolve_tracks(loose.clone()));
                sections.push(
                    Section::new(loose, ListPage::loose_track_subtitle, move |index| {
                        // The row's index is into the listed tracks; the queue is
                        // built from the artist's whole catalogue, so the chosen
                        // track has to be located in that instead.
                        let Some(chosen) = shown.get(index) else { return };
                        let Some(first) = tracks.iter().position(|t| t.id == chosen.id) else {
                            return;
                        };
                        player.send(Command::PlayShuffled {
                            items: tracks.clone(),
                            first,
                        });
                        if let Some(nav) = nav_loose.upgrade() {
                            nav.pop_to_tag("now-playing");
                        }
                    })
                    .with_heading("Other tracks")
                    .with_menu(loose_menu),
                );
            }

            nav.push(&ListPage::build_sections(&title, "Albums", sections, None));
        });
}

fn push_tracks(nav: &adw::NavigationView, session: &Session, album_id: &str, album_name: &str) {
    let nav_weak = nav.downgrade();
    let session = session.clone();
    let title = album_name.to_string();
    let album_id = album_id.to_string();

    session
        .browser
        .clone()
        .album_tracks(&album_id, move |result| {
            let Some(nav) = nav_weak.upgrade() else { return };
            let items = match result {
                Ok(items) => items,
                Err(err) => return toast_error(&title, &err),
            };

            // Activating a track plays it and shuffles the rest of the album
            // behind it - picking a song is a choice about that song, not a
            // request to hear the album from there. The header's Play button is
            // what plays the album in order. Starting playback always returns to
            // now-playing, so the browse stack does not pile up behind it.
            let player_rows = session.player.clone();
            let tracks = items.clone();
            let nav_rows = nav.downgrade();

            let player_all = session.player.clone();
            let all_tracks = items.clone();
            let nav_all = nav.downgrade();

            let menu = enqueue_actions(&session.player, resolve_tracks(items.clone()));
            let section = Section::new(items, ListPage::track_subtitle, move |index| {
                player_rows.send(Command::PlayShuffled {
                    items: tracks.clone(),
                    first: index,
                });
                if let Some(nav) = nav_rows.upgrade() {
                    nav.pop_to_tag("now-playing");
                }
            })
            .with_menu(menu);

            let page = ListPage::build_sections(
                &title,
                "Tracks",
                vec![section],
                Some((
                    "Play",
                    Box::new(move || {
                        player_all.send(Command::PlayItems {
                            items: all_tracks.clone(),
                            start: 0,
                        });
                        if let Some(nav) = nav_all.upgrade() {
                            nav.pop_to_tag("now-playing");
                        }
                    }),
                )),
            );
            nav.push(&page);
        });
}

/// Reports a failed library query as a toast, leaving the user where they are.
///
/// It used to push a whole page whose only content was the error text, which
/// meant a transient network blip cost a navigation step and hid the list that
/// was already on screen. `context` is the page the query was for ("Library",
/// an artist name), since the message on its own does not say what failed.
///
/// Nothing happens if the popup is gone - the log line below is then the record,
/// which is also why it is logged even when a toast is shown: the popup is
/// usually hidden, and a toast that times out unseen leaves no trace.
/// A plain confirmation through the same overlay as the errors.
///
/// Enqueueing needs one: it deliberately changes nothing on screen (no playback,
/// no navigation), so without a word from the app a working "Play next" and a
/// broken one look identical.
fn toast_info(message: String) {
    let toaster = TOASTER.with(|slot| slot.borrow().clone());
    if let Some(toaster) = toaster {
        toaster.show(message);
    }
}

fn toast_error(context: &str, err: &anyhow::Error) {
    tracing::warn!(%err, context, "library query failed");
    let message = format!("{context}: {err:#}");
    // Cloned out of the thread local before showing: `add_toast` can run a
    // pending toast's `dismissed` handler synchronously, and holding the borrow
    // across that invites a re-entrant borrow_mut for no benefit.
    let toaster = TOASTER.with(|slot| slot.borrow().clone());
    if let Some(toaster) = toaster {
        toaster.show(message);
    }
}

/// Shown when there are no credentials or no audio device.
fn signed_out_page() -> gtk::Widget {
    let label = gtk::Label::builder()
        .label("Not signed in.\n\nRun `trayplay login` in a terminal, then restart trayplay.")
        .justify(gtk::Justification::Center)
        .wrap(true)
        .build();
    label.set_widget_name("trayplay-status");

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(gtk::Align::Center)
        .build();
    body.add_css_class("trayplay-body");
    body.append(&label);
    body.upcast()
}

thread_local! {
    /// The live popup's toaster.
    ///
    /// A thread local rather than an argument threaded through every `push_*`
    /// function: a library query fails inside a callback four levels down from
    /// the page that started it, and there is exactly one popup per process
    /// (`build` early-returns on a second `activate`) with everything that could
    /// raise a toast living on the GTK thread. `Toaster` itself is `Rc` and
    /// holds widgets, so it could not be a plain static anyway.
    static TOASTER: RefCell<Option<Rc<Toaster>>> = RefCell::new(None);
}

/// Transient error banner stacked over the whole window.
struct Toaster {
    overlay: adw::ToastOverlay,
    /// The message currently on screen, so a repeat can be suppressed.
    showing: RefCell<Option<String>>,
}

impl Toaster {
    fn new(child: &adw::NavigationView) -> Rc<Self> {
        let overlay = adw::ToastOverlay::new();
        // Stable selector root for themes.
        overlay.set_widget_name("trayplay-toast");
        overlay.set_child(Some(child));
        let toaster = Rc::new(Self {
            overlay,
            showing: RefCell::new(None),
        });
        TOASTER.with(|slot| *slot.borrow_mut() = Some(toaster.clone()));
        toaster
    }

    /// Shows `message`, ignoring it while an identical one is still visible.
    ///
    /// Failures arrive in bursts - a seek on an unseekable track emits one per
    /// attempt, and a queue of unplayable tracks emits one per skip - and
    /// ToastOverlay shows them strictly one at a time, so without this the same
    /// sentence would replay for half a minute.
    fn show(self: &Rc<Self>, message: String) {
        if self.showing.borrow().as_deref() == Some(message.as_str()) {
            return;
        }

        let toast = adw::Toast::builder()
            .title(&message)
            .timeout(TOAST_TIMEOUT_SECS)
            .build();
        // Error strings are plain text and can contain angle brackets from a
        // transport error, which Pango would reject as bad markup.
        toast.set_use_markup(false);

        // Weak, or the toast's own handler would keep the Toaster alive through
        // the closure it holds.
        let weak = Rc::downgrade(self);
        toast.connect_dismissed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.showing.replace(None);
            }
        });

        self.showing.replace(Some(message));
        self.overlay.add_toast(toast);
    }
}

fn spawn_event_loop(
    now_playing: Rc<NowPlaying>,
    toaster: Rc<Toaster>,
    events: async_channel::Receiver<Event>,
) {
    glib::spawn_future_local(async move {
        while let Ok(event) = events.recv().await {
            match event {
                Event::TrackChanged(item) => now_playing.set_track(item),
                // The queue page listens for this itself; nothing on the root
                // view shows the queue.
                Event::QueueChanged => {}
                Event::RepeatChanged(repeat) => now_playing.set_repeat(repeat),
                Event::StateChanged(state) => now_playing.set_state(state),
                Event::Position(pos) => now_playing.set_position(pos),
                Event::Seeked(pos) => now_playing.set_seeked(pos),
                Event::Failed(message) => {
                    // A refused seek ("this track cannot be seeked") never
                    // confirms, so release the slider here rather than leaving
                    // it pinned until the timeout.
                    now_playing.cancel_pending_seek();
                    // Logged as well as shown: the popup is usually hidden, and
                    // a toast that times out while it is closed leaves the log
                    // as the only record.
                    tracing::warn!(message, "playback error");
                    toaster.show(message);
                }
            }
        }
    });
}

fn apply_layer_anchor(window: &adw::ApplicationWindow, cfg: &Config) {
    let (top, bottom, left, right) = match cfg.anchor {
        Anchor::TopLeft => (true, false, true, false),
        Anchor::TopRight => (true, false, false, true),
        Anchor::BottomLeft => (false, true, true, false),
        Anchor::BottomRight => (false, true, false, true),
    };

    for (edge, anchored) in [
        (Edge::Top, top),
        (Edge::Bottom, bottom),
        (Edge::Left, left),
        (Edge::Right, right),
    ] {
        window.set_anchor(edge, anchored);
        if anchored {
            window.set_margin(edge, cfg.margin);
        }
    }
}
