use std::collections::HashSet;
use std::sync::Arc;

use serde::Serialize;

use netodia_core::entities::track::TrackSummary;

use super::types::{PersistableQueue, RepeatMode};

/// Result of [`QueueState::prune_missing`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PruneOutcome {
    /// Number of distinct entries removed from `tracks`.
    pub removed: usize,
    /// True if the `play_order[current_index]` entry — the thing the player
    /// believes is currently playing — was in `ids_to_remove`. Caller uses
    /// this to decide whether to auto-skip to the next track.
    pub current_was_removed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueState {
    /// Canonical track data — each track stored exactly once.
    pub tracks: Vec<Arc<TrackSummary>>,
    /// Current playback order — indices into `tracks`.
    pub play_order: Vec<usize>,
    /// Original insertion order — indices into `tracks`. Used to restore on unshuffle.
    pub(crate) original_order: Vec<usize>,
    /// Index into `play_order` of the currently-playing track, or `None`
    /// when nothing is playing. The on-disk and Slint representations use a
    /// signed integer with `-1` as the no-track sentinel; conversion happens
    /// at those boundaries (`to_persistable`, `to_view_model`).
    pub current_index: Option<usize>,
    pub shuffle_enabled: bool,
    pub repeat_mode: RepeatMode,
    /// Monotonic counter incremented on every queue mutation.
    /// Used by `with_state_emit` to detect whether the queue changed.
    pub version: u64,
}

impl Default for QueueState {
    fn default() -> Self {
        Self {
            tracks: Vec::new(),
            play_order: Vec::new(),
            original_order: Vec::new(),
            current_index: None,
            shuffle_enabled: false,
            repeat_mode: RepeatMode::Off,
            version: 0,
        }
    }
}

/// Encode `current_index` as the on-disk / Slint i32 representation
/// (`-1` for `None`, otherwise the index saturated to `i32::MAX`).
pub fn current_index_to_i32(idx: Option<usize>) -> i32 {
    idx.map_or(-1, |i| i32::try_from(i).unwrap_or(i32::MAX))
}

/// Decode an i32 (`-1` sentinel, otherwise a non-negative index) into the
/// internal `Option<usize>` form.
pub fn current_index_from_i32(idx: i32) -> Option<usize> {
    usize::try_from(idx).ok()
}

impl QueueState {
    /// Number of tracks in the current play order.
    pub fn len(&self) -> usize {
        self.play_order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.play_order.is_empty()
    }

    /// Resolve an index in `play_order` to the actual track.
    fn track_at(&self, play_index: usize) -> Option<&Arc<TrackSummary>> {
        self.play_order.get(play_index).and_then(|&ti| self.tracks.get(ti))
    }

    pub fn add_tracks(&mut self, new_tracks: Vec<Arc<TrackSummary>>) {
        let start = self.tracks.len();
        let count = new_tracks.len();
        self.tracks.extend(new_tracks);
        let range = start..start + count;
        self.play_order.extend(range.clone());
        self.original_order.extend(range);
        self.version += 1;
    }

    pub fn insert_next(&mut self, track: Arc<TrackSummary>) {
        let track_idx = self.tracks.len();
        self.tracks.push(track);
        self.original_order.push(track_idx);

        let insert_pos = self.current_index.map_or(0, |ci| ci + 1).min(self.play_order.len());
        self.play_order.insert(insert_pos, track_idx);
        self.version += 1;
    }

    pub fn remove_at(&mut self, index: usize) -> bool {
        if index >= self.play_order.len() {
            return false;
        }

        let removed_track_idx = self.play_order.remove(index);

        self.tracks.remove(removed_track_idx);
        self.original_order.retain(|&i| i != removed_track_idx);
        for i in self.play_order.iter_mut().chain(&mut self.original_order) {
            if *i > removed_track_idx {
                *i -= 1;
            }
        }

        // Adjust current_index
        if let Some(ci) = self.current_index {
            if index < ci {
                self.current_index = Some(ci - 1);
            } else if index == ci {
                if self.play_order.is_empty() {
                    self.current_index = None;
                } else if ci >= self.play_order.len() {
                    self.current_index = Some(self.play_order.len() - 1);
                }
            }
        }

        self.version += 1;
        true
    }

    /// Remove multiple play-order indices at once in O(n) time.
    pub fn remove_batch(&mut self, indices: &[usize]) {
        if indices.is_empty() {
            return;
        }

        let removed: HashSet<usize> =
            indices.iter().filter_map(|&i| self.play_order.get(i).copied()).collect();
        self.remove_tracks(|index, _| removed.contains(&index));
    }

    pub fn move_track(&mut self, from: usize, to: usize) -> bool {
        if from >= self.play_order.len() || to >= self.play_order.len() {
            return false;
        }

        let idx = self.play_order.remove(from);
        self.play_order.insert(to, idx);

        if let Some(ci) = self.current_index {
            self.current_index = Some(if from == ci {
                to
            } else if from < ci && to >= ci {
                ci - 1
            } else if from > ci && to <= ci {
                ci + 1
            } else {
                ci
            });
        }
        self.version += 1;
        true
    }

    pub fn clear(&mut self) {
        self.tracks.clear();
        self.play_order.clear();
        self.original_order.clear();
        self.current_index = None;
        self.version += 1;
    }

    /// Jump to a specific index in the play order.
    pub fn skip_to_index(&mut self, index: usize) -> Option<&Arc<TrackSummary>> {
        if index >= self.len() {
            return None;
        }
        self.current_index = Some(index);
        self.version += 1;
        self.track_at(index)
    }

    pub fn get_current(&self) -> Option<&Arc<TrackSummary>> {
        let ci = self.current_index?;
        if ci >= self.len() {
            return None;
        }
        self.track_at(ci)
    }

    pub fn advance(&mut self) -> Option<&Arc<TrackSummary>> {
        if self.play_order.is_empty() {
            return None;
        }

        self.version += 1;
        let len = self.len();
        let next_from = |ci: Option<usize>| ci.map_or(0, |i| i + 1);
        match self.repeat_mode {
            RepeatMode::One => self.get_current(),
            RepeatMode::All => {
                let next = next_from(self.current_index) % len;
                self.current_index = Some(next);
                self.get_current()
            }
            RepeatMode::Off => {
                let next_index = next_from(self.current_index);
                if next_index >= len {
                    return None;
                }
                self.current_index = Some(next_index);
                self.get_current()
            }
        }
    }

    /// User-initiated skip — advances to next track.
    /// `RepeatMode::One` wraps like `RepeatMode::All`: manual navigation
    /// walks the queue in a loop. Only `Off` stops at the end.
    pub fn advance_skip(&mut self) -> Option<&Arc<TrackSummary>> {
        if self.play_order.is_empty() {
            return None;
        }

        self.version += 1;
        let len = self.len();
        let next_from = |ci: Option<usize>| ci.map_or(0, |i| i + 1);
        match self.repeat_mode {
            RepeatMode::All | RepeatMode::One => {
                let next = next_from(self.current_index) % len;
                self.current_index = Some(next);
                self.get_current()
            }
            RepeatMode::Off => {
                let next_index = next_from(self.current_index);
                if next_index >= len {
                    return None;
                }
                self.current_index = Some(next_index);
                self.get_current()
            }
        }
    }

    /// Move past an entry that could not be played. Unlike a user-initiated skip, failure
    /// recovery must not get trapped on the same item when Repeat One is selected. It still
    /// respects whether the queue wraps, and callers bound attempts to one queue lap.
    pub fn advance_after_failure(&mut self) -> Option<&Arc<TrackSummary>> {
        if self.play_order.is_empty() {
            return None;
        }

        let next = self.current_index.map_or(0, |index| index + 1);
        let next = match self.repeat_mode {
            RepeatMode::All | RepeatMode::One => next % self.len(),
            RepeatMode::Off if next < self.len() => next,
            RepeatMode::Off => return None,
        };
        self.version += 1;
        self.current_index = Some(next);
        self.get_current()
    }

    pub fn previous(&mut self) -> Option<&Arc<TrackSummary>> {
        if self.play_order.is_empty() {
            return None;
        }

        self.version += 1;
        if self.repeat_mode.wraps() {
            // Wraps. None and Some(0) both go to the last track.
            let next = self.current_index.filter(|&ci| ci > 0).map_or(self.len() - 1, |ci| ci - 1);
            self.current_index = Some(next);
        } else if let Some(ci) = self.current_index
            && ci > 0
        {
            self.current_index = Some(ci - 1);
        }
        // Off with None or Some(0): no change.
        self.get_current()
    }

    pub fn peek_next(&self) -> Option<&Arc<TrackSummary>> {
        if self.play_order.is_empty() {
            return None;
        }

        let len = self.len();
        let next_from = |ci: Option<usize>| ci.map_or(0, |i| i + 1);
        match self.repeat_mode {
            RepeatMode::One => self.get_current(),
            RepeatMode::All => self.track_at(next_from(self.current_index) % len),
            RepeatMode::Off => {
                let next = next_from(self.current_index);
                if next >= len { None } else { self.track_at(next) }
            }
        }
    }

    /// Begin shuffle: returns (`track_count`, `current_track_index`) for the shell to generate random indices.
    pub fn begin_shuffle(&self) -> (usize, Option<usize>) {
        (self.play_order.len(), self.current_index)
    }

    /// Apply a shuffled order provided by the shell.
    /// `indices` is a permutation of `0..play_order.len()` with the current track at index 0.
    /// These are indices into `play_order`, not into `tracks`.
    pub fn apply_shuffle_order(&mut self, indices: &[usize]) {
        // Remap: new play_order[i] = old play_order[indices[i]]
        let new_play_order: Vec<usize> =
            indices.iter().filter_map(|&i| self.play_order.get(i).copied()).collect();
        self.play_order = new_play_order;
        self.current_index = Some(0);
        self.shuffle_enabled = true;
        self.version += 1;
    }

    /// In-place version of [`Self::apply_shuffle_order`] that shuffles
    /// `play_order` directly using the caller-provided RNG. Avoids the
    /// double `Vec<usize>` allocation that the shell-driven path required
    /// (one for the indices in the caller, one for the remapped play
    /// order here). If `anchor_to_current` is true and the queue has a
    /// current track, that track is swapped to the front of the shuffled
    /// order so playback carries on from it.
    pub fn shuffle_play_order_in_place<R: rand::Rng + ?Sized>(
        &mut self,
        rng: &mut R,
        anchor_to_current: bool,
    ) {
        use rand::seq::SliceRandom;
        if self.play_order.is_empty() {
            return;
        }
        let pinned_track_idx = if anchor_to_current {
            self.current_index.and_then(|ci| self.play_order.get(ci).copied())
        } else {
            None
        };
        self.play_order.shuffle(rng);
        if let Some(ti) = pinned_track_idx
            && let Some(new_pos) = self.play_order.iter().position(|&i| i == ti)
        {
            self.play_order.swap(0, new_pos);
        }
        self.current_index = Some(0);
        self.shuffle_enabled = true;
        self.version += 1;
    }

    /// Restore original track order (unshuffle).
    /// Only clones `Vec<usize>` instead of `Vec<TrackSummary>`.
    pub fn unshuffle(&mut self) {
        let current_track_id = self.get_current().map(|t| t.id);
        self.play_order.clone_from(&self.original_order);

        if let Some(track_id) = current_track_id
            && let Some(pos) = self
                .play_order
                .iter()
                .position(|&ti| self.tracks.get(ti).is_some_and(|t| t.id == track_id))
        {
            self.current_index = Some(pos);
        }
        self.shuffle_enabled = false;
        self.version += 1;
    }

    pub fn cycle_repeat_mode(&mut self) {
        self.set_repeat_mode(match self.repeat_mode {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        });
    }

    /// Bumps `version` only on a move, so a panel re-sending the mode already set re-publishes
    /// no queue.
    pub fn set_repeat_mode(&mut self, mode: RepeatMode) {
        if self.repeat_mode == mode {
            return;
        }
        self.repeat_mode = mode;
        self.version += 1;
    }

    /// Drop tracks whose ID is in `ids_to_remove` from the queue. Maintains the
    /// `tracks` / `play_order` / `original_order` invariants by remapping all
    /// surviving track-indices. Adjusts `current_index` to land on the next
    /// surviving play-order entry at or after the old slot (or, if everything
    /// at or after was removed, the last survivor — or `None` for an empty
    /// queue).
    ///
    /// Bumps `version` only when at least one entry actually changed, so a
    /// no-op call (empty input or no matches) won't trigger a `ViewModel` re-emit.
    pub fn prune_missing(&mut self, ids_to_remove: &HashSet<i64>) -> PruneOutcome {
        self.remove_tracks(|_, track| ids_to_remove.contains(&track.id))
    }

    /// Prune missing local-library rows without treating a cloud song with the same numeric ID as
    /// the same queue entry. Cloud and local IDs belong to separate namespaces.
    pub fn prune_missing_local(&mut self, ids_to_remove: &HashSet<i64>) -> PruneOutcome {
        self.remove_tracks(|_, track| {
            !track.file_path.is_empty() && ids_to_remove.contains(&track.id)
        })
    }

    fn remove_tracks(
        &mut self,
        mut should_remove: impl FnMut(usize, &TrackSummary) -> bool,
    ) -> PruneOutcome {
        let old_len = self.tracks.len();
        let Some(first_removed) =
            self.tracks.iter().enumerate().position(|(i, track)| should_remove(i, track))
        else {
            return PruneOutcome::default();
        };

        // Allocate the index map only when there is something to remove. Compact the
        // canonical tracks in place so survivors keep their order without cloning any Arcs.
        let mut remap = Vec::with_capacity(old_len);
        remap.extend((0..first_removed).map(Some));
        let mut old_index = 0;
        let mut next_index = first_removed;
        self.tracks.retain(|track| {
            let index = old_index;
            old_index += 1;
            if index < first_removed {
                return true;
            }
            let keep = index != first_removed && !should_remove(index, track);
            remap.push(keep.then_some(next_index));
            next_index += usize::from(keep);
            keep
        });

        let current_was_removed = self
            .current_index
            .and_then(|i| self.play_order.get(i))
            .is_some_and(|&i| remap[i].is_none());
        let surviving_before_current = self.current_index.map(|index| {
            self.play_order.iter().take(index).filter(|&&i| remap[i].is_some()).count()
        });

        for order in [&mut self.play_order, &mut self.original_order] {
            order.retain_mut(|i| {
                let Some(new_index) = remap[*i] else { return false };
                *i = new_index;
                true
            });
        }
        self.current_index = surviving_before_current
            .and_then(|index| self.play_order.len().checked_sub(1).map(|last| index.min(last)));
        self.version += 1;

        PruneOutcome { removed: old_len - self.tracks.len(), current_was_removed }
    }

    pub fn to_persistable(&self) -> PersistableQueue {
        let mut track_ids: Vec<i64> = Vec::with_capacity(self.play_order.len());
        track_ids
            .extend(self.play_order.iter().filter_map(|&ti| self.tracks.get(ti).map(|t| t.id)));
        PersistableQueue { track_ids, current_index: current_index_to_i32(self.current_index) }
    }

    /// Return tracks in current play order for `ViewModel` emission.
    /// With Arc, each clone is a ref-count increment (no deep String copies).
    pub fn tracks_in_play_order(&self) -> Vec<Arc<TrackSummary>> {
        let mut result = Vec::with_capacity(self.play_order.len());
        for &ti in &self.play_order {
            if let Some(track) = self.tracks.get(ti) {
                result.push(Arc::clone(track));
            }
        }
        result
    }
}

#[cfg(test)]
#[path = "tests/queue_tests.rs"]
mod tests;
