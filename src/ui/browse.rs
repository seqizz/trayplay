use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use gtk::gdk;
use gtk::glib;

use crate::jellyfin::models::{Item, Kind};

use super::nowplaying::format_time;

/// How far a press has to travel before it counts as a scroll rather than a
/// click on a row. Below this the gesture stays unclaimed, so activating a row
/// still works normally.
const DRAG_SCROLL_THRESHOLD: f64 = 8.0;

/// How long the list keeps moving after a flick.
const GLIDE_DURATION_MS: u32 = 450;

/// Seconds of travel at release speed that the glide covers. Larger feels
/// slippery, smaller feels like the list is being held back.
const GLIDE_SECONDS: f64 = 0.3;

/// Release speed below which there was no flick, just a stop. Pixels per second.
const GLIDE_MIN_VELOCITY: f64 = 80.0;

/// One entry in a row's right-click menu.
///
/// `Rc` rather than `Box` because the same callback is reached two ways: the
/// menu, and Shift+Enter on the focused row (which runs the *first* action - see
/// `wire_arrow_keys`).
#[derive(Clone)]
pub struct RowAction {
    pub label: String,
    pub run: Rc<dyn Fn(usize)>,
}

impl RowAction {
    pub fn new(label: impl Into<String>, run: impl Fn(usize) + 'static) -> Self {
        Self {
            label: label.into(),
            run: Rc::new(run),
        }
    }
}

/// One group of rows on a list page.
///
/// Pages can hold more than one because an artist has both albums and, in badly
/// tagged libraries, tracks that belong to no album at all.
pub struct Section {
    /// Shown above the rows. None for a single-section page, where the header
    /// subtitle already says what the rows are.
    pub heading: Option<String>,
    pub items: Vec<Item>,
    pub subtitle_of: Box<dyn Fn(&Item) -> Option<String>>,
    pub on_activate: Box<dyn Fn(usize)>,
    /// Right-click menu for this section's rows. Empty means no menu.
    pub menu: Vec<RowAction>,
}

impl Section {
    pub fn new(
        items: Vec<Item>,
        subtitle_of: impl Fn(&Item) -> Option<String> + 'static,
        on_activate: impl Fn(usize) + 'static,
    ) -> Self {
        Self {
            heading: None,
            items,
            subtitle_of: Box::new(subtitle_of),
            on_activate: Box::new(on_activate),
            menu: Vec::new(),
        }
    }

    pub fn with_heading(mut self, heading: impl Into<String>) -> Self {
        self.heading = Some(heading.into());
        self
    }

    /// The first action is also what Shift+Enter does on a focused row, so order
    /// it deliberately.
    pub fn with_menu(mut self, menu: Vec<RowAction>) -> Self {
        self.menu = menu;
        self
    }
}

/// Widgets that filtering needs to touch, per section.
struct SectionUi {
    heading: Option<gtk::Label>,
    list: gtk::ListBox,
    /// Lowercased row names, indexed the same as the rows. Behind a `RefCell`
    /// because a `LiveList` can replace the rows after construction, and the
    /// filter func resolves a row index through this - left immutable it would
    /// match new rows against the names of the ones they replaced.
    names: Rc<RefCell<Vec<String>>>,
    /// Kept so Shift+Enter can run the first entry without going through the
    /// popover.
    menu: Vec<RowAction>,
}

/// Handle to a page's single section, for pages whose rows change while they are
/// on screen.
///
/// Only the queue page needs this: it is a view of state the player owns and
/// keeps changing (a track advances, a random queue refills), so its rows have
/// to be rebuilt from a fresh snapshot rather than being the list they were at
/// construction. Every other list page shows a query result that cannot change
/// underneath it.
pub struct LiveList {
    list: gtk::ListBox,
    names: Rc<RefCell<Vec<String>>>,
    /// The header's own title widget, so a page whose row count changes can say
    /// so ("12 tracks") instead of keeping the count it was built with.
    title: adw::WindowTitle,
}

impl LiveList {
    /// Updates the existing rows' subtitles, leaving the rows themselves alone.
    ///
    /// The queue's common case: a track advanced, so only the "Now playing"
    /// marker moved. Rebuilding for that would destroy the focused row and drop
    /// keyboard focus mid-list every few minutes, which `replace` cannot avoid
    /// and this does not have to.
    pub fn relabel(&self, items: &[Item], subtitle_of: impl Fn(&Item) -> Option<String>) {
        for (index, item) in items.iter().enumerate() {
            let Some(row) = self
                .list
                .row_at_index(index as i32)
                .and_downcast::<adw::ActionRow>()
            else {
                continue;
            };
            row.set_subtitle(&subtitle_of(item).map_or_else(String::new, |s| markup_escape(&s)));
        }
    }

    pub fn set_kind(&self, kind: &str) {
        self.title.set_subtitle(kind);
    }

    /// Swaps in a new set of rows, keeping the section's filter honest.
    pub fn replace(
        &self,
        items: Vec<Item>,
        subtitle_of: impl Fn(&Item) -> Option<String> + 'static,
        on_activate: impl Fn(usize) + 'static,
    ) {
        // Walked by sibling rather than by repeatedly taking first_child: a
        // remove GTK refuses would leave that child in place, and the loop would
        // spin on it forever. Rows only, too - anything else parented to a list
        // is not ours to remove.
        let mut child = self.list.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            if let Some(row) = widget.downcast_ref::<gtk::ListBoxRow>() {
                self.list.remove(row);
            }
        }
        let names = fill_list(&self.list, &items, subtitle_of, on_activate);
        *self.names.borrow_mut() = names;
        // After the borrow is released: invalidate_filter runs the filter func,
        // which reads the same RefCell.
        self.list.invalidate_filter();
    }
}

pub struct ListPage;

impl ListPage {
    /// Single-section page that opens scrolled to `scroll_to` instead of the top
    /// - the queue page's "don't make me scroll to find what's playing" request -
    /// and hands back a handle for replacing its rows afterwards.
    ///
    /// Kept separate from the plain `build` rather than adding two parameters
    /// every other caller would have to pass `None` for. The queue is the only
    /// page that needs either: it opens on what is playing, and it has to follow
    /// the player from there rather than showing the snapshot it was built from
    /// forever (see `LiveList`).
    pub fn build_live(
        title: &str,
        kind: &str,
        subtitle_of: impl Fn(&Item) -> Option<String> + 'static,
        items: Vec<Item>,
        on_activate: impl Fn(usize) + 'static,
        menu: Vec<RowAction>,
        scroll_to: usize,
    ) -> (adw::NavigationPage, LiveList) {
        let (page, sections, title_widget) = Self::build_inner(
            title,
            kind,
            vec![Section::new(items, subtitle_of, on_activate).with_menu(menu)],
            None,
            Some(scroll_to),
        );
        // One section by construction, so the indexed access is sound.
        let (list, names) = {
            let sections = sections.borrow();
            let ui = &sections[0];
            (ui.list.clone(), ui.names.clone())
        };
        (
            page,
            LiveList {
                list,
                names,
                title: title_widget,
            },
        )
    }

    /// Multi-section page with type-to-filter.
    ///
    /// `kind` labels what the rows are ("Albums", "Tracks"), so a page reached
    /// by clicking an artist or album name says plainly what it is listing.
    pub fn build_sections(
        title: &str,
        kind: &str,
        sections: Vec<Section>,
        header_action: Option<(&str, Box<dyn Fn()>)>,
    ) -> adw::NavigationPage {
        Self::build_inner(title, kind, sections, header_action, None).0
    }

    /// The section handles and the header's title widget come back alongside the
    /// page so `build_live` can keep them; every other caller drops them.
    fn build_inner(
        title: &str,
        kind: &str,
        sections: Vec<Section>,
        header_action: Option<(&str, Box<dyn Fn()>)>,
        scroll_to: Option<usize>,
    ) -> (
        adw::NavigationPage,
        Rc<RefCell<Vec<SectionUi>>>,
        adw::WindowTitle,
    ) {
        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();
        body.add_css_class("trayplay-body");

        let mut section_ui = Vec::new();
        for section in sections {
            section_ui.push(build_section(&body, section));
        }

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&body)
            .build();
        attach_drag_scroll(&scroller);

        // Filtering is client side: the list is already loaded, so this is
        // instant and costs the server nothing.
        let entry = gtk::SearchEntry::builder()
            .placeholder_text("Filter")
            .build();
        entry.set_widget_name("trayplay-filter-entry");

        let search_bar = gtk::SearchBar::builder().child(&entry).build();
        search_bar.set_widget_name("trayplay-filter");
        search_bar.connect_entry(&entry);

        let term = Rc::new(RefCell::new(String::new()));

        for ui in section_ui.iter() {
            let term = term.clone();
            let names = ui.names.clone();
            ui.list.set_filter_func(move |row| {
                let term = term.borrow();
                if term.is_empty() {
                    return true;
                }
                // Rows without a name (the empty-state placeholder) always show.
                match names.borrow().get(row.index() as usize) {
                    Some(name) => name.contains(term.as_str()),
                    None => true,
                }
            });
        }

        let section_ui = Rc::new(RefCell::new(section_ui));

        entry.connect_search_changed({
            let term = term.clone();
            let section_ui = section_ui.clone();
            move |entry| {
                let text = entry.text().trim().to_lowercase();
                *term.borrow_mut() = text.clone();

                for ui in section_ui.borrow().iter() {
                    ui.list.invalidate_filter();

                    // A section with nothing matching is hidden entirely, so a
                    // stray heading does not sit above an empty list.
                    let names = ui.names.borrow();
                    let matches = text.is_empty()
                        || names.is_empty()
                        || names.iter().any(|name| name.contains(&text));
                    ui.list.set_visible(matches);
                    if let Some(heading) = &ui.heading {
                        heading.set_visible(matches);
                    }
                }
            }
        });

        let header = adw::HeaderBar::new();
        header.set_show_start_title_buttons(false);
        header.set_show_end_title_buttons(false);
        let title_widget = adw::WindowTitle::new(title, kind);
        header.set_title_widget(Some(&title_widget));

        if let Some((label, action)) = header_action {
            let button = gtk::Button::with_label(label);
            button.set_widget_name("trayplay-page-action");
            button.add_css_class("suggested-action");
            button.connect_clicked(move |_| action());
            header.pack_end(&button);
        }

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.add_top_bar(&search_bar);
        toolbar.set_content(Some(&scroller));

        let page = adw::NavigationPage::new(&toolbar, title);
        wire_arrow_keys(&page, section_ui.clone());

        // Typing anywhere on the page opens the filter bar, so the list is
        // reachable from the keyboard without hunting for a search box.
        search_bar.set_key_capture_widget(Some(&page));

        // Queue's "open already scrolled to what's playing" request: rows
        // have no size yet at construction time (nothing has been laid out),
        // so this waits for the page to actually be shown, then for one more
        // main-loop turn on top of that (`idle_add_local_once`) for the
        // layout pass `map` triggers to have actually run - jumping a beat
        // too early would read stale (usually zero) row coordinates.
        if let Some(index) = scroll_to {
            if let Some(row) = section_ui
                .borrow()
                .first()
                .and_then(|s| s.list.row_at_index(index as i32))
            {
                let adjustment = scroller.vadjustment();
                let body = body.clone();
                page.connect_map(move |_| {
                    let adjustment = adjustment.clone();
                    let body = body.clone();
                    let row = row.clone();
                    glib::idle_add_local_once(move || {
                        let Some(point) =
                            row.compute_point(&body, &gtk::graphene::Point::new(0.0, 0.0))
                        else {
                            return;
                        };
                        let max = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
                        adjustment.set_value((point.y() as f64).clamp(adjustment.lower(), max));
                    });
                });
            }
        }

        (page, section_ui, title_widget)
    }

    /// A list page whose content can be replaced after construction, and whose
    /// filter box does not do the usual client-side contains-match.
    ///
    /// Library is the only page like this: a name-only filter over the loaded
    /// artist list cannot find an album or a track (see `Client::search`), so
    /// typing here has to reach the server instead. `on_query` gets the trimmed,
    /// lowercased text and a render callback, and decides when to call it -
    /// synchronously to fall back to the initial content on an empty query, or
    /// after a debounced round trip for anything else. That leaves debouncing
    /// and stale-response handling to the caller, since only it knows when a
    /// query has been superseded.
    pub fn build_dynamic(
        title: &str,
        kind: &str,
        initial: Vec<Section>,
        on_query: impl Fn(String, Rc<dyn Fn(Vec<Section>)>) + 'static,
    ) -> adw::NavigationPage {
        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();
        body.add_css_class("trayplay-body");

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&body)
            .build();
        attach_drag_scroll(&scroller);

        let section_ui: Rc<RefCell<Vec<SectionUi>>> = Rc::new(RefCell::new(Vec::new()));

        // Rows already reflect exactly what should be shown - the server did the
        // matching - so there is no per-keystroke filter func to wire, unlike
        // `build_sections`.
        let render: Rc<dyn Fn(Vec<Section>)> = {
            let body = body.clone();
            let section_ui = section_ui.clone();
            Rc::new(move |sections: Vec<Section>| {
                // Sibling walk, not repeated first_child: a child GTK declines to
                // remove would otherwise be returned again on the next turn and
                // the loop would never finish (a live spin, with one warning per
                // attempt).
                let mut child = body.first_child();
                while let Some(widget) = child {
                    child = widget.next_sibling();
                    body.remove(&widget);
                }
                *section_ui.borrow_mut() =
                    sections.into_iter().map(|s| build_section(&body, s)).collect();
            })
        };
        render(initial);

        let entry = gtk::SearchEntry::builder().placeholder_text("Search").build();
        entry.set_widget_name("trayplay-filter-entry");

        let search_bar = gtk::SearchBar::builder().child(&entry).build();
        search_bar.set_widget_name("trayplay-filter");
        search_bar.connect_entry(&entry);

        entry.connect_search_changed(move |entry| {
            let text = entry.text().trim().to_lowercase();
            on_query(text, render.clone());
        });

        let header = adw::HeaderBar::new();
        header.set_show_start_title_buttons(false);
        header.set_show_end_title_buttons(false);
        header.set_title_widget(Some(&adw::WindowTitle::new(title, kind)));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.add_top_bar(&search_bar);
        toolbar.set_content(Some(&scroller));

        let page = adw::NavigationPage::new(&toolbar, title);
        wire_arrow_keys(&page, section_ui);

        search_bar.set_key_capture_widget(Some(&page));
        page
    }

    /// Subtitle for an album row: release-ish context is not fetched, so the
    /// album artist is the useful line.
    pub fn album_subtitle(item: &Item) -> Option<String> {
        item.album_artist.clone()
    }

    /// Subtitle for a track row: track number and length.
    pub fn track_subtitle(item: &Item) -> Option<String> {
        let length = item.duration().map(|d| format_time(d.as_secs_f64()));
        match (item.index_number, length) {
            (Some(n), Some(len)) => Some(format!("{n}. {len}")),
            (Some(n), None) => Some(format!("{n}.")),
            (None, Some(len)) => Some(len),
            (None, None) => None,
        }
    }

    /// Subtitle for a standalone track, which has no album to sit under.
    pub fn loose_track_subtitle(item: &Item) -> Option<String> {
        item.duration().map(|d| format_time(d.as_secs_f64()))
    }

    /// Subtitle for a Library search hit: results mix artists, albums and
    /// tracks with nothing else on the row to tell them apart.
    pub fn search_subtitle(item: &Item) -> Option<String> {
        match item.kind() {
            Kind::Artist => Some("Artist".to_string()),
            Kind::Album => Some(item.album_artist.clone().unwrap_or_else(|| "Album".to_string())),
            Kind::Track => {
                let artist = item.display_artist();
                match item.album.as_deref() {
                    Some(album) if !album.is_empty() => Some(format!("{artist} · {album}")),
                    _ => Some(artist.to_string()),
                }
            }
            Kind::Other => None,
        }
    }
}

/// Arrow keys across the whole page.
///
/// Up/Down have to be handled here because a GtkListBox keeps them to itself: on
/// a page with an "Albums" list and an "Other tracks" list, the arrows stop dead
/// at the end of whichever list has focus and only Tab crosses over. This walks
/// the rows of every section as one list, skipping rows the filter has hidden.
///
/// Right activates the focused row and Left goes back, so a whole browse can be
/// done with one hand and without reaching for Enter.
fn wire_arrow_keys(page: &adw::NavigationPage, sections: Rc<RefCell<Vec<SectionUi>>>) {
    let keys = gtk::EventControllerKey::new();
    // Capture, or the focused row's own handling of Up/Down wins.
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);

    let page_weak = page.downgrade();
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        if modifiers.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK) {
            return glib::Propagation::Proceed;
        }
        let Some(page) = page_weak.upgrade() else {
            return glib::Propagation::Proceed;
        };

        let focus = page
            .root()
            .and_downcast::<gtk::Window>()
            // Qualified: GtkWindowExt and RootExt both offer focus() on a window.
            .and_then(|window| gtk::prelude::GtkWindowExt::focus(&window));

        // The filter entry owns its arrows: they move the caret. Focus lands on
        // the GtkText inside it, not on the entry itself.
        if let Some(focus) = &focus {
            if focus.is::<gtk::Text>() || focus.ancestor(gtk::Text::static_type()).is_some() {
                return glib::Propagation::Proceed;
            }
        }

        let sections = sections.borrow();
        match key {
            gdk::Key::Left => {
                if let Some(nav) = page
                    .ancestor(adw::NavigationView::static_type())
                    .and_downcast::<adw::NavigationView>()
                {
                    nav.pop();
                }
            }
            gdk::Key::Right => {
                if let Some(row) = focused_row(sections.as_slice(), focus.as_ref()) {
                    row.activate();
                }
            }
            // Shift+Enter is the row menu's *first* entry - "Add to queue" on a
            // library page, "Remove from queue" on the queue. Plain Enter is
            // left to GTK, which activates the row as usual.
            gdk::Key::Return | gdk::Key::KP_Enter
                if modifiers.contains(gdk::ModifierType::SHIFT_MASK) =>
            {
                let Some(row) = focused_row(sections.as_slice(), focus.as_ref()) else {
                    return glib::Propagation::Proceed;
                };
                let Some(action) = section_of(sections.as_slice(), &row)
                    .and_then(|section| section.menu.first())
                else {
                    return glib::Propagation::Proceed;
                };
                (action.run)(row.index() as usize);
            }
            gdk::Key::Up | gdk::Key::Down => {
                let rows = visible_rows(sections.as_slice());
                if rows.is_empty() {
                    return glib::Propagation::Stop;
                }
                let forward = key == gdk::Key::Down;
                let next = match focused_row(sections.as_slice(), focus.as_ref())
                    .and_then(|row| rows.iter().position(|candidate| *candidate == row))
                {
                    // Stops at both ends rather than wrapping: a list that jumps
                    // back to the top when you hold Down is disorienting.
                    Some(index) if forward => (index + 1).min(rows.len() - 1),
                    Some(index) => index.saturating_sub(1),
                    // Nothing focused yet, so enter the list from the near end.
                    None if forward => 0,
                    None => rows.len() - 1,
                };
                rows[next].grab_focus();
            }
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });

    page.add_controller(keys);
}

/// Rows of every section in display order, skipping those the filter hid and
/// those that cannot be activated (the "Nothing here" placeholder).
fn visible_rows(sections: &[SectionUi]) -> Vec<gtk::ListBoxRow> {
    let mut rows = Vec::new();
    for section in sections {
        if !section.list.is_visible() {
            continue;
        }
        let mut child = section.list.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            if let Some(row) = widget.downcast_ref::<gtk::ListBoxRow>() {
                if row.is_visible() && row.is_sensitive() {
                    rows.push(row.clone());
                }
            }
        }
    }
    rows
}

/// The row holding focus, whether focus is on the row itself or on something
/// inside it.
/// The section whose list `row` sits in, so a keyboard shortcut can reach that
/// section's menu - rows are direct children of their `ListBox`.
fn section_of<'a>(sections: &'a [SectionUi], row: &gtk::ListBoxRow) -> Option<&'a SectionUi> {
    let parent = row.parent()?;
    sections
        .iter()
        .find(|section| *section.list.upcast_ref::<gtk::Widget>() == parent)
}

fn focused_row(sections: &[SectionUi], focus: Option<&gtk::Widget>) -> Option<gtk::ListBoxRow> {
    let focus = focus?;
    visible_rows(sections)
        .into_iter()
        .find(|row| row.upcast_ref::<gtk::Widget>() == focus || focus.is_ancestor(row))
}

/// Drag the list with the pointer to scroll it, the way touch already does.
///
/// `kinetic-scrolling` on GtkScrolledWindow only applies to touch, so a mouse
/// gets nothing. The gesture runs in the capture phase, which is the only way it
/// sees a press that lands on a row - but it stays *unclaimed* until the pointer
/// has moved `DRAG_SCROLL_THRESHOLD`, so a plain click still reaches the row and
/// activates it. Claiming mid-drag cancels the row's own click gesture, so a
/// drag never plays anything by accident.
///
/// Releasing after a flick keeps the list moving and decelerates, which is what
/// makes dragging feel like scrolling rather than like moving a scrollbar.
/// GestureDrag reports no velocity of its own (that is GestureSwipe), so it is
/// measured from the last two updates.
fn attach_drag_scroll(scroller: &gtk::ScrolledWindow) {
    let drag = gtk::GestureDrag::new();
    drag.set_propagation_phase(gtk::PropagationPhase::Capture);

    // Where the view was when the press started; offsets from the gesture are
    // relative to that, not incremental.
    let origin = Rc::new(Cell::new(0.0));
    let adjustment = scroller.vadjustment();

    // Last sample and the speed derived from it, for the glide on release.
    let sample = Rc::new(Cell::new(None::<(Instant, f64)>));
    let velocity = Rc::new(Cell::new(0.0));
    // Held so the animation is not dropped mid-flight, which would cancel it.
    let glide: Rc<RefCell<Option<adw::TimedAnimation>>> = Rc::new(RefCell::new(None));

    drag.connect_drag_begin({
        let origin = origin.clone();
        let adjustment = adjustment.clone();
        let sample = sample.clone();
        let velocity = velocity.clone();
        let glide = glide.clone();
        move |_, _, _| {
            // Touching the list stops it: a new press should take over from
            // wherever the glide had got to.
            if let Some(animation) = glide.borrow().as_ref() {
                animation.pause();
            }
            origin.set(adjustment.value());
            sample.set(None);
            velocity.set(0.0);
        }
    });

    drag.connect_drag_update({
        let origin = origin.clone();
        let adjustment = adjustment.clone();
        let sample = sample.clone();
        let velocity = velocity.clone();
        move |gesture, _, offset| {
            if offset.abs() < DRAG_SCROLL_THRESHOLD {
                return;
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);

            let now = Instant::now();
            if let Some((then, previous)) = sample.get() {
                let elapsed = now.duration_since(then).as_secs_f64();
                // Very short intervals make the division explode, and a stale
                // speed is better than a wild one.
                if elapsed > 0.004 {
                    velocity.set((offset - previous) / elapsed);
                }
            }
            sample.set(Some((now, offset)));

            // Content follows the pointer, so dragging up scrolls down.
            adjustment.set_value((origin.get() - offset).clamp(adjustment.lower(), max_of(&adjustment)));
        }
    });

    drag.connect_drag_end({
        let adjustment = adjustment.clone();
        let velocity = velocity.clone();
        let glide = glide.clone();
        let scroller = scroller.clone();
        move |_, _, _| {
            let speed = velocity.get();
            if speed.abs() < GLIDE_MIN_VELOCITY {
                return;
            }

            let from = adjustment.value();
            let to = (from - speed * GLIDE_SECONDS).clamp(adjustment.lower(), max_of(&adjustment));
            // Already at the end of the list, or close enough that animating it
            // would only be a flicker.
            if (to - from).abs() < 1.0 {
                return;
            }

            let target = adw::PropertyAnimationTarget::new(&adjustment, "value");
            let animation =
                adw::TimedAnimation::new(&scroller, from, to, GLIDE_DURATION_MS, target);
            // Decelerating, not symmetric: the movement is already at full speed
            // when the pointer lets go.
            animation.set_easing(adw::Easing::EaseOutCubic);
            animation.play();
            *glide.borrow_mut() = Some(animation);
        }
    });

    scroller.add_controller(drag);
}

/// Largest value the adjustment can take, i.e. the bottom of the list.
fn max_of(adjustment: &gtk::Adjustment) -> f64 {
    (adjustment.upper() - adjustment.page_size()).max(adjustment.lower())
}

fn build_section(body: &gtk::Box, section: Section) -> SectionUi {
    let heading = section.heading.map(|text| {
        let label = gtk::Label::builder().label(text).xalign(0.0).build();
        label.set_widget_name("trayplay-section");
        label.add_css_class("heading");
        body.append(&label);
        label
    });

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    list.set_widget_name("trayplay-list");
    list.add_css_class("boxed-list");

    let names = fill_list(&list, &section.items, section.subtitle_of, section.on_activate);
    attach_row_menu(&list, &section.menu);

    body.append(&list);

    SectionUi {
        heading,
        list,
        names: Rc::new(RefCell::new(names)),
        menu: section.menu,
    }
}

/// Wires a right-click menu covering every row of one list.
///
/// Deliberately **not** a `PopoverMenu` over a `gio::Menu`: a menu model's items
/// reach their `GAction` through the widget hierarchy, and when that lookup finds
/// nothing the item just goes insensitive - a menu that opens, looks right and
/// does nothing when clicked, with no error anywhere. That is exactly what the
/// first version did. A plain `Popover` of buttons calls the closure directly,
/// so there is no name resolution left to fail.
///
/// The popover is built per click and parented to the *row*: a long-lived one
/// parented to the list is a child `ListBox::remove` refuses to take back, so
/// every loop clearing the list would have to know to skip it. Parented to the
/// row it is torn down with the row, and it unparents itself when it closes.
fn attach_row_menu(list: &gtk::ListBox, actions: &[RowAction]) {
    if actions.is_empty() {
        return;
    }
    let actions = actions.to_vec();

    let click = gtk::GestureClick::builder()
        .button(gdk::BUTTON_SECONDARY)
        .build();
    let list_weak = list.downgrade();
    click.connect_pressed(move |_, _, x, y| {
        let Some(list) = list_weak.upgrade() else { return };
        // Insensitive rows are the "Nothing here" placeholder, which no menu
        // entry could act on.
        let Some(row) = list.row_at_y(y as i32).filter(|row| row.is_sensitive()) else {
            return;
        };
        let index = row.index() as usize;

        let entries = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let popover = gtk::Popover::builder()
            .child(&entries)
            .has_arrow(false)
            .halign(gtk::Align::Start)
            .build();
        popover.set_widget_name("trayplay-row-menu");

        for action in &actions {
            let button = gtk::Button::with_label(&action.label);
            button.add_css_class("flat");
            // Menu entries read left-aligned; a Button centres its label.
            if let Some(label) = button.child().and_downcast::<gtk::Label>() {
                label.set_xalign(0.0);
            }

            let run = action.run.clone();
            let label = action.label.clone();
            let popover_weak = popover.downgrade();
            button.connect_clicked(move |_| {
                tracing::debug!(index, label = %label, "row menu");
                // Down first: the action can take a moment (an album's tracks
                // come from the server) and a menu left open over it looks stuck.
                if let Some(popover) = popover_weak.upgrade() {
                    popover.popdown();
                }
                run(index);
            });
            entries.append(&button);
        }

        popover.set_parent(&row);

        // Pointing coordinates must be relative to the popover's parent, and the
        // gesture reports them relative to the list.
        if let Some(point) = list.compute_point(&row, &gtk::graphene::Point::new(x as f32, y as f32))
        {
            popover.set_pointing_to(Some(&gdk::Rectangle::new(
                point.x() as i32,
                point.y() as i32,
                1,
                1,
            )));
        }

        // Nothing else holds it, so it goes when it closes rather than lingering
        // on the row until the row itself is replaced. The counter is what stops
        // auto-hide from dismissing the popup while the menu has the focus.
        popover.connect_closed(|popover| {
            super::menu_closed();
            if popover.parent().is_some() {
                popover.unparent();
            }
        });
        super::menu_opened();
        popover.popup();
    });
    list.add_controller(click);
}

/// Appends one row per item and returns their lowercased names for the filter.
///
/// Shared with `LiveList::replace`, which is the reason this is not inlined in
/// `build_section`: rows built at construction and rows built to replace them
/// must be identical in every respect, or a rebuilt queue page would quietly
/// lose its CSS class or its markup escaping.
fn fill_list(
    list: &gtk::ListBox,
    items: &[Item],
    subtitle_of: impl Fn(&Item) -> Option<String> + 'static,
    on_activate: impl Fn(usize) + 'static,
) -> Vec<String> {
    if items.is_empty() {
        let empty = adw::ActionRow::builder().title("Nothing here").build();
        empty.set_sensitive(false);
        list.append(&empty);
    }

    let on_activate = Rc::new(on_activate);
    for (index, item) in items.iter().enumerate() {
        let row = adw::ActionRow::builder()
            .title(markup_escape(&item.name))
            .activatable(true)
            .build();
        row.add_css_class("trayplay-row");
        if let Some(subtitle) = subtitle_of(item) {
            row.set_subtitle(&markup_escape(&subtitle));
        }

        let cb = on_activate.clone();
        row.connect_activated(move |_| cb(index));
        list.append(&row);
    }

    items.iter().map(|item| item.name.to_lowercase()).collect()
}

/// ActionRow titles are parsed as Pango markup, so raw names with an ampersand
/// would break rendering.
pub fn markup_escape(text: &str) -> String {
    glib::markup_escape_text(text).to_string()
}
