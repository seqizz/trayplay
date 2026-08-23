//! The queue as a list page.
//!
//! A row plays from that point, which is the same thing a track row in an album
//! does, and its menu removes it. Reordering is still out - it would need the
//! player to own a move command and the page somewhere to move a track *to*.
//!
//! Live while it is open, though: the page subscribes to the player's events and
//! refreshes itself on every track change, so the "Now playing" marker follows
//! the music and tracks a random refill appended show up. It used to be a single
//! snapshot taken when the page was pushed, which went stale the moment the track
//! advanced.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::jellyfin::models::Item;
use crate::player::{Command, Event, PlayerHandle};

use super::browse::{ListPage, LiveList, RowAction};
use super::nowplaying::format_time;
use super::{bridge_events, on_runtime, Session};

pub fn push(nav: &adw::NavigationView, session: &Session) {
    let nav_weak = nav.downgrade();
    let session = session.clone();
    let player = session.player.clone();

    on_runtime(
        &session.browser.runtime(),
        async move { player.snapshot().await },
        move |snapshot| {
            let Some(nav) = nav_weak.upgrade() else { return };
            let Some(snapshot) = snapshot else {
                tracing::warn!("no reply to the queue snapshot request");
                return;
            };

            let current = current_id(&snapshot.items, snapshot.cursor);
            // Shared with the row menu, which must act on the queue as it is
            // now, not as it was when the page was built: `LiveList::replace`
            // swaps the rows but leaves the menu (it belongs to the list, not to
            // a row), so a captured id list would go stale on the first refresh.
            let ids = Rc::new(RefCell::new(ids_of(&snapshot.items)));

            let (page, live) = ListPage::build_live(
                "Queue",
                &kind_label(snapshot.items.len()),
                subtitle_fn(current),
                snapshot.items.clone(),
                activate_fn(&nav, session.player.clone(), snapshot.items),
                remove_actions(session.player.clone(), ids.clone()),
                snapshot.cursor,
            );
            nav.push(&page);
            follow_player(&page, &nav, &session, live, ids);
        },
    );
}

/// Keeps the page in step with the player for as long as it is on screen.
///
/// Its own event bridge rather than a share of the popup's: `bridge_events` is a
/// fresh broadcast subscription, the receiver lives and dies with this page, and
/// the task feeding it on the runtime stops by itself once that receiver is
/// dropped. Nothing has to be unsubscribed on pop.
fn follow_player(
    page: &adw::NavigationPage,
    nav: &adw::NavigationView,
    session: &Session,
    live: LiveList,
    ids: Rc<RefCell<Vec<String>>>,
) {
    let events = bridge_events(&session.browser.runtime(), &session.player);
    let page_weak = page.downgrade();
    let nav_weak = nav.downgrade();
    let session = session.clone();
    let live = Rc::new(live);

    glib::spawn_future_local(async move {
        while let Ok(event) = events.recv().await {
            // Position fires four times a second and moves nothing on this page.
            // A track change moves the marker (and reveals what a random refill
            // appended); a queue change is an enqueue or a removal.
            if !matches!(event, Event::TrackChanged(_) | Event::QueueChanged) {
                continue;
            }
            // Popped, or the whole popup went away: there is nothing left to
            // refresh, and dropping the receiver ends the bridge task too.
            if page_weak.upgrade().is_none() {
                break;
            }
            refresh(&session, nav_weak.clone(), live.clone(), ids.clone());
        }
    });
}

/// Re-reads the queue and puts it on screen.
///
/// The event carries the new track, but not the queue around it, so this asks:
/// the cursor may have jumped rather than stepped (Previous, a row activation)
/// and a refill may have appended behind it.
fn refresh(
    session: &Session,
    nav_weak: glib::WeakRef<adw::NavigationView>,
    live: Rc<LiveList>,
    ids: Rc<RefCell<Vec<String>>>,
) {
    let player = session.player.clone();
    let session = session.clone();
    on_runtime(
        &session.browser.runtime(),
        async move { player.snapshot().await },
        move |snapshot| {
            let Some(snapshot) = snapshot else { return };
            let Some(nav) = nav_weak.upgrade() else { return };

            let current = current_id(&snapshot.items, snapshot.cursor);
            let new_ids = ids_of(&snapshot.items);
            // Bound on its own line: keeping the borrow alive into the else
            // branch would collide with the borrow_mut there.
            let same_queue = *ids.borrow() == new_ids;

            if same_queue {
                // Only the marker moved, so the rows stay and keep their focus.
                live.relabel(&snapshot.items, subtitle_fn(current));
                return;
            }

            *ids.borrow_mut() = new_ids;
            live.set_kind(&kind_label(snapshot.items.len()));
            // Row activation closures capture their index, so a changed queue
            // needs new ones - the old index would play the wrong track.
            live.replace(
                snapshot.items.clone(),
                subtitle_fn(current),
                activate_fn(&nav, session.player.clone(), snapshot.items),
            );
        },
    );
}

fn current_id(items: &[Item], cursor: usize) -> Option<String> {
    items.get(cursor).map(|item| item.id.clone())
}

fn ids_of(items: &[Item]) -> Vec<String> {
    items.iter().map(|item| item.id.clone()).collect()
}

fn kind_label(count: usize) -> String {
    if count == 1 {
        "1 track".to_string()
    } else {
        format!("{count} tracks")
    }
}

/// Row subtitle: who it is by, how long it runs, and whether it is the one
/// playing.
///
/// Compared by id rather than by index, because the closure only sees the item.
/// A queue holding the same track twice marks both, which is a fair price for
/// not threading indices through.
fn subtitle_fn(current: Option<String>) -> impl Fn(&Item) -> Option<String> {
    move |item: &Item| {
        let length = item.duration().map(|d| format_time(d.as_secs_f64()));
        let artist = item.display_artists();
        let base = match length {
            Some(length) => format!("{artist} · {length}"),
            None => artist,
        };
        if current.as_deref() == Some(item.id.as_str()) {
            Some(format!("Now playing · {base}"))
        } else {
            Some(base)
        }
    }
}

/// The queue page's row menu: one entry, which Shift+Enter therefore also runs.
///
/// The id goes along with the index so the player can tell a stale removal from
/// a real one - `ids` is the list this page last rendered, and the queue may have
/// moved on since. The player refuses to remove the track that is playing (and
/// one already handed to the sink for a gapless transition), which surfaces as a
/// toast rather than being silently ignored.
fn remove_actions(player: PlayerHandle, ids: Rc<RefCell<Vec<String>>>) -> Vec<RowAction> {
    vec![RowAction::new("Remove from queue", move |index| {
        let Some(id) = ids.borrow().get(index).cloned() else {
            return;
        };
        player.send(Command::Remove { index, id });
    })]
}

/// Activating a row plays the queue from there - the same rule as everywhere
/// else, so it also returns to now-playing rather than leaving the list up.
fn activate_fn(
    nav: &adw::NavigationView,
    player: PlayerHandle,
    items: Vec<Item>,
) -> impl Fn(usize) + 'static {
    let nav_weak = nav.downgrade();
    move |index| {
        player.send(Command::PlayItems {
            items: items.clone(),
            start: index,
        });
        if let Some(nav) = nav_weak.upgrade() {
            nav.pop_to_tag("now-playing");
        }
    }
}
