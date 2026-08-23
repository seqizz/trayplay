//! Horizontal strip of artist buttons, one per credited artist.
//!
//! A track can credit more than one artist, and each of them is a place you can
//! navigate to, so a single label is both wrong and unclickable for all but the
//! first. The strip lists them all and scrolls when they do not fit: hover and
//! use the wheel, or click and drag it.
//!
//! Overflow is shown by fading the artists out at whichever edge has more to
//! come, which is why this is a widget and not a Box in a ScrolledWindow: GTK4
//! has no `mask-image`, and CSS gradients cannot fade a widget to transparent -
//! only to a colour, which would smear over the cover art behind it. The fade is
//! a `gsk` mask over the child's own snapshot, as in `ui/artwork.rs`.

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::jellyfin::models::NameId;

/// Width of the fade at each edge, in pixels. Reached gradually: a strip
/// scrolled two pixels off the end shows two pixels of fade, not a hard band.
const FADE: f32 = 28.0;

/// How far one wheel notch moves the strip.
const WHEEL_STEP: f64 = 48.0;

/// How far a press has to travel before it scrolls rather than activating the
/// artist under it.
const DRAG_THRESHOLD: f64 = 6.0;

mod imp {
    use std::cell::Cell;

    use gtk::gdk;
    use gtk::glib;
    use gtk::graphene;
    use gtk::gsk;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;

    pub struct ArtistStrip {
        pub scroller: gtk::ScrolledWindow,
        pub row: gtk::Box,
        /// Set while a drag is scrolling, so the click that ends it does not
        /// navigate.
        pub dragging: Cell<bool>,
    }

    impl Default for ArtistStrip {
        fn default() -> Self {
            // Centred so a single artist sits under the title like the album
            // line does. Once the artists overflow, the box is wider than the
            // viewport and alignment stops mattering - it scrolls instead.
            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(2)
                .halign(gtk::Align::Center)
                .build();

            let scroller = gtk::ScrolledWindow::builder()
                // External rather than Never: Never makes the scrolled window
                // demand its child's full width, which would widen the popup.
                .hscrollbar_policy(gtk::PolicyType::External)
                .vscrollbar_policy(gtk::PolicyType::Never)
                .child(&row)
                .build();

            Self {
                scroller,
                row,
                dragging: Cell::new(false),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ArtistStrip {
        const NAME: &'static str = "TrayplayArtistStrip";
        type Type = super::ArtistStrip;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for ArtistStrip {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            self.scroller.set_parent(&*obj);

            // The fade depends on how far the strip is scrolled and on how much
            // there is to scroll, so both have to trigger a redraw.
            let adjustment = self.scroller.hadjustment();
            for property in ["value", "upper", "page-size"] {
                let weak = obj.downgrade();
                adjustment.connect_notify_local(Some(property), move |_, _| {
                    if let Some(obj) = weak.upgrade() {
                        obj.queue_draw();
                    }
                });
            }
        }

        // A custom widget owns its children explicitly; without this the child
        // outlives the parent and GTK complains on teardown.
        fn dispose(&self) {
            self.scroller.unparent();
        }
    }

    impl WidgetImpl for ArtistStrip {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let (_, natural, _, _) = self.scroller.measure(orientation, for_size);
            match orientation {
                // Zero minimum width: the strip is allowed to be narrower than
                // its contents, which is the whole point of it scrolling.
                gtk::Orientation::Horizontal => (0, natural, -1, -1),
                _ => {
                    let (minimum, natural, _, _) = self.scroller.measure(orientation, for_size);
                    (minimum, natural, -1, -1)
                }
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.scroller.allocate(width, height, baseline, None);
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let width = widget.width() as f32;
            let height = widget.height() as f32;
            if width <= 0.0 || height <= 0.0 {
                return;
            }

            let adjustment = self.scroller.hadjustment();
            let before = adjustment.value() as f32;
            let after =
                (adjustment.upper() - adjustment.page_size() - adjustment.value()).max(0.0) as f32;

            // Proportional to how much is actually hidden, so the fade grows in
            // as the strip starts moving instead of appearing all at once.
            let left = (before / super::FADE).min(1.0) * super::FADE;
            let right = (after / super::FADE).min(1.0) * super::FADE;

            // Nothing hidden, or too narrow to fade without swallowing the
            // content: draw it plainly.
            if (left + right) < 1.0 || (left + right) >= width {
                widget.snapshot_child(&self.scroller, snapshot);
                return;
            }

            snapshot.push_mask(gsk::MaskMode::Alpha);

            let opaque = gdk::RGBA::new(1.0, 1.0, 1.0, 1.0);
            let clear = gdk::RGBA::new(1.0, 1.0, 1.0, 0.0);
            let mut stops = Vec::with_capacity(4);
            stops.push(gsk::ColorStop::new(
                0.0,
                if left > 0.0 { clear } else { opaque },
            ));
            stops.push(gsk::ColorStop::new(left / width, opaque));
            stops.push(gsk::ColorStop::new(1.0 - right / width, opaque));
            stops.push(gsk::ColorStop::new(
                1.0,
                if right > 0.0 { clear } else { opaque },
            ));

            snapshot.append_linear_gradient(
                &graphene::Rect::new(0.0, 0.0, width, height),
                &graphene::Point::new(0.0, 0.0),
                &graphene::Point::new(width, 0.0),
                &stops,
            );
            // A mask node records the mask first and the source second, so this
            // takes two pops.
            snapshot.pop();

            widget.snapshot_child(&self.scroller, snapshot);
            snapshot.pop();
        }
    }
}

glib::wrapper! {
    pub struct ArtistStrip(ObjectSubclass<imp::ArtistStrip>)
        @extends gtk::Widget;
}

impl Default for ArtistStrip {
    fn default() -> Self {
        Self::new()
    }
}

impl ArtistStrip {
    pub fn new() -> Self {
        let strip: Self = glib::Object::builder().build();
        strip.set_widget_name("trayplay-artists");
        strip.wire_scrolling();
        strip
    }

    /// Replaces the contents. `fallback` is used when the server gave no artist
    /// items at all - a name with no id, so it is shown but not clickable.
    pub fn set_artists(
        &self,
        artists: &[NameId],
        fallback: &str,
        on_pick: impl Fn(&NameId) + 'static,
    ) {
        let row = &self.imp().row;
        // Sibling walk rather than repeatedly taking first_child: a remove GTK
        // declines would leave that child in place and the loop would spin on it.
        let mut child = row.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            row.remove(&widget);
        }
        // Back to the start, or a shorter list would stay scrolled off screen.
        self.imp().scroller.hadjustment().set_value(0.0);

        if artists.is_empty() {
            let label = gtk::Button::builder().label(fallback).build();
            label.add_css_class("flat");
            label.add_css_class("trayplay-artist");
            label.set_sensitive(false);
            row.append(&label);
            return;
        }

        let on_pick = std::rc::Rc::new(on_pick);
        for artist in artists {
            let button = gtk::Button::builder().label(&artist.name).build();
            button.add_css_class("flat");
            button.add_css_class("trayplay-artist");

            let on_pick = on_pick.clone();
            let artist = artist.clone();
            let strip = self.downgrade();
            button.connect_clicked(move |_| {
                // A drag that ends over a button still delivers its click; the
                // gesture sets this so the release is ignored.
                if let Some(strip) = strip.upgrade() {
                    if strip.imp().dragging.replace(false) {
                        return;
                    }
                }
                on_pick(&artist);
            });
            row.append(&button);
        }
    }

    /// Moves focus to the first artist, for the keyboard shortcut. From there
    /// GTK's own directional focus walks the row and Enter activates, so there is
    /// nothing else to wire.
    pub fn focus_first(&self) -> bool {
        match self.imp().row.first_child() {
            Some(first) if first.is_sensitive() => first.grab_focus(),
            _ => false,
        }
    }

    fn wire_scrolling(&self) {
        let adjustment = self.imp().scroller.hadjustment();

        // Both axes: a vertical wheel is the natural way to nudge a horizontal
        // strip, and a touchpad sends horizontal deltas of its own.
        let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        wheel.connect_scroll({
            let adjustment = adjustment.clone();
            move |_, dx, dy| {
                let delta = if dx != 0.0 { dx } else { dy };
                set_clamped(&adjustment, adjustment.value() + delta * WHEEL_STEP);
                glib::Propagation::Stop
            }
        });
        self.add_controller(wheel);

        // Same shape as the list pages: capture phase so it sees presses that
        // land on a button, unclaimed until the pointer has actually moved.
        let drag = gtk::GestureDrag::new();
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);

        let origin = std::rc::Rc::new(std::cell::Cell::new(0.0));
        drag.connect_drag_begin({
            let origin = origin.clone();
            let adjustment = adjustment.clone();
            move |_, _, _| origin.set(adjustment.value())
        });
        drag.connect_drag_update({
            let origin = origin.clone();
            let adjustment = adjustment.clone();
            let strip = self.downgrade();
            move |gesture, offset, _| {
                if offset.abs() < DRAG_THRESHOLD {
                    return;
                }
                gesture.set_state(gtk::EventSequenceState::Claimed);
                if let Some(strip) = strip.upgrade() {
                    strip.imp().dragging.set(true);
                }
                set_clamped(&adjustment, origin.get() - offset);
            }
        });
        self.add_controller(drag);
    }
}

fn set_clamped(adjustment: &gtk::Adjustment, value: f64) {
    let max = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
    adjustment.set_value(value.clamp(adjustment.lower(), max));
}
