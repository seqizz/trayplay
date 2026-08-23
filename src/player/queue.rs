use serde::{Deserialize, Serialize};

use crate::jellyfin::models::Item;

/// How the current queue was built. Random queues refill themselves as they run
/// down; explicit ones (an album, an artist) simply end.
///
/// Persisted with the queue, so a restored random queue keeps refilling instead
/// of ending where the last session happened to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Random,
    Explicit,
}

#[derive(Debug)]
pub struct Queue {
    items: Vec<Item>,
    cursor: usize,
    pub mode: Mode,
}

impl Queue {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            cursor: 0,
            mode: Mode::Explicit,
        }
    }

    pub fn replace(&mut self, items: Vec<Item>, start: usize, mode: Mode) {
        self.items = items;
        self.cursor = start.min(self.items.len().saturating_sub(1));
        self.mode = mode;
    }

    pub fn append(&mut self, items: Vec<Item>) {
        self.items.extend(items);
    }

    /// Inserts directly after `index`, which is how "Play next" puts tracks
    /// between what is playing and the rest of the queue.
    pub fn insert_after(&mut self, index: usize, items: Vec<Item>) {
        let at = (index + 1).min(self.items.len());
        self.items.splice(at..at, items);
    }

    /// Drops the track at the cursor, which [`Queue::remove`] refuses to touch.
    ///
    /// Only for a track that cannot be played at all - the caller is responsible
    /// for the sink, which is still holding it. Returns whether the cursor now
    /// points at a track that *followed* it: removing shifts the tail down, so
    /// the cursor lands on the successor for free. False means there was none,
    /// and the cursor has been clamped back into range.
    pub fn remove_current(&mut self) -> bool {
        if self.cursor >= self.items.len() {
            return false;
        }
        let had_successor = self.cursor + 1 < self.items.len();
        self.items.remove(self.cursor);
        if !had_successor {
            self.cursor = self.items.len().saturating_sub(1);
        }
        had_successor
    }

    /// Drops one track. False when there is nothing at `index`, or when it is
    /// the track playing right now - removing that would leave the sink playing
    /// something the queue no longer contains.
    ///
    /// The cursor follows its own track rather than staying on a number, so
    /// removing something above it shifts it down.
    pub fn remove(&mut self, index: usize) -> bool {
        if index >= self.items.len() || index == self.cursor {
            return false;
        }
        self.items.remove(index);
        if index < self.cursor {
            self.cursor -= 1;
        }
        true
    }

    pub fn current(&self) -> Option<&Item> {
        self.items.get(self.cursor)
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The track the sink should cross into, `wrap` being repeat-all as in
    /// [`Queue::advance`] - the two have to agree, or the queue cursor and what
    /// is actually being heard part ways at the end of a queue.
    pub fn peek_next(&self, wrap: bool) -> Option<&Item> {
        match self.items.get(self.cursor + 1) {
            Some(item) => Some(item),
            None if wrap => self.items.first(),
            None => None,
        }
    }

    /// Advances one track. None means the queue is exhausted.
    ///
    /// `wrap` is repeat-all: the queue starts over instead of running out. It is
    /// a parameter rather than state on the queue because the player owns the
    /// repeat setting - MPRIS can change it as well as the button.
    pub fn advance(&mut self, wrap: bool) -> Option<&Item> {
        if self.cursor + 1 >= self.items.len() {
            if !wrap || self.items.is_empty() {
                return None;
            }
            self.cursor = 0;
            return self.current();
        }
        self.cursor += 1;
        self.current()
    }

    /// Steps back one track, staying put at the start of the queue.
    pub fn back(&mut self) -> Option<&Item> {
        self.cursor = self.cursor.saturating_sub(1);
        self.current()
    }

    /// True when fewer than `slack` tracks remain, i.e. time to refill.
    pub fn running_low(&self, slack: usize) -> bool {
        self.items.len().saturating_sub(self.cursor) <= slack
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}
