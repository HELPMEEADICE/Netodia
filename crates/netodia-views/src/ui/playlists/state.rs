//! Internal data structures + constants used by the Playlists grid and
//! Playlist Detail submodules.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

use crate::ui::track_detail::DetailCache;
use netodia_core::entities::playlist::PlaylistStats;
use netodia_core::entities::smart_criteria::SmartCriteria;

/// A playlist's pre-lowercased name, computed once per `fetch_grid` so the
/// name sort allocates nothing. Positionally aligned with
/// [`GridData::playlists`].
#[derive(Clone)]
pub(super) struct PlaylistSortKey {
    pub name_lc: String,
}

/// The grid's canonical data: the playlist list plus its pre-lowercased
/// sort keys, kept together behind one `Arc` so a rebuild is a
/// single refcount bump (and the two halves can never drift out of sync).
#[derive(Clone)]
pub(super) struct GridData {
    pub playlists: Vec<PlaylistStats>,
    pub keys: Vec<PlaylistSortKey>,
    /// Positionally aligned with [`GridData::playlists`]. `true` iff the row is
    /// a smart playlist whose criteria can be moved by a play-count flush
    /// (`SmartCriteria::depends_on_play_stats`); always `false` for regular
    /// playlists. Computed once here (one criteria parse per smart row) so the
    /// `stats_changed` gate is a cheap flag scan and the stats-only refresh can
    /// recount just the rows a play stat change can affect.
    pub stat_dependent: Vec<bool>,
}

impl GridData {
    pub(super) fn new(playlists: Vec<PlaylistStats>) -> Self {
        let keys =
            playlists.iter().map(|p| PlaylistSortKey { name_lc: p.name.to_lowercase() }).collect();
        let stat_dependent = playlists
            .iter()
            .map(|p| {
                p.is_smart
                    && SmartCriteria::from_json_opt(p.smart_criteria.as_deref())
                        .depends_on_play_stats()
            })
            .collect();
        Self { playlists, keys, stat_dependent }
    }

    /// Patch measured track stats without replacing list metadata or card ordering.
    pub(super) fn update_track_stats(&mut self, detail: &PlaylistStats) -> bool {
        let Some(playlist) = self.playlists.iter_mut().find(|row| row.id == detail.id) else {
            return false;
        };
        if playlist.track_count == detail.track_count
            && playlist.total_duration_ms == detail.total_duration_ms
        {
            return false;
        }
        playlist.track_count = detail.track_count;
        playlist.total_duration_ms = detail.total_duration_ms;
        true
    }

    /// Whether any cached row is a stat-dependent smart playlist — the gate for
    /// the `stats_changed` refresh.
    pub(super) fn has_stat_dependent(&self) -> bool {
        self.stat_dependent.iter().any(|&b| b)
    }

    /// `(track_count, total_duration_ms, stat_dependent)` for the row with `id`,
    /// if present. Backs the stats-only refresh: stat-dependent rows are
    /// recounted, others carry these cached counts forward. `stat_dependent` is
    /// `false` for regular playlists, so the same lookup gates the open-detail
    /// refresh too.
    pub(super) fn row_stats_by_id(&self, id: i64) -> Option<(i32, i64, bool)> {
        self.playlists.iter().position(|p| p.id == id).map(|i| {
            let p = &self.playlists[i];
            (p.track_count, p.total_duration_ms, self.stat_dependent[i])
        })
    }
}

/// Memoized filter + sort result — playlist indices in display order,
/// plus the `(filter, sort_field, sort_dir)` that produced them. A pure
/// `columns-changed` re-chunk reuses `indices` without re-filtering /
/// re-sorting; a filter or sort change recomputes. Cleared whenever
/// `fetch_grid` replaces the grid data.
pub(super) struct GridIndexCache {
    pub filter: String,
    pub sort_field: String,
    pub sort_dir: String,
    pub indices: Vec<usize>,
}

impl GridIndexCache {
    pub(super) fn matches(&self, filter: &str, sort_field: &str, sort_dir: &str) -> bool {
        self.filter == filter && self.sort_field == sort_field && self.sort_dir == sort_dir
    }
}

/// Grid-side state — the canonical playlist data the card grid derives
/// from.
pub(super) struct PlaylistGridState {
    pub data: Mutex<Arc<GridData>>,
    pub index_cache: Mutex<Option<GridIndexCache>>,
    pub categories: Mutex<HashMap<i32, CachedGrid>>,
}

pub(super) struct CachedGrid {
    pub account_generation: u64,
    pub fetched_at: Instant,
    pub data: Arc<GridData>,
}

/// Detail-side state: the shared track cache plus the curated order only this detail has.
pub(super) struct PlaylistDetailState {
    pub cache: DetailCache,
    /// Track ids in CANONICAL position (insertion / drag-reorder) order.
    /// Drag-reorder mutates this **optimistically** before the DB write
    /// lands, so the `"position"` sort can rebuild `tracks` from this
    /// without a re-fetch. Always in lock-step with the playlist's
    /// `playlist_items.position` column on disk modulo the in-flight
    /// optimistic write.
    pub position_order: Mutex<Vec<i64>>,
}

/// How many leading playlists' covers `fetch_grid` prewarms before the
/// grid first paints. Same rationale as the Albums tier — covers the first
/// screenful at any reasonable column count.
pub(super) const GRID_PREWARM_AHEAD: usize = 24;
