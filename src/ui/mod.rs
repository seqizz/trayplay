pub mod artists;
pub mod artwork;
pub mod browse;
pub mod nowplaying;
pub mod popup;
pub mod queue;
pub mod settings;
pub mod x11;

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use gtk::glib;

use crate::jellyfin::models::Item;
use crate::jellyfin::Client;
use crate::player::{Event, PlayerHandle};

pub use popup::Popup;

thread_local! {
    /// How many row menus are open right now.
    ///
    /// A popover is its own surface, so opening one takes focus off the popup
    /// window and auto-hide would dismiss the whole thing under the menu the user
    /// just asked for. `wire_dismiss` consults this instead of the window's active
    /// state alone. A counter rather than a flag because a second menu can open
    /// while the first is still closing.
    ///
    /// One popup, one GTK thread, so a thread local is the whole of the sharing
    /// this needs - the alternative is threading a cell from `Popup::new` through
    /// every page down to `attach_row_menu`.
    static MENUS_OPEN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub fn menu_opened() {
    MENUS_OPEN.with(|open| open.set(open.get() + 1));
}

pub fn menu_closed() {
    MENUS_OPEN.with(|open| open.set(open.get().saturating_sub(1)));
}

/// True while a row menu is on screen, i.e. while losing window focus is
/// expected rather than a reason to hide.
pub fn menu_is_open() -> bool {
    MENUS_OPEN.with(|open| open.get() > 0)
}

/// Runs a future on the tokio runtime and delivers the result back on the GTK
/// main thread.
///
/// GTK callbacks cannot await reqwest futures directly: glib's executor has no
/// tokio reactor, so the I/O would never be driven. The result crosses back
/// over an async_channel, which lets the callback stay non-Send and touch
/// widgets.
pub fn on_runtime<T, F, C>(rt: &tokio::runtime::Handle, fut: F, cb: C)
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
    C: FnOnce(T) + 'static,
{
    let (tx, rx) = async_channel::bounded(1);
    rt.spawn(async move {
        let _ = tx.send(fut.await).await;
    });
    glib::spawn_future_local(async move {
        if let Ok(value) = rx.recv().await {
            cb(value);
        }
    });
}

/// One cached library query.
///
/// Keyed by what was asked for rather than by URL: `artist_page` and
/// `artist_tracks` ask the server the same question, so they must share an entry
/// or the second one would refetch what the first already has.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Query {
    Artists,
    ArtistAlbums(String),
    ArtistTracks(String),
    AlbumTracks(String),
}

/// Most distinct queries kept at once.
///
/// A browse session touches a handful of artists; this only exists so that
/// walking a large library for an hour cannot grow the map without bound. The
/// oldest entries go first, which for a browse is also the least interesting.
const MAX_CACHED_QUERIES: usize = 64;

/// Cached results by query, each stamped with when it landed.
type Entries = HashMap<Query, (Instant, Vec<Item>)>;

/// Results of library queries, reused for `ttl` after they land.
///
/// `Rc`/`RefCell` rather than a lock: there is one `Browser`, cloned around the
/// GTK thread, and every read and write of this happens in a GTK callback.
#[derive(Clone)]
struct QueryCache {
    entries: Rc<RefCell<Entries>>,
    ttl: Duration,
}

impl QueryCache {
    fn get(&self, query: &Query) -> Option<Vec<Item>> {
        if self.ttl.is_zero() {
            return None;
        }
        let entries = self.entries.borrow();
        let (at, items) = entries.get(query)?;
        (at.elapsed() < self.ttl).then(|| items.clone())
    }

    fn put(&self, query: Query, items: &[Item]) {
        if self.ttl.is_zero() {
            return;
        }
        let mut entries = self.entries.borrow_mut();
        entries.retain(|_, (at, _)| at.elapsed() < self.ttl);
        while entries.len() >= MAX_CACHED_QUERIES {
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(query, _)| query.clone())
            else {
                break;
            };
            entries.remove(&oldest);
        }
        entries.insert(query, (Instant::now(), items.to_vec()));
    }
}

/// Library queries for the UI, with results delivered on the GTK thread.
#[derive(Clone)]
pub struct Browser {
    rt: tokio::runtime::Handle,
    client: Arc<Client>,
    /// Short-lived reuse of query results, so walking back up a drill-down does
    /// not re-ask the server for the page that was on screen a moment ago.
    /// Search and cover art are deliberately not in it: a search key is a
    /// keystroke-shaped thing that would never be hit twice, and the art is one
    /// request per track change rather than per page.
    cache: QueryCache,
}

impl Browser {
    pub fn new(rt: tokio::runtime::Handle, client: Arc<Client>, cache_ttl: Duration) -> Self {
        Self {
            rt,
            client,
            cache: QueryCache {
                entries: Rc::default(),
                ttl: cache_ttl,
            },
        }
    }

    /// For callers that need to run something on the runtime themselves - the
    /// queue page asks the *player*, not the server, so it has no query here.
    pub fn runtime(&self) -> tokio::runtime::Handle {
        self.rt.clone()
    }

    pub fn artists(&self, cb: impl FnOnce(Result<Vec<Item>>) + 'static) {
        self.cached(Query::Artists, cb, |client| async move {
            client.artists().await
        });
    }

    /// Runs `fetch` unless the cache can answer, and records what comes back.
    ///
    /// A hit still goes through `glib::idle_add_local_once` rather than calling
    /// `cb` here: the callers push a page and then fill it in from this callback,
    /// and filling it before the push had returned would be a reentrant surprise
    /// for something that is asynchronous everywhere else.
    fn cached<F, Fut>(&self, query: Query, cb: impl FnOnce(Result<Vec<Item>>) + 'static, fetch: F)
    where
        F: FnOnce(Arc<Client>) -> Fut,
        Fut: Future<Output = Result<Vec<Item>>> + Send + 'static,
    {
        if let Some(items) = self.cache.get(&query) {
            glib::idle_add_local_once(move || cb(Ok(items)));
            return;
        }
        let cache = self.cache.clone();
        on_runtime(&self.rt, fetch(self.client.clone()), move |result| {
            if let Ok(items) = &result {
                cache.put(query, items);
            }
            cb(result);
        });
    }

    /// Everything an artist page needs: their albums and *all* their tracks.
    /// Both queries run concurrently on the runtime, so the page still costs one
    /// round trip of latency.
    ///
    /// The full track list, not just the album-less ones the page shows under
    /// "Other tracks": playing one of those shuffles the artist's whole
    /// catalogue behind it, so the page needs to have it to hand.
    #[allow(clippy::type_complexity)]
    pub fn artist_page(
        &self,
        artist_id: &str,
        cb: impl FnOnce(Result<(Vec<Item>, Vec<Item>)>) + 'static,
    ) {
        let albums_key = Query::ArtistAlbums(artist_id.to_string());
        let tracks_key = Query::ArtistTracks(artist_id.to_string());

        // Both halves or neither: a partial hit would still cost the round trip
        // that the caller is waiting on, so there is nothing to save by
        // fetching only one of them.
        if let (Some(albums), Some(tracks)) =
            (self.cache.get(&albums_key), self.cache.get(&tracks_key))
        {
            glib::idle_add_local_once(move || cb(Ok((albums, tracks))));
            return;
        }

        let client = self.client.clone();
        let id = artist_id.to_string();
        let cache = self.cache.clone();
        on_runtime(
            &self.rt,
            async move { tokio::try_join!(client.artist_albums(&id), client.artist_tracks(&id)) },
            move |result| {
                if let Ok((albums, tracks)) = &result {
                    cache.put(albums_key, albums);
                    // Shared with `artist_tracks`, which asks the server exactly
                    // this - a search hit resolving its artist's catalogue right
                    // after their page was open should not refetch it.
                    cache.put(tracks_key, tracks);
                }
                cb(result);
            },
        );
    }

    /// Mixed-type search backing the Library page's filter.
    pub fn search(&self, term: &str, limit: u32, cb: impl FnOnce(Result<Vec<Item>>) + 'static) {
        let client = self.client.clone();
        let term = term.to_string();
        on_runtime(&self.rt, async move { client.search(&term, limit).await }, cb);
    }

    pub fn album_tracks(&self, album_id: &str, cb: impl FnOnce(Result<Vec<Item>>) + 'static) {
        let id = album_id.to_string();
        self.cached(Query::AlbumTracks(id.clone()), cb, |client| async move {
            client.album_tracks(&id).await
        });
    }

    /// Every track by an artist, regardless of album - the fallback for a
    /// Library search hit with no album of its own to queue.
    pub fn artist_tracks(&self, artist_id: &str, cb: impl FnOnce(Result<Vec<Item>>) + 'static) {
        let id = artist_id.to_string();
        self.cached(Query::ArtistTracks(id.clone()), cb, |client| async move {
            client.artist_tracks(&id).await
        });
    }

    /// Fetches cover art. Bytes rather than a texture because the conversion
    /// has to happen on the GTK thread.
    pub fn cover(&self, item: &Item, height: u32, cb: impl FnOnce(Option<Vec<u8>>) + 'static) {
        let Some((id, tag)) = item.cover_source() else {
            cb(None);
            return;
        };
        let url = self.client.image_url(id, tag, height);
        let client = self.client.clone();
        on_runtime(
            &self.rt,
            async move { client.fetch_bytes(&url).await.ok() },
            cb,
        );
    }
}

/// Everything the popup needs when there is a working session.
#[derive(Clone)]
pub struct Session {
    pub player: PlayerHandle,
    pub browser: Browser,
}

/// Forwards player events onto the GTK main thread.
///
/// The broadcast receiver cannot be awaited from glib's executor, so a runtime
/// task relays into an async_channel instead.
pub fn bridge_events(
    rt: &tokio::runtime::Handle,
    player: &PlayerHandle,
) -> async_channel::Receiver<Event> {
    let (tx, rx) = async_channel::unbounded();
    let mut events = player.subscribe();
    rt.spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
                // Falling behind only costs intermediate positions.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    rx
}
