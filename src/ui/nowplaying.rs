use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::gdk;
use gtk::glib;

use crate::config::{Repeat, Settings};
use crate::jellyfin::models::Item;
use crate::player::{Command, State};

use super::artists::ArtistStrip;
use super::artwork::ArtBackdrop;
use super::Session;

/// Cover art is requested large because it is stretched across the whole popup
/// as a backdrop, not shown as a thumbnail.
const ART_HEIGHT: u32 = 768;

/// How long to wait for the slider to settle before acting on a drag.
const SEEK_DEBOUNCE: Duration = Duration::from_millis(150);

/// Fraction of the track one arrow key covers.
const SEEK_KEY_FRACTION: f64 = 0.10;

/// How long artist focus survives without a keypress. Without this the strip
/// keeps a focus ring long after the shortcut has been forgotten about.
const ARTIST_FOCUS_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the track's text takes to fade in on a change. Shorter than the
/// cover crossfade: the text is what is being read, so it should settle first.
const TAG_FADE_MS: u32 = 160;

/// How far the text drops back before fading in. Not to zero - a full blink is
/// more distracting than the snap it replaces.
const TAG_FADE_FROM: f64 = 0.35;

/// How long the slider may stay pinned to a requested position before position
/// events take it back.
///
/// A seek is not instant: it waits for the debounce, then for the cache entry to
/// be complete, then rebuilds a decoder. Some of those cannot be confirmed at
/// all - a `Seek` dropped by `try_send` under load produces no event either way -
/// so the pin needs an expiry, not only a confirmation.
const SEEK_PIN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a track may be loading before anything is shown for it.
///
/// A cached track opens in a millisecond or two, and most do: without a grace
/// period the indicator would flash on every single Next press and read as a
/// glitch rather than as information.
const LOAD_GRACE: Duration = Duration::from_millis(350);

/// How often the loading bar pulses while the download length is unknown - a
/// server-side transcode sends no Content-Length, so there is no fraction to
/// draw and the bar can only say "still working".
const LOAD_PULSE: Duration = Duration::from_millis(120);

/// How long a load may run before it is worth interrupting for.
///
/// The bar is deliberately quiet, which is right for the two-second case and
/// wrong for the one where the server has stopped answering. This is the point
/// where a hidden popup should have left something behind in the log and a
/// visible one should say so in words.
const LOAD_NAG: Duration = Duration::from_secs(8);

/// Transport glyph size in pixels. Larger than the 16px icon-theme default
/// because the glyphs carry no button shape, so nothing else gives them
/// presence.
const GLYPH_SIZE: i32 = 24;

/// Play/pause, a fifth larger than the rest: it is the control that gets used.
const PLAY_GLYPH_SIZE: i32 = 29;

/// Instant mix, a little under the rest of the row.
///
/// Not a taste exception - it is a correction for the artwork, and the row looks
/// uneven without it. Font Awesome's wand ink spans about 97% of its canvas where
/// the ionicons glyphs beside it span about 87%, so at a shared `GLYPH_SIZE` it
/// draws roughly a tenth larger than its neighbours while nominally matching
/// them. 21 puts the *ink* back level with shuffle's.
const MIX_GLYPH_SIZE: i32 = 21;

/// Action row glyphs. Settings stays at the icon-theme default, being a utility
/// button in a fixed square; Library and Queue are destinations and carry the
/// row, so they get a fifth more.
const ACTION_GLYPH_SIZE: i32 = 16;
const ACTION_GLYPH_SIZE_LARGE: i32 = 19;

/// How far from the requested position a position report may land and still
/// count as the seek having arrived. One tick of the player's 250ms clock, with
/// room for the byte-offset estimate being approximate.
const SEEK_PIN_TOLERANCE: f64 = 2.0;

/// What the user asked to navigate to from the current track.
#[derive(Debug, Clone)]
pub enum Navigate {
    Artist { id: String, name: String },
    Album { id: String, name: String },
    Library,
    Queue,
    Settings,
}

/// The root view: cover, tags, seek bar and transport.
pub struct NowPlaying {
    pub root: gtk::Widget,
    /// Cover art, drawn behind everything, sharp at the top and blurred lower
    /// down where the text sits.
    backdrop: ArtBackdrop,
    title: gtk::Label,
    /// One button per credited artist, scrollable when they do not fit.
    artists: ArtistStrip,
    album: gtk::Button,
    /// Title, artists and album together, so the three can fade in as one on a
    /// track change instead of snapping to the new text.
    tags: gtk::Box,
    /// Held so the fade is not dropped mid-flight, which cancels it.
    tag_fade: RefCell<Option<adw::TimedAnimation>>,
    seek: gtk::Scale,
    /// Download progress for the track being loaded, overlaid on the seek bar so
    /// showing it never reflows the popup.
    loading: gtk::ProgressBar,
    /// Item id being waited for. `Event::Buffering` also reports prefetches, and
    /// only the track the user is waiting for should be drawn.
    load_id: Rc<RefCell<Option<String>>>,
    /// Bumped per load, so the grace timer, the pulse and the nag from an
    /// earlier one all become no-ops without anything to cancel.
    load_generation: Rc<Cell<u64>>,
    /// True once real byte counts have arrived, which stops the pulse: the two
    /// cannot both drive the same bar.
    load_determinate: Rc<Cell<bool>>,
    play_button: gtk::Button,
    /// The play button's image, so the icon can be swapped without rebuilding
    /// the child and losing its pixel size.
    play_icon: gtk::Image,
    /// Same reason as `play_icon`: the repeat button's glyph changes with the
    /// setting, and it is the only indication of which state is in force.
    repeat_icon: gtk::Image,
    /// Instant mix, kept because it is only usable with a track to seed from.
    mix: gtk::Button,
    /// Current track, needed to resolve artist/album navigation targets.
    track: Rc<RefCell<Option<Item>>>,
    /// Position the user asked for, while the seek is still in flight. Position
    /// reports are ignored until it is reached, so the slider does not snap back
    /// to where playback still is and then forward again.
    pin: Rc<Cell<Option<(f64, Instant)>>>,
    /// Bumped per seek request; only the newest one is sent. A drag emits a
    /// continuous stream and every seek rebuilds a decoder.
    seek_generation: Rc<Cell<u64>>,
    /// True while an artist button holds focus from the `A` shortcut, which is
    /// what makes the arrows navigate artists rather than seek.
    artist_mode: Rc<Cell<bool>>,
    /// Bumped on every keypress that keeps artist focus alive, so a stale
    /// timeout does nothing.
    artist_generation: Rc<Cell<u64>>,
    /// Bumped per track, so a cover fetch that lands after the track has changed
    /// is discarded instead of showing the wrong art.
    art_generation: Rc<Cell<u64>>,
    /// Kept because the artist buttons are rebuilt per track, so their
    /// navigation has to be wired again each time rather than once at
    /// construction.
    on_navigate: Rc<dyn Fn(Navigate)>,
    session: Session,
}

impl NowPlaying {
    pub fn new(session: Session, on_navigate: impl Fn(Navigate) + 'static) -> Rc<Self> {
        let on_navigate: Rc<dyn Fn(Navigate)> = Rc::new(on_navigate);

        // Cover fills the popup and is cropped rather than letterboxed. The
        // widget itself does the sharp-to-blurred gradient; the scrim on top
        // only handles text contrast.
        let backdrop = ArtBackdrop::new();
        backdrop.set_widget_name("trayplay-art");

        // Reserves room at the top for the art to show through. Always present
        // and expanding, so the controls stay bottom-anchored; its minimum height
        // comes from CSS and applies only under `.has-art`, which is what lets a
        // "nothing playing" popup collapse to a compact layout. Not kept as a
        // field: nothing touches it again after construction.
        let art_space = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        art_space.set_widget_name("trayplay-art-space");

        let title = gtk::Label::builder()
            .label("Nothing playing")
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .wrap(false)
            .build();
        title.set_widget_name("trayplay-title");
        title.add_css_class("title-3");

        let artists = ArtistStrip::new();
        let album = flat_button("trayplay-album");

        let seek = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1.0);
        seek.set_widget_name("trayplay-seek");
        seek.set_hexpand(true);
        seek.set_sensitive(false);

        // No permanent elapsed/total labels either side of the bar. The elapsed
        // time is GtkScale's own value, drawn above the handle and following it,
        // and only visible while the bar is being used - the only time the exact
        // number matters. Total length is dropped with them: it was the less
        // useful half, and the bar itself shows progress.
        //
        // Drawn at all times and hidden with opacity in CSS rather than toggled
        // through draw-value: the label counts towards the widget's height, so
        // switching it on and off resized the scale and shifted the rows below it
        // every time the pointer crossed the bar.
        seek.set_draw_value(true);
        seek.set_value_pos(gtk::PositionType::Top);
        // A setter, not a signal: GTK4 replaced ::format-value with
        // gtk_scale_set_format_value_func.
        seek.set_format_value_func(|_, value| format_time(value));

        let hover = gtk::EventControllerMotion::new();
        hover.connect_enter({
            let seek = seek.clone();
            move |_, _, _| seek.add_css_class("showing-value")
        });
        hover.connect_leave({
            let seek = seek.clone();
            move |_| seek.remove_css_class("showing-value")
        });
        seek.add_controller(hover);

        // Sits on the seek bar rather than in the box next to it: appearing and
        // disappearing must not move the transport row, and the bar is where a
        // user already looks for playback progress.
        let loading = gtk::ProgressBar::builder()
            .visible(false)
            .valign(gtk::Align::End)
            .can_target(false)
            .build();
        loading.set_widget_name("trayplay-loading");

        let prev = icon_button("trayplay-prev-symbolic", "trayplay-prev", GLYPH_SIZE);
        // The image is kept: set_state swaps the icon on it. Calling
        // Button::set_icon_name instead would replace the child with a fresh
        // image of the default size and undo the sizing below.
        let play_icon = gtk::Image::from_icon_name("trayplay-play-symbolic");
        play_icon.set_pixel_size(PLAY_GLYPH_SIZE);
        let play_button = glyph_button(&play_icon, "trayplay-play");
        let next = icon_button("trayplay-next-symbolic", "trayplay-next", GLYPH_SIZE);

        // Shuffle sits with the transport rather than in the action row: it
        // starts playback, which is what the row around it does.
        let random = icon_button("trayplay-shuffle-symbolic", "trayplay-random", GLYPH_SIZE);
        random.set_tooltip_text(Some("Random play"));

        // Next to shuffle because it is the same kind of control - it replaces
        // the queue with one nobody typed out - and the glyph says so, being
        // shuffle's with a sparkle on it. Insensitive until there is a track to
        // seed the mix from, which `set_track` maintains.
        let mix = icon_button("trayplay-mix-symbolic", "trayplay-mix", MIX_GLYPH_SIZE);
        mix.set_tooltip_text(Some("Instant mix from this track"));
        mix.set_sensitive(false);

        // Balances shuffle on the other side of the transport. The glyph shows
        // the state that is in force, not what the next click would do, so a
        // plain forward arrow means "no repeat".
        let repeat_icon = gtk::Image::from_icon_name("trayplay-repeat-off-symbolic");
        repeat_icon.set_pixel_size(GLYPH_SIZE);
        let repeat = glyph_button(&repeat_icon, "trayplay-repeat");

        // The three buttons stay centred while shuffle and repeat are pushed out
        // to the edges, so adding either does not shift play-pause off centre.
        let transport = gtk::CenterBox::new();
        transport.set_widget_name("trayplay-transport");

        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        buttons.append(&prev);
        buttons.append(&play_button);
        buttons.append(&next);
        // Both queue-building buttons on the same side, in one box: a CenterBox
        // keeps its centre child centred regardless of what the ends weigh, so
        // play-pause does not move when this side grows.
        let generators = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        generators.append(&random);
        generators.append(&mix);

        transport.set_center_widget(Some(&buttons));
        transport.set_start_widget(Some(&repeat));
        transport.set_end_widget(Some(&generators));

        // Icons rather than labels: the glyphs carry the meaning, and tooltips
        // keep them discoverable.
        let settings = action_button(
            "trayplay-settings-symbolic",
            "trayplay-settings",
            "Settings",
            ACTION_GLYPH_SIZE,
        );
        // Square and only as wide as it needs to be, so it reads as a utility
        // button rather than a third destination.
        settings.set_hexpand(false);
        // Library rather than a search field: every list page has type-to-filter,
        // so opening the library and typing does the same job with one less
        // destination to maintain.
        let library = action_button(
            "trayplay-library-symbolic",
            "trayplay-library",
            "Library",
            ACTION_GLYPH_SIZE_LARGE,
        );
        let queue = action_button(
            "trayplay-queue-symbolic",
            "trayplay-queue",
            "Queue",
            ACTION_GLYPH_SIZE_LARGE,
        );

        let actions = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        actions.set_widget_name("trayplay-actions");
        actions.append(&settings);
        actions.append(&library);
        actions.append(&queue);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();
        content.add_css_class("trayplay-body");
        content.append(&art_space);
        // Pushes the controls to the bottom so the art has the top of the popup.
        art_space.set_vexpand(true);
        // Title, artists and album fade in together on a track change, so
        // grouped rather than appended one by one.
        let tags = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        tags.set_widget_name("trayplay-tags");
        tags.append(&title);
        tags.append(&artists);
        tags.append(&album);

        content.append(&tags);

        let seek_area = gtk::Overlay::new();
        seek_area.set_child(Some(&seek));
        seek_area.add_overlay(&loading);
        content.append(&seek_area);
        content.append(&transport);
        content.append(&actions);

        // Backdrop is the overlay's own child so it fills the popup; the scrim
        // and the controls stack on top of it.
        let scrim = gtk::Box::new(gtk::Orientation::Vertical, 0);
        scrim.set_widget_name("trayplay-scrim");

        let root = gtk::Overlay::new();
        root.set_child(Some(&backdrop));
        root.add_overlay(&scrim);
        root.add_overlay(&content);

        let this = Rc::new(Self {
            root: root.upcast(),
            backdrop,
            title,
            artists,
            album,
            tags,
            tag_fade: RefCell::new(None),
            seek,
            loading,
            load_id: Rc::new(RefCell::new(None)),
            load_generation: Rc::new(Cell::new(0)),
            load_determinate: Rc::new(Cell::new(false)),
            play_button,
            play_icon,
            repeat_icon,
            mix: mix.clone(),
            track: Rc::new(RefCell::new(None)),
            pin: Rc::new(Cell::new(None)),
            seek_generation: Rc::new(Cell::new(0)),
            artist_mode: Rc::new(Cell::new(false)),
            artist_generation: Rc::new(Cell::new(0)),
            art_generation: Rc::new(Cell::new(0)),
            on_navigate: on_navigate.clone(),
            session,
        });

        // Whatever was in force last session. Repeat has no event to wait for
        // unless something changes it, and motion has no event at all.
        // Not named `settings`: that is the settings *button* a few lines up.
        let stored = Settings::load();
        this.set_repeat(stored.repeat);
        this.set_reduce_motion(stored.reduce_motion);

        this.wire(
            &prev, &next, &random, &repeat, &library, &queue, &settings, on_navigate,
        );
        this.wire_mix();
        this
    }

    /// Handles a single-key shortcut for the root view.
    ///
    /// Called from the window's key controller rather than from one on this
    /// view's own widgets: key events only reach widgets in the focus chain, and
    /// with nothing inside the view focused this root is not in it. The caller is
    /// responsible for only routing keys here while now-playing is the visible
    /// page.
    pub fn handle_key(
        self: &Rc<Self>,
        key: gdk::Key,
        modifiers: gdk::ModifierType,
    ) -> glib::Propagation {
        // Anything with a modifier belongs to the window or the desktop. Shift
        // is the exception: it is how A is told from a.
        if modifiers.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK) {
            return glib::Propagation::Proceed;
        }

        // While an artist is focused the arrows walk the strip and Escape backs
        // out of it, rather than seeking and closing the popup. Every one of
        // these keeps the focus alive for another five seconds.
        if self.artist_mode.get() {
            match key {
                gdk::Key::Left | gdk::Key::Right => {
                    self.bump_artist_focus();
                    return glib::Propagation::Proceed;
                }
                gdk::Key::Escape => {
                    self.exit_artist_mode();
                    return glib::Propagation::Stop;
                }
                // GTK activates the focused button; the navigation that follows
                // ends the mode by itself.
                gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::ISO_Enter => {
                    self.artist_mode.set(false);
                    return glib::Propagation::Proceed;
                }
                // Any other shortcut means the user has moved on.
                _ => self.exit_artist_mode(),
            }
        }

        match key {
            // Ten percent a press. Sensitivity stands in for "the length is
            // known": an unseekable or unloaded track has an insensitive bar.
            gdk::Key::Left | gdk::Key::Right => {
                if !self.seek.is_sensitive() {
                    return glib::Propagation::Stop;
                }
                let span = self.seek.adjustment().upper();
                let step = span * SEEK_KEY_FRACTION;
                let target = if key == gdk::Key::Left {
                    self.seek.value() - step
                } else {
                    self.seek.value() + step
                };
                self.request_seek(target.clamp(0.0, span));
            }
            gdk::Key::space => self.session.player.send(Command::PlayPause),
            gdk::Key::n => self.session.player.send(Command::Next),
            gdk::Key::p => self.session.player.send(Command::Previous),
            // Cycles the same way the button does; the glyph is what reports
            // where it landed.
            gdk::Key::r => self.session.player.send(Command::CycleRepeat),
            gdk::Key::l => (self.on_navigate)(Navigate::Library),
            gdk::Key::q => (self.on_navigate)(Navigate::Queue),
            gdk::Key::s => (self.on_navigate)(Navigate::Settings),
            gdk::Key::a => {
                let target = self.track.borrow().as_ref().and_then(|item| {
                    Some(Navigate::Album {
                        id: item.album_id.clone()?,
                        name: item.album.clone()?,
                    })
                });
                // Nothing playing, or a track with no album: the key is still
                // swallowed, so it never reaches a focused widget instead.
                if let Some(target) = target {
                    (self.on_navigate)(target);
                }
            }
            // Focus rather than navigate: *which* artist is the point of the
            // shortcut, so the arrows and Enter make the choice. Both are GTK's
            // own behaviour for a focused button in a box.
            gdk::Key::A => self.enter_artist_mode(),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    }

    fn enter_artist_mode(self: &Rc<Self>) {
        // Nothing to focus on a track with no navigable artist.
        if !self.artists.focus_first() {
            return;
        }
        self.artist_mode.set(true);
        self.bump_artist_focus();
    }

    /// Restarts the idle timeout. A generation counter rather than cancelling the
    /// timeout: nothing to unregister, and a stale callback simply does nothing.
    fn bump_artist_focus(self: &Rc<Self>) {
        let generation = self.artist_generation.get().wrapping_add(1);
        self.artist_generation.set(generation);

        let this = Rc::downgrade(self);
        glib::timeout_add_local_once(ARTIST_FOCUS_TIMEOUT, move || {
            let Some(this) = this.upgrade() else { return };
            if this.artist_generation.get() == generation {
                this.exit_artist_mode();
            }
        });
    }

    /// Drops the focus ring by clearing the window's focus outright: moving it
    /// somewhere else would only put a ring on that instead.
    fn exit_artist_mode(&self) {
        if !self.artist_mode.replace(false) {
            return;
        }
        if let Some(window) = self.root.root().and_downcast::<gtk::Window>() {
            // Qualified: GtkWindowExt and RootExt both offer set_focus on a
            // window, and they are equivalent here.
            gtk::prelude::GtkWindowExt::set_focus(&window, gtk::Widget::NONE);
        }
    }

    /// Moves the slider and asks the player to follow, debounced.
    ///
    /// Shared by the drag and the arrow keys: both can arrive in bursts, and
    /// every seek that reaches the player rebuilds a decoder. The pin and the
    /// pulse are set here too, so a keyboard seek behaves exactly like a dragged
    /// one while it settles.
    fn request_seek(&self, secs: f64) {
        let secs = secs.max(0.0);
        self.pin.set(Some((secs, Instant::now())));
        self.seek.add_css_class("seeking");
        self.seek.set_value(secs);

        let generation = self.seek_generation.get().wrapping_add(1);
        self.seek_generation.set(generation);

        let player = self.session.player.clone();
        let pending = self.seek_generation.clone();
        glib::timeout_add_local_once(SEEK_DEBOUNCE, move || {
            if pending.get() == generation {
                player.send(Command::Seek(Duration::from_secs_f64(secs)));
            }
        });
    }

    /// Separate from `wire` only because that one is already at clippy's
    /// argument limit, and the mix button is the one whose handler needs the
    /// current track rather than just the player.
    fn wire_mix(self: &Rc<Self>) {
        let player = self.session.player.clone();
        let track = self.track.clone();
        self.mix.connect_clicked(move |_| {
            // Sensitivity already implies a track, but reading it back is the
            // only way to know *which* - and a click can still land in the gap
            // between a track ending and the button being greyed out again.
            let Some(id) = track.borrow().as_ref().map(|item| item.id.clone()) else {
                return;
            };
            player.send(Command::PlayInstantMix(id));
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn wire(
        self: &Rc<Self>,
        prev: &gtk::Button,
        next: &gtk::Button,
        random: &gtk::Button,
        repeat: &gtk::Button,
        library: &gtk::Button,
        queue: &gtk::Button,
        settings: &gtk::Button,
        on_navigate: Rc<dyn Fn(Navigate)>,
    ) {
        let player_prev = self.session.player.clone();
        prev.connect_clicked(move |_| player_prev.send(Command::Previous));

        let player_next = self.session.player.clone();
        next.connect_clicked(move |_| player_next.send(Command::Next));

        let player_play = self.session.player.clone();
        self.play_button
            .connect_clicked(move |_| player_play.send(Command::PlayPause));

        let player_random = self.session.player.clone();
        random.connect_clicked(move |_| player_random.send(Command::PlayRandom));

        // The button asks for the next state and waits to be told what it became
        // (`Event::RepeatChanged`), rather than painting the new glyph itself:
        // MPRIS can change this too, so the player is the only thing that knows
        // the current value.
        let player_repeat = self.session.player.clone();
        repeat.connect_clicked(move |_| player_repeat.send(Command::CycleRepeat));

        let nav = on_navigate.clone();
        library.connect_clicked(move |_| nav(Navigate::Library));

        let nav_queue = on_navigate.clone();
        queue.connect_clicked(move |_| nav_queue(Navigate::Queue));

        let nav_settings = on_navigate.clone();
        settings.connect_clicked(move |_| nav_settings(Navigate::Settings));

        // change_value only fires for user interaction, so programmatic updates
        // from position events cannot feed back into a seek loop. The debounce,
        // the pin and the pulse all live in request_seek, which the arrow keys
        // share.
        let this = Rc::downgrade(self);
        self.seek.connect_change_value(move |_, _, value| {
            if let Some(this) = this.upgrade() {
                this.request_seek(value);
            }
            glib::Propagation::Proceed
        });

        // Artist buttons are wired in set_track instead: there is one per
        // credited artist and they are rebuilt whenever the track changes.

        let track_album = self.track.clone();
        let nav_album = on_navigate;
        self.album.connect_clicked(move |_| {
            let Some(item) = track_album.borrow().clone() else {
                return;
            };
            if let (Some(id), Some(name)) = (item.album_id.clone(), item.album.clone()) {
                nav_album(Navigate::Album { id, name });
            }
        });
    }

    /// A track is on its way. Nothing is shown for `LOAD_GRACE`, so a cached
    /// track - the common case - passes through invisibly.
    ///
    /// `None` is a load whose track is not known yet (a random queue being
    /// fetched): the bar still appears, it just cannot name anything if it has
    /// to nag.
    pub fn set_loading(self: &Rc<Self>, item: Option<Item>) {
        let id = item.as_ref().map(|item| item.id.clone());
        // The same track is announced twice for a Next - once before the queue
        // refill, once when the cursor reaches it - and restarting the grace
        // period there would blink the bar back off mid-load.
        if id.is_some() && *self.load_id.borrow() == id {
            return;
        }

        let generation = self.load_generation.get().wrapping_add(1);
        self.load_generation.set(generation);
        *self.load_id.borrow_mut() = id;
        self.load_determinate.set(false);
        self.loading.set_fraction(0.0);
        self.loading.set_visible(false);

        let name = item.map(|item| item.name);
        let this = Rc::downgrade(self);
        glib::timeout_add_local_once(LOAD_GRACE, move || {
            let Some(this) = this.upgrade() else { return };
            if this.load_generation.get() != generation {
                return;
            }
            this.loading.set_visible(true);
            this.pulse_loading(generation);
            this.nag_loading(generation, name);
        });
    }

    /// Drives the bar while the download length is unknown. Stops as soon as
    /// real byte counts arrive, or the load ends.
    fn pulse_loading(self: &Rc<Self>, generation: u64) {
        let this = Rc::downgrade(self);
        glib::timeout_add_local(LOAD_PULSE, move || {
            let Some(this) = this.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if this.load_generation.get() != generation || this.load_determinate.get() {
                return glib::ControlFlow::Break;
            }
            this.loading.pulse();
            glib::ControlFlow::Continue
        });
    }

    /// Says it in words once a load has gone on long enough to look broken.
    ///
    /// A toast rather than something on the bar: the popup is usually hidden, so
    /// this is also what puts the wait in the log.
    fn nag_loading(self: &Rc<Self>, generation: u64, name: Option<String>) {
        let this = Rc::downgrade(self);
        glib::timeout_add_local_once(LOAD_NAG, move || {
            let Some(this) = this.upgrade() else { return };
            if this.load_generation.get() != generation {
                return;
            }
            let message = match &name {
                Some(name) => format!("Still waiting for “{name}”"),
                None => "Still waiting for the server".to_string(),
            };
            tracing::warn!(message, "track load is taking a long time");
            super::popup::toast_info(message);
        });
    }

    /// Download progress for one cache entry.
    ///
    /// Ignored unless it describes the track being waited for: prefetches report
    /// on the same channel, and drawing one would show a bar racing ahead of a
    /// track that has not started.
    pub fn set_buffering(&self, id: &str, got: u64, total: Option<u64>) {
        if self.load_id.borrow().as_deref() != Some(id) {
            return;
        }
        // No Content-Length: the pulse stays in charge, since a fraction cannot
        // be computed at all.
        let Some(total) = total.filter(|total| *total > 0) else {
            return;
        };
        self.load_determinate.set(true);
        self.loading
            .set_fraction((got as f64 / total as f64).clamp(0.0, 1.0));
    }

    /// Ends the current load, whichever way it went. Both outcomes reach this:
    /// the track starting (`TrackChanged`) and the load failing (`Failed`).
    pub fn clear_loading(&self) {
        self.load_generation
            .set(self.load_generation.get().wrapping_add(1));
        *self.load_id.borrow_mut() = None;
        self.load_determinate.set(false);
        self.loading.set_visible(false);
        self.loading.set_fraction(0.0);
    }

    pub fn set_track(self: &Rc<Self>, item: Option<Item>) {
        // Whatever was loading has either arrived or been given up on.
        self.clear_loading();
        // A track change outranks any seek still in flight on the old one.
        self.release_pin();
        // The artist buttons are about to be rebuilt, so any focus among them is
        // gone regardless; this keeps the flag honest. It is a no-op unless the
        // mode was actually on, so a focused filter box on another page is safe.
        self.exit_artist_mode();

        // Invalidates any cover fetch already in flight, including on the way to
        // "nothing playing": a late reply must not paint art back over an empty
        // popup.
        let generation = self.art_generation.get().wrapping_add(1);
        self.art_generation.set(generation);

        let Some(item) = item else {
            self.title.set_label("Nothing playing");
            self.album.set_label("");
            self.artists.set_visible(false);
            self.album.set_visible(false);
            self.backdrop.set_texture(None);
            // Nothing playing is the one state that still collapses: there is no
            // name to build a panel out of, so there would be nothing in it.
            self.backdrop.set_placeholder(None);
            self.root.remove_css_class("has-art");
            self.seek.set_sensitive(false);
            self.seek.set_value(0.0);
            self.mix.set_sensitive(false);
            *self.track.borrow_mut() = None;
            return;
        };

        self.title.set_label(&item.name);
        self.mix.set_sensitive(true);

        // Every credited artist, each linking to its own page. `display_artist`
        // is the fallback for a track the server gave no artist items for: it is
        // a name without an id, so it shows but does not navigate.
        let navigate = self.on_navigate.clone();
        self.artists
            .set_artists(&item.artist_items, item.display_artist(), move |artist| {
                navigate(Navigate::Artist {
                    id: artist.id.clone(),
                    name: artist.name.clone(),
                });
            });
        self.artists.set_visible(true);

        match (&item.album, &item.album_id) {
            (Some(album), Some(_)) => {
                self.album.set_label(album);
                self.album.set_visible(true);
                self.album.set_sensitive(true);
            }
            (Some(album), None) => {
                self.album.set_label(album);
                self.album.set_visible(true);
                self.album.set_sensitive(false);
            }
            _ => self.album.set_visible(false),
        }

        let secs = item.duration().map(|d| d.as_secs_f64()).unwrap_or(0.0);
        self.seek.set_range(0.0, secs.max(1.0));
        self.seek.set_value(0.0);
        self.seek.set_sensitive(secs > 0.0);

        // What the generated panel says if this track turns out to have no cover.
        // The album, because that is what the panel stands in for and what makes
        // one colour recognisable across a whole record; the artist or the title
        // only when there is no album to name.
        self.backdrop.set_placeholder(Some(placeholder_text(&item)));

        // The old cover deliberately stays up until the new one has arrived: it
        // is replaced by a crossfade, and clearing it here would put a hole in
        // the popup for the length of the fetch - the flash this avoids.
        //
        // The generation bumped above guards against a slow fetch landing after
        // the track has already moved on, which would otherwise leave the wrong
        // art up until the next change.
        let this = Rc::downgrade(self);
        let pending = self.art_generation.clone();
        self.session.browser.cover(&item, ART_HEIGHT, move |bytes| {
            if pending.get() != generation {
                return;
            }
            let Some(this) = this.upgrade() else { return };

            // No art at all: clear the texture, which is what lets the generated
            // panel show through. `.has-art` stays on, because there is now
            // something worth reserving the space for - it used to come off here
            // and collapse the layout.
            let Some(bytes) = bytes else {
                this.backdrop.set_texture(None);
                this.root.add_css_class("has-art");
                return;
            };
            match gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)) {
                Ok(texture) => {
                    this.backdrop.set_texture(Some(texture));
                    this.root.add_css_class("has-art");
                }
                Err(err) => tracing::debug!(%err, "cannot decode cover art"),
            }
        });

        self.fade_tags();
        *self.track.borrow_mut() = Some(item);
    }

    /// Fades the track's text back in, so a change reads as a transition rather
    /// than as the labels snapping to new content.
    fn fade_tags(&self) {
        let target = adw::PropertyAnimationTarget::new(&self.tags, "opacity");
        let animation =
            adw::TimedAnimation::new(&self.tags, TAG_FADE_FROM, 1.0, TAG_FADE_MS, target);
        animation.set_easing(adw::Easing::EaseOutCubic);
        animation.play();
        // Replacing any fade still running: a rapid Next should restart it, not
        // race the previous one.
        *self.tag_fade.borrow_mut() = Some(animation);
    }

    pub fn set_state(&self, state: State) {
        let icon = match state {
            State::Playing => "trayplay-pause-symbolic",
            _ => "trayplay-play-symbolic",
        };
        self.play_icon.set_icon_name(Some(icon));
    }

    /// Passes the setting down to the backdrop, which owns the animations.
    pub fn set_reduce_motion(&self, reduce: bool) {
        self.backdrop.set_reduce_motion(reduce);
    }

    /// Paints the repeat state. The tooltip carries the wording, since three
    /// similar glyphs cannot say "all" and "one" on their own.
    pub fn set_repeat(&self, repeat: Repeat) {
        let (icon, tip) = match repeat {
            Repeat::Off => ("trayplay-repeat-off-symbolic", "Repeat: off"),
            Repeat::All => ("trayplay-repeat-all-symbolic", "Repeat: whole queue"),
            Repeat::One => ("trayplay-repeat-one-symbolic", "Repeat: this track"),
        };
        self.repeat_icon.set_icon_name(Some(icon));
        // A state the glyph alone would not distinguish gets a class as well, so
        // a theme can highlight an active repeat.
        if let Some(button) = self.repeat_icon.parent() {
            button.remove_css_class("repeat-active");
            if repeat != Repeat::Off {
                button.add_css_class("repeat-active");
            }
            button.set_tooltip_text(Some(tip));
        }
    }

    pub fn set_position(&self, pos: Duration) {
        let secs = pos.as_secs_f64();

        if let Some((target, since)) = self.pin.get() {
            let arrived = (secs - target).abs() <= SEEK_PIN_TOLERANCE;
            if !arrived && since.elapsed() < SEEK_PIN_TIMEOUT {
                // Still the pre-seek position: leave the slider where the user
                // put it.
                return;
            }
            self.release_pin();
        }

        self.seek.set_value(secs);
    }

    /// A seek the player has accepted.
    ///
    /// This does *not* release the pin. The player emits it as soon as the sink
    /// has been handed the command, and the sink is another thread: position
    /// reports from before the swap can still arrive afterwards. Only a position
    /// that agrees with the pin releases it, in `set_position`.
    ///
    /// It still moves the widgets, because a seek may have been issued from
    /// somewhere other than the slider - MPRIS - and then nothing is pinned.
    pub fn set_seeked(&self, pos: Duration) {
        let secs = pos.as_secs_f64();
        if self.pin.get().is_none() {
            // A seek from MPRIS: pin it here so the same protection applies, and
            // show the same pulse while it settles.
            self.pin.set(Some((secs, Instant::now())));
            self.seek.add_css_class("seeking");
        }
        self.seek.set_value(secs);
    }

    /// Drops a pin whose seek will never be confirmed, so the slider follows
    /// playback again instead of waiting out `SEEK_PIN_TIMEOUT`.
    pub fn cancel_pending_seek(&self) {
        self.release_pin();
    }

    /// Clears the pin and stops the pulse. Always together: a pulsing fill with
    /// no seek behind it never stops.
    fn release_pin(&self) {
        self.pin.set(None);
        self.seek.remove_css_class("seeking");
    }
}

fn flat_button(name: &str) -> gtk::Button {
    // Centred and only as wide as its label: appended to a vertical box it would
    // otherwise stretch across the popup, and the hover highlight with it.
    let button = gtk::Button::builder()
        .visible(false)
        .halign(gtk::Align::Center)
        .build();
    button.set_widget_name(name);
    button.add_css_class("flat");
    button
}

/// A button in the bottom action row: icon only, so it needs the tooltip to say
/// what it does.
/// Sized through the image for the same reason as `icon_button`: CSS
/// `-gtk-icon-size` never reaches the image inside a `from_icon_name` button.
fn action_button(icon: &str, name: &str, tooltip: &str, size: i32) -> gtk::Button {
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(size);

    let button = gtk::Button::builder().child(&image).build();
    button.set_widget_name(name);
    button.set_tooltip_text(Some(tooltip));
    button.set_hexpand(true);
    button
}

/// A transport-row button: the glyph is the whole control, with no button shape
/// behind it. `.trayplay-glyph` is what strips the shape (see default.css) -
/// Adwaita's own `.flat` still paints a background on hover.
///
/// The size is set on the image rather than through CSS `-gtk-icon-size`: that
/// property applies to the icon node, and a button built by `from_icon_name`
/// does not pass it down to the image it wraps. `set_pixel_size` is also
/// authoritative, which is what the glyphs need now that no button shape gives
/// them a minimum size.
fn icon_button(icon: &str, name: &str, size: i32) -> gtk::Button {
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(size);
    glyph_button(&image, name)
}

/// Same, for a glyph whose image is kept around to be swapped later.
fn glyph_button(image: &gtk::Image, name: &str) -> gtk::Button {
    let button = gtk::Button::builder().child(image).build();
    button.set_widget_name(name);
    button.add_css_class("trayplay-glyph");
    button
}

/// Text for the no-cover panel, and with it the colour and the font: the hash of
/// this string picks both, so it has to be the album rather than the track, or
/// every track of a record would look different.
///
/// Falling back to the credited artists (all of them) and then to the title, for
/// the badly tagged corners of a library where a track has neither.
fn placeholder_text(item: &Item) -> String {
    if let Some(album) = &item.album {
        if !album.is_empty() {
            return album.clone();
        }
    }
    // Every credited artist, not just the first: a collaboration with no album
    // and no cover would otherwise put one name on the panel and list both in the
    // tags right underneath it.
    let artists = item.display_artists();
    if artists != "Unknown Artist" {
        return artists;
    }
    item.name.clone()
}

pub fn format_time(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    format!("{}:{:02}", total / 60, total % 60)
}
