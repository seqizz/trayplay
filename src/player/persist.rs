//! The queue as a state file, so a restart resumes where the last session left
//! off instead of on a fresh random queue.
//!
//! Whole `Item`s are stored, not ids: rebuilding them would be one HTTP call per
//! track and would leave the popup empty whenever the server is unreachable,
//! which is exactly when having the old queue still matters. JSON rather than
//! TOML because `Item` is a nested structure that arrived as JSON to begin with.
//!
//! Machine-owned, next to `settings.toml` under `$XDG_STATE_HOME` - never in
//! `config.toml`, which is hand-edited.

use std::fs;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::queue_state_path;
use crate::jellyfin::models::Item;

use super::queue::{Mode, Queue};

/// Bounds the state file, so a session that has been running for days does not
/// end up rewriting megabytes on every track change.
///
/// Generous on purpose: what gets saved has to include everything the user
/// queued by hand, and those go to the end of a queue that a random refill has
/// already grown. Losing them would be the one truncation nobody would forgive.
const MAX_ITEMS: usize = 2000;

/// How much already-played queue is kept when the cap bites. Enough for Previous
/// to still work a few times after a restart; the rest of the history is of no
/// use to anyone.
const KEPT_HISTORY: usize = 50;

#[derive(Debug, Serialize, Deserialize)]
struct Saved {
    items: Vec<Item>,
    cursor: usize,
    mode: Mode,
}

/// Writes the queue out, atomically.
///
/// Called on every track change, so a crash or a kill -9 mid-write must not
/// leave a half-written file behind: it goes to a sibling `.tmp` and is renamed
/// over the target, which is atomic within one filesystem.
///
/// Deliberately synchronous even though the caller is the async player actor:
/// this is a few hundred KB at most, once per track, and `tokio::spawn`ing it
/// would let two writes race and land out of order.
pub fn save(queue: &Queue) -> Result<()> {
    let path = queue_state_path()?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }

    let items = queue.items();
    let cursor = queue.cursor();

    // Windowed around the cursor, not truncated from the front. Keeping the first
    // MAX_ITEMS would throw away the *future* of a long random queue - including
    // anything just added by hand, which lands at the end - and clamp the cursor
    // into the middle of the session's history, so a restart would resume
    // somewhere the user had already been.
    // The second term keeps the window as far back as it can while still reaching
    // the end of the queue, so a short future is padded with extra history rather
    // than saving less than the cap allows.
    let start = cursor
        .saturating_sub(KEPT_HISTORY)
        .min(items.len().saturating_sub(MAX_ITEMS));
    let end = (start + MAX_ITEMS).min(items.len());

    let saved = Saved {
        items: items[start..end].to_vec(),
        cursor: cursor.saturating_sub(start),
        mode: queue.mode,
    };

    let raw = serde_json::to_vec(&saved).context("serialising queue")?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, raw).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("renaming onto {}", path.display()))
}

/// Reads the queue back. None when there is nothing usable to restore.
///
/// Never an error, for the same reason `Settings::load` is not: a corrupt or
/// stale state file must not stop the player from starting. The worst case is
/// starting with an empty queue, which is what happens on a fresh install.
pub fn load() -> Option<(Vec<Item>, usize, Mode)> {
    let path = queue_state_path().ok()?;
    let raw = fs::read(&path).ok()?;
    match serde_json::from_slice::<Saved>(&raw) {
        Ok(saved) if !saved.items.is_empty() => {
            let cursor = saved.cursor.min(saved.items.len() - 1);
            Some((saved.items, cursor, saved.mode))
        }
        Ok(_) => None,
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "ignoring unreadable queue state");
            None
        }
    }
}
