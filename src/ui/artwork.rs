use adw::prelude::*;
use gtk::glib;
use gtk::subclass::prelude::*;

/// Blur radius applied to the lower part of the art.
const BLUR_RADIUS: f64 = 22.0;

/// How long one cover dissolves into the next. Short enough not to feel like an
/// effect; long enough that a track change is not a flash.
const CROSSFADE_MS: u32 = 220;

/// Where the sharp-to-blurred transition starts and ends, as a fraction of the
/// widget height. The band between them is the gradient.
///
/// Deliberately wide. A narrow band puts a visible horizontal seam across the
/// art where sharp meets blurred; spreading it over most of the height makes the
/// change gradual enough that there is no edge to see. It costs sharpness at the
/// top, which is fine - this is a backdrop.
const FADE_START: f32 = 0.08;
const FADE_END: f32 = 0.88;

/// Angle of the placeholder text, degrees, negative for rising to the right.
const PLACEHOLDER_ANGLE: f32 = -20.0;

/// One full sweep of the placeholder gradient's drift, one way. Also drives the
/// pattern's slow zoom.
///
/// Slow on purpose: this is meant to be noticed only if you look, and it is the
/// one thing in the app that animates continuously. It runs only while the widget
/// is mapped, so a hidden popup costs nothing.
const DRIFT_MS: u32 = 14_000;

/// How long the pattern takes to travel one row, i.e. one full period.
///
/// The slide is linear and repeating rather than alternating: the pattern is
/// periodic, so wrapping from one row to the next is invisible, and a pattern
/// that reversed direction would be very visible indeed.
const SLIDE_MS: u32 = 11_000;

/// Zoom applied to the pattern at the far end of the drift. Small - the point is
/// that the panel is never quite still, not that it breathes.
const PATTERN_ZOOM: f32 = 0.08;

/// Where the drift is parked when motion is switched off. Mid-cycle rather than
/// 0, so the still panel gets the gradient at its most even rather than at one
/// extreme of the sweep.
const PARKED_DRIFT: f64 = 0.5;

/// Gap between repetitions, as a fraction of the text's own size: sideways
/// between copies on a row, and vertically between rows.
const PATTERN_GAP_X: f32 = 0.8;
const PATTERN_GAP_Y: f32 = 0.9;

mod imp {
    use std::cell::{Cell, RefCell};

    use gtk::gdk;
    use gtk::glib;
    use gtk::graphene;
    use gtk::gsk;
    use gtk::pango;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;

    pub struct ArtBackdrop {
        pub texture: RefCell<Option<gdk::Texture>>,
        /// The cover being faded out from underneath the new one.
        pub outgoing: RefCell<Option<gdk::Texture>>,
        /// How far the incoming cover has faded in, 0 to 1.
        pub progress: Cell<f64>,
        /// Held so the animation is not dropped mid-fade, which cancels it.
        pub crossfade: RefCell<Option<adw::TimedAnimation>>,
        pub blur: Cell<f64>,
        pub fade_start: Cell<f32>,
        pub fade_end: Cell<f32>,
        /// Text drawn over a generated gradient when the track has no cover.
        pub placeholder: RefCell<Option<String>>,
        /// Where the placeholder gradient has drifted to, 0 to 1.
        pub drift: Cell<f64>,
        /// How far the pattern has slid towards the next row, 0 to 1.
        pub slide: Cell<f64>,
        /// Held for the same reason as `crossfade`: dropping it stops it.
        pub drift_animation: RefCell<Option<adw::TimedAnimation>>,
        pub slide_animation: RefCell<Option<adw::TimedAnimation>>,
        /// Settings' "reduce motion": the panel is still drawn, it just holds
        /// still.
        pub reduce_motion: Cell<bool>,
    }

    impl Default for ArtBackdrop {
        fn default() -> Self {
            Self {
                texture: RefCell::new(None),
                outgoing: RefCell::new(None),
                progress: Cell::new(1.0),
                crossfade: RefCell::new(None),
                blur: Cell::new(super::BLUR_RADIUS),
                fade_start: Cell::new(super::FADE_START),
                fade_end: Cell::new(super::FADE_END),
                placeholder: RefCell::new(None),
                drift: Cell::new(0.0),
                slide: Cell::new(0.0),
                drift_animation: RefCell::new(None),
                slide_animation: RefCell::new(None),
                reduce_motion: Cell::new(false),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ArtBackdrop {
        const NAME: &'static str = "TrayplayArtBackdrop";
        type Type = super::ArtBackdrop;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for ArtBackdrop {
        fn constructed(&self) {
            self.parent_constructed();
            // The art is scaled to cover, so it must not paint outside itself.
            self.obj().set_overflow(gtk::Overflow::Hidden);
        }
    }

    impl WidgetImpl for ArtBackdrop {
        /// Requests nothing: this is a backdrop and takes whatever the overlay
        /// gives it.
        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            (0, 0, -1, -1)
        }

        /// The drift only runs while the widget is on screen. The popup spends
        /// most of its life hidden, and an animation ticking behind a closed
        /// window is pure waste - it would also keep the compositor busy.
        fn map(&self) {
            self.parent_map();
            self.obj().start_drift();
        }

        fn unmap(&self) {
            self.obj().stop_drift();
            self.parent_unmap();
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let width = widget.width() as f32;
            let height = widget.height() as f32;
            if width <= 0.0 || height <= 0.0 {
                return;
            }

            let progress = self.progress.get();
            let incoming = self.texture.borrow().clone();

            // No cover anywhere: the generated panel takes the whole area. Drawn
            // instead of the art rather than under it, so a track that does have
            // art never pays for it.
            if incoming.is_none() && self.outgoing.borrow().is_none() {
                if let Some(text) = self.placeholder.borrow().clone() {
                    self.draw_placeholder(snapshot, &text, width, height);
                }
                return;
            }

            // The old cover stays fully opaque underneath and the new one fades
            // in over it, rather than both being cross-dissolved: dissolving
            // would dip towards the empty background halfway through, which is
            // the flash this is here to avoid.
            if progress < 1.0 {
                if let Some(previous) = self.outgoing.borrow().clone() {
                    self.draw_cover(snapshot, &previous, width, height);
                }
            }

            let Some(texture) = incoming else {
                return;
            };

            if progress < 1.0 {
                snapshot.push_opacity(progress);
                self.draw_cover(snapshot, &texture, width, height);
                snapshot.pop();
            } else {
                self.draw_cover(snapshot, &texture, width, height);
            }
        }
    }

    impl ArtBackdrop {
        /// The stand-in for a track with no cover: a colour panel derived from
        /// the text itself, with that text set large and on an angle across it.
        ///
        /// Derived rather than random so one album always looks the same - the
        /// panel becomes a weak visual identity for it instead of noise that
        /// changes every time the track comes round again.
        fn draw_placeholder(&self, snapshot: &gtk::Snapshot, text: &str, width: f32, height: f32) {
            let bounds = graphene::Rect::new(0.0, 0.0, width, height);
            let seed = hash(text.as_bytes());
            let hue = (seed % 360) as f32;
            let drift = self.drift.get() as f32;

            // Diagonal, with both ends anchored off-canvas: the drift moves the
            // stops without ever bringing an end point into view, which would
            // read as a hard edge sliding past.
            snapshot.append_linear_gradient(
                &bounds,
                &graphene::Point::new(width * (-0.35 + 0.3 * drift), -0.2 * height),
                &graphene::Point::new(width * (1.15 - 0.2 * drift), 1.2 * height),
                &[
                    gsk::ColorStop::new(0.0, hsl(hue, 0.42, 0.34)),
                    gsk::ColorStop::new(0.55, hsl(hue + 26.0, 0.38, 0.22)),
                    gsk::ColorStop::new(1.0, hsl(hue + 52.0, 0.34, 0.11)),
                ],
            );

            let widget = self.obj();
            let layout = widget.create_pango_layout(Some(&text.to_uppercase()));

            let mut font = pango::FontDescription::new();
            font.set_weight(pango::Weight::Bold);
            // A bundled family, chosen from the same hash as the colour: the
            // album keeps one look rather than being redrawn differently every
            // time it comes round. Nothing bundled means the default sans.
            let families = crate::fonts::families();
            if !families.is_empty() {
                font.set_family(families[(seed % families.len() as u64) as usize]);
            }
            // Scaled to the widget, as an absolute (pixel) size: it has to tile
            // the panel rather than be readable at a chosen size, so it does not
            // follow the desktop's font scaling the way the tags do. Smaller than
            // a single line would be - the repetition is the effect, and a couple
            // of huge words would not read as a pattern.
            let size = (width / 13.0).clamp(14.0, 28.0);
            font.set_absolute_size(size as f64 * pango::SCALE as f64);
            layout.set_font_description(Some(&font));

            let attrs = pango::AttrList::new();
            attrs.insert(pango::AttrInt::new_letter_spacing(2 * pango::SCALE));
            layout.set_attributes(Some(&attrs));
            // No wrap and no ellipsis: a tile is one full copy of the name, and
            // the widget clips whatever runs past its edges (Overflow::Hidden).

            let (text_width, text_height) = layout.pixel_size();
            let step_x = text_width as f32 + size * super::PATTERN_GAP_X;
            let step_y = text_height as f32 + size * super::PATTERN_GAP_Y;
            if step_x <= 0.0 || step_y <= 0.0 {
                return;
            }

            // Low alpha: the scrim sits over this, the tags spell the same words
            // out below it, and there is a lot more ink here than one line of it.
            let ink = gdk::RGBA::new(1.0, 1.0, 1.0, 0.11);

            let zoom = 1.0 + super::PATTERN_ZOOM * drift;
            // The pattern is drawn rotated, so it has to cover the widget's
            // diagonal rather than its width, and the zoom shrinks the effective
            // area it covers.
            let reach = width.hypot(height) / zoom;
            let cols = (reach / step_x).ceil() as i32 + 1;
            let rows = (reach / step_y).ceil() as i32 + 1;

            snapshot.save();
            snapshot.translate(&graphene::Point::new(width / 2.0, height / 2.0));
            snapshot.rotate(super::PLACEHOLDER_ANGLE);
            snapshot.scale(zoom, zoom);

            // One row of travel per period, and half a tile sideways with it.
            //
            // The sideways part is what makes the wrap invisible, and leaving it
            // out is what made the pattern visibly cut and reset. Rows are
            // staggered by half a tile on alternate rows, so when the phase wraps
            // and every copy takes the place of the one above it, it lands in a
            // row of the *opposite* stagger - a half-tile jump sideways. Sliding
            // half a tile over the same period cancels exactly that: at the end
            // of a period the pattern is identical to its own start, one row up.
            let progress = self.slide.get() as f32;
            let phase_y = progress * step_y;
            let phase_x = progress * step_x / 2.0;

            for row in -rows..=rows {
                let y = row as f32 * step_y + phase_y;
                // Alternate rows offset, so the copies do not line up into
                // columns and the repetition is less obvious.
                let stagger = if row.rem_euclid(2) == 0 {
                    0.0
                } else {
                    step_x / 2.0
                };
                for col in -cols..=cols {
                    snapshot.save();
                    snapshot.translate(&graphene::Point::new(
                        col as f32 * step_x + stagger + phase_x - text_width as f32 / 2.0,
                        y - text_height as f32 / 2.0,
                    ));
                    snapshot.append_layout(&layout, &ink);
                    snapshot.restore();
                }
            }

            snapshot.restore();
        }

        /// One cover: a blurred copy with a sharp one masked over its top.
        fn draw_cover(
            &self,
            snapshot: &gtk::Snapshot,
            texture: &gdk::Texture,
            width: f32,
            height: f32,
        ) {
            let bounds = graphene::Rect::new(0.0, 0.0, width, height);
            let art = cover_rect(width, height, texture.width(), texture.height());

            // Fully blurred copy underneath.
            snapshot.push_blur(self.blur.get());
            snapshot.append_texture(texture, &art);
            snapshot.pop();

            // Sharp copy on top, masked so it fades out further down. A mask
            // node records the mask first, then the source, so there are two
            // pops rather than one.
            snapshot.push_mask(gsk::MaskMode::Alpha);

            let opaque = gdk::RGBA::new(1.0, 1.0, 1.0, 1.0);
            let clear = gdk::RGBA::new(1.0, 1.0, 1.0, 0.0);
            snapshot.append_linear_gradient(
                &bounds,
                &graphene::Point::new(0.0, height * self.fade_start.get()),
                &graphene::Point::new(0.0, height * self.fade_end.get()),
                &[
                    gsk::ColorStop::new(0.0, opaque),
                    gsk::ColorStop::new(1.0, clear),
                ],
            );
            snapshot.pop();

            snapshot.append_texture(texture, &art);
            snapshot.pop();
        }
    }

    /// FNV-1a. Hand-rolled rather than `DefaultHasher` because the value decides
    /// what colour an album is: `DefaultHasher`'s output is explicitly not stable
    /// across releases, so an album's panel would change colour on a rustc
    /// upgrade for no reason anyone could explain.
    fn hash(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in bytes {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    /// HSL to RGBA, hue in degrees (wrapped), saturation and lightness 0 to 1.
    ///
    /// Colours are picked in HSL because the derived palette needs one varying
    /// axis (hue, from the hash) with saturation and lightness held at values
    /// that keep text readable over them. That is awkward to express in RGB.
    fn hsl(hue: f32, saturation: f32, lightness: f32) -> gdk::RGBA {
        let hue = hue.rem_euclid(360.0) / 60.0;
        let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
        let second = chroma * (1.0 - (hue % 2.0 - 1.0).abs());
        let (r, g, b) = match hue as u32 {
            0 => (chroma, second, 0.0),
            1 => (second, chroma, 0.0),
            2 => (0.0, chroma, second),
            3 => (0.0, second, chroma),
            4 => (second, 0.0, chroma),
            _ => (chroma, 0.0, second),
        };
        let base = lightness - chroma / 2.0;
        gdk::RGBA::new(r + base, g + base, b + base, 1.0)
    }

    /// Rectangle that scales the texture to cover the widget, centred, keeping
    /// aspect ratio. Equivalent to ContentFit::Cover on a GtkPicture.
    fn cover_rect(width: f32, height: f32, tex_width: i32, tex_height: i32) -> graphene::Rect {
        let tex_width = tex_width as f32;
        let tex_height = tex_height as f32;
        if tex_width <= 0.0 || tex_height <= 0.0 {
            return graphene::Rect::new(0.0, 0.0, width, height);
        }

        let scale = (width / tex_width).max(height / tex_height);
        let scaled_width = tex_width * scale;
        let scaled_height = tex_height * scale;
        graphene::Rect::new(
            (width - scaled_width) / 2.0,
            (height - scaled_height) / 2.0,
            scaled_width,
            scaled_height,
        )
    }
}

glib::wrapper! {
    /// Cover art backdrop with a vertical sharp-to-blurred gradient.
    ///
    /// GTK's CSS `filter: blur()` is uniform across a widget, so a gradual
    /// transition is not expressible in CSS. This draws the texture twice and
    /// combines the two with a mask node instead.
    pub struct ArtBackdrop(ObjectSubclass<imp::ArtBackdrop>)
        @extends gtk::Widget;
}

impl Default for ArtBackdrop {
    fn default() -> Self {
        Self::new()
    }
}

impl ArtBackdrop {
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    /// Replaces the cover, dissolving from the current one.
    ///
    /// Fades only when there is something to fade *from* and *to*: clearing to
    /// nothing, or arriving from nothing, lands immediately. Half a fade against
    /// an empty backdrop reads as a flicker rather than a transition.
    pub fn set_texture(&self, texture: Option<gtk::gdk::Texture>) {
        let imp = self.imp();
        let current = imp.texture.borrow().clone();
        // Same cover twice - two tracks off one album - is not a transition.
        if current == texture {
            return;
        }

        let fade = current.is_some() && texture.is_some();
        *imp.outgoing.borrow_mut() = current;
        *imp.texture.borrow_mut() = texture;

        if !fade {
            imp.progress.set(1.0);
            *imp.crossfade.borrow_mut() = None;
            *imp.outgoing.borrow_mut() = None;
            self.queue_draw();
            return;
        }

        let target = adw::CallbackAnimationTarget::new({
            let weak = self.downgrade();
            move |value| {
                if let Some(backdrop) = weak.upgrade() {
                    backdrop.imp().progress.set(value);
                    backdrop.queue_draw();
                }
            }
        });

        let animation = adw::TimedAnimation::new(self, 0.0, 1.0, CROSSFADE_MS, target);
        animation.set_easing(adw::Easing::EaseOutCubic);
        // Dropping the previous cover once it is invisible: two full-size
        // textures are worth holding for 220ms, not for the rest of the track.
        animation.connect_done({
            let weak = self.downgrade();
            move |_| {
                if let Some(backdrop) = weak.upgrade() {
                    *backdrop.imp().outgoing.borrow_mut() = None;
                }
            }
        });
        animation.play();
        *imp.crossfade.borrow_mut() = Some(animation);
    }

    /// Text for the generated panel shown when a track has no cover art.
    ///
    /// Kept even while a cover is showing, so a track change from art to no art
    /// has something to fall back to without a second call.
    pub fn set_placeholder(&self, text: Option<String>) {
        let imp = self.imp();
        if *imp.placeholder.borrow() == text {
            return;
        }
        *imp.placeholder.borrow_mut() = text;
        // Nothing to drift for once there is no placeholder; and if one just
        // appeared while the popup is open, it should start moving now rather
        // than at the next map.
        if imp.placeholder.borrow().is_some() {
            self.start_drift();
        } else {
            self.stop_drift();
        }
        self.queue_draw();
    }

    /// Holds the no-art panel still, or lets it move again.
    ///
    /// The panel is still drawn either way - this is about motion, not about the
    /// decoration. Stopping parks it at a fixed point in the cycle rather than
    /// wherever the animation happened to be, so the still version looks the same
    /// every time instead of depending on when the switch was flipped.
    pub fn set_reduce_motion(&self, reduce: bool) {
        let imp = self.imp();
        if imp.reduce_motion.get() == reduce {
            return;
        }
        imp.reduce_motion.set(reduce);

        if reduce {
            self.stop_drift();
            imp.drift.set(PARKED_DRIFT);
            imp.slide.set(0.0);
            self.queue_draw();
        } else {
            self.start_drift();
        }
    }

    /// Starts the placeholder's two animations, unless they are already running
    /// or there is nothing to animate.
    ///
    /// Two rather than one because they need opposite behaviour at the end of a
    /// period: the gradient and the zoom reverse (a jump back would show), while
    /// the slide must carry on in the same direction (a reversal would show).
    fn start_drift(&self) {
        let imp = self.imp();
        if imp.reduce_motion.get() || imp.placeholder.borrow().is_none() || !self.is_mapped() {
            return;
        }

        if imp.drift_animation.borrow().is_none() {
            let target = adw::CallbackAnimationTarget::new({
                let weak = self.downgrade();
                move |value| {
                    if let Some(backdrop) = weak.upgrade() {
                        backdrop.imp().drift.set(value);
                        backdrop.queue_draw();
                    }
                }
            });

            let animation = adw::TimedAnimation::new(self, 0.0, 1.0, DRIFT_MS, target);
            animation.set_easing(adw::Easing::EaseInOutSine);
            animation.set_alternate(true);
            animation.set_repeat_count(0);
            animation.play();
            *imp.drift_animation.borrow_mut() = Some(animation);
        }

        if imp.slide_animation.borrow().is_none() {
            let target = adw::CallbackAnimationTarget::new({
                let weak = self.downgrade();
                move |value| {
                    if let Some(backdrop) = weak.upgrade() {
                        backdrop.imp().slide.set(value);
                        backdrop.queue_draw();
                    }
                }
            });

            let animation = adw::TimedAnimation::new(self, 0.0, 1.0, SLIDE_MS, target);
            // Linear, or the pattern would visibly speed up and slow down once a
            // period while never changing direction.
            animation.set_easing(adw::Easing::Linear);
            animation.set_repeat_count(0);
            animation.play();
            *imp.slide_animation.borrow_mut() = Some(animation);
        }
    }

    fn stop_drift(&self) {
        let imp = self.imp();
        if let Some(animation) = imp.drift_animation.borrow_mut().take() {
            animation.pause();
        }
        if let Some(animation) = imp.slide_animation.borrow_mut().take() {
            animation.pause();
        }
    }

    /// Blur radius of the lower region, in pixels.
    pub fn set_blur(&self, radius: f64) {
        self.imp().blur.set(radius);
        self.queue_draw();
    }

    /// Fractions of the height where the blur starts and finishes ramping in.
    pub fn set_fade(&self, start: f32, end: f32) {
        self.imp().fade_start.set(start.clamp(0.0, 1.0));
        self.imp().fade_end.set(end.clamp(0.0, 1.0));
        self.queue_draw();
    }
}
