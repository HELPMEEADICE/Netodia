//! The Recently Played page: a `MosaicTabHero` banner over two tabs.
//!
//! Songs is a capped `TrackList` of the most-recently-played tracks, fetched once per refresh and
//! re-walked in memory on a keystroke. It mounts `sortable: false`: recency *is* the page, so its
//! headers resize and toggle but never re-order. Most Played is a virtualized grid over the
//! account's all-time listening ranking. Both tabs read cloud snapshots, not local statistics.
//!
//! The page's shared contract is `.claude/rules/ui-patterns.md`'s "three tabbed pages" block; the
//! deltas below are this page's.
//!
//! The tree is split by the question each file answers rather than by tab. This file is the handle
//! and its teardown; `tabs.rs` the sub-view enum and its seeding,
//! `rows.rs` the Slint models, `hero.rs` the band, `songs.rs` the Songs tab, and `grid/` the Most
//! Played tab in three parts — `fetch`, `apply` and `warm`.

mod callbacks;
mod grid;
mod hero;
mod rows;
mod selection;
mod songs;
mod state;
mod tabs;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use crate::ui::artwork_cache::BlurSpec;
use crate::ui::detail_artwork;
use crate::ui::hero_folds::{HeroFold, MostPlayedTotals};
use crate::ui::section_state::{SectionState, impl_section_state_helpers};
use crate::ui::view_ctx::ViewCtx;
use netodia_artwork::media::image::cover_thumbs::CoverThumbs;

use state::{RecentlyPlayedUiState, SongsTotals};

/// This page's `Nav.selected-index` — see [`crate::ui::favorites::NAV_FAVORITES`].
pub const NAV_RECENTLY_PLAYED: i32 = 8;

// `pub(super)` is `pub(in crate::ui)` here, exactly the reach these need: this slice's own
// `callbacks/`, plus `ui::hero_chips`.
pub(super) use grid::{apply_filtered_grid_now, apply_filtered_grid_settled, refresh_grid};
pub(super) use rows::to_slint_most_played_row;
pub(super) use selection::{clear_selection, handle_select_row, select_all};
pub(super) use songs::{
    apply_filtered_tracks, apply_row_favorite, refresh_liked_flags, refresh_tracks,
    restore_cached_tracks_now, set_filter,
};
pub use tabs::RecentlyPlayedTab;
pub(super) use tabs::{seed_tab, tab_from_index};

/// Install the Recently-Played models, build the handle, wire every `RecentlyPlayed.*` callback,
/// and seed the persisted tab.
///
/// A trimmed Favorites. Its row-menu "Go to …" entries are wired centrally by
/// `wire_cross_tab_nav`, so it takes no peer handle; the tab seed folds in here for the reason it
/// does there — see [`crate::ui::favorites::install`].
pub fn install(cx: ViewCtx<'_>) -> Arc<RecentlyPlayedUi> {
    rows::install_recently_played_models(cx.app);
    let rp_ui =
        Arc::new(RecentlyPlayedUi::new(cx.cover_thumbs.clone(), detail_artwork::blur_spec(cx.app)));
    callbacks::wire(cx.app, cx.state, &rp_ui);
    if let Some(vs) = cx.view_state {
        seed_tab(cx.app, &rp_ui, vs.recently_played_tab);
    }
    rp_ui
}

/// Rust-side state for the Recently Played view, shared between the UI callbacks and the async
/// fetchers behind an `Arc`.
pub struct RecentlyPlayedUi {
    inner: RecentlyPlayedUiState,
    /// The shared row tier, for the Songs tab's `TrackList` column.
    pub(super) cover_thumbs: Arc<CoverThumbs>,
    /// The band's blur shape, or `None` under the aurora setting — read once at install, the
    /// setting being restart-gated, because the compose runs where no window handle is in reach.
    pub(super) hero_blur: Option<BlurSpec>,
    /// Visibility + staleness + the mutation gate, the unit every entity grid carries. Also gates
    /// the refresh subscriber, so a background tick can't repaint a hidden view.
    section: SectionState,
    /// Synchronous shadow of `RecentlyPlayed.tab-idx`. The off-thread fetchers pick which model to
    /// fill and which tier to warm from it — only one sub-view is ever mounted, so doing both is
    /// twice the resident buffers for a surface nobody can scroll.
    active_tab: AtomicU8,
    /// [`SectionState`]'s dirty flag one level down, for Most Played alone.
    ///
    /// Seeded on entry, leave and account changes, so the Songs tab does not fetch a ranking
    /// nobody can see and the first pick that mounts it always requests the current account.
    grid_dirty: AtomicBool,
    /// Bumped by every write to the filter needle, so a grid build running off the UI thread can
    /// tell whether it still answers the current one.
    ///
    /// The Most Played walk builds on a worker — once a build outlives its keystroke, two can
    /// finish in either order and the loser paints a needle the user has moved past.
    /// `write_filtered_grid`'s signature check
    /// can't see it: a stale set has a *different* signature, which is exactly what it reads as
    /// "apply this".
    filter_generation: AtomicU64,
    /// Monotonic token for cloud-history refreshes. An older response must not overwrite a newer
    /// visit or account snapshot when requests finish out of order.
    tracks_refresh_generation: AtomicU64,
    grid_refresh_generation: AtomicU64,
}

impl RecentlyPlayedUi {
    fn new(cover_thumbs: Arc<CoverThumbs>, hero_blur: Option<BlurSpec>) -> Self {
        Self {
            inner: RecentlyPlayedUiState::new(),
            cover_thumbs,
            hero_blur,
            section: SectionState::new(),
            active_tab: AtomicU8::new(RecentlyPlayedTab::Songs.as_code()),
            grid_dirty: AtomicBool::new(true),
            filter_generation: AtomicU64::new(0),
            tracks_refresh_generation: AtomicU64::new(0),
            grid_refresh_generation: AtomicU64::new(0),
        }
    }

    /// Mirror the mounted sub-view. Written on the UI thread, from the tab bar's pick and from the
    /// section-lifecycle seed.
    pub fn set_active_tab(&self, tab: RecentlyPlayedTab) {
        self.active_tab.store(tab.as_code(), Ordering::Relaxed);
    }

    pub fn active_tab(&self) -> RecentlyPlayedTab {
        RecentlyPlayedTab::from_code(self.active_tab.load(Ordering::Relaxed))
    }

    /// Remember that a refresh tick reached the page with Most Played not mounted.
    pub fn mark_grid_dirty(&self) {
        self.grid_dirty.store(true, Ordering::Release);
    }

    /// `true` iff the Most Played cache has missed a tick since its last fetch.
    pub fn take_grid_dirty(&self) -> bool {
        self.grid_dirty.swap(false, Ordering::AcqRel)
    }

    /// Record that the needle moved, handing back the token a deferred build carries so it can
    /// tell whether it is still the current answer.
    pub(super) fn bump_filter_generation(&self) -> u64 {
        self.filter_generation.fetch_add(1, Ordering::AcqRel).wrapping_add(1)
    }

    /// The token a deferred grid build must still match to write.
    pub(super) fn filter_generation(&self) -> u64 {
        self.filter_generation.load(Ordering::Acquire)
    }

    pub(super) fn begin_tracks_refresh(&self) -> u64 {
        self.tracks_refresh_generation.fetch_add(1, Ordering::AcqRel).wrapping_add(1)
    }

    pub(super) fn tracks_refresh_is_current(&self, generation: u64) -> bool {
        self.tracks_refresh_generation.load(Ordering::Acquire) == generation
    }

    pub(super) fn begin_grid_refresh(&self) -> u64 {
        self.grid_refresh_generation.fetch_add(1, Ordering::AcqRel).wrapping_add(1)
    }

    pub(super) fn grid_refresh_is_current(&self, generation: u64) -> bool {
        self.grid_refresh_generation.load(Ordering::Acquire) == generation
    }

    pub(super) fn grid_refresh_generation(&self) -> u64 {
        self.grid_refresh_generation.load(Ordering::Acquire)
    }

    /// Drop the previous account's recent-history snapshot before its replacement can arrive.
    pub(super) fn clear_account_tracks(&self) {
        self.begin_tracks_refresh();
        self.begin_grid_refresh();
        self.bump_filter_generation();
        self.mark_grid_dirty();
        self.forget_grid_signature();
        let _gate = self.gate();
        self.inner.tracks_all.lock().clear();
        self.inner.most_played.lock().clear();
        *self.inner.most_played_totals.lock() = MostPlayedTotals::default();
        *self.inner.songs_totals.lock() = SongsTotals::default();
        *self.inner.songs_fold.lock() = HeroFold::default();
        self.inner.applied_selection.lock().clear();
    }

    /// Serialize a bulk-state wipe against a data write. Held only around the write; never across
    /// an `.await`.
    pub(super) fn gate(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.section.gate()
    }

    /// Forget the collage recorded as on screen, so the next refresh recomposes. Paired with the
    /// leave handler's hero-slot wipe rather than with [`Self::release_section_state`] — see
    /// [`crate::ui::favorites::FavoritesUi::forget_mosaic`].
    pub fn forget_mosaic(&self) {
        self.inner.last_mosaic_paths.forget();
    }

    /// Forget what the grid last painted, so the next apply rebuilds instead of recognising its
    /// own output and skipping. Sits beside [`Self::forget_mosaic`] and is unconditional for the
    /// same reason: the model is emptied there, so a surviving signature would match the identical
    /// data on re-enter and leave the grid blank.
    pub fn forget_grid_signature(&self) {
        self.inner.last_grid_signature.lock().take();
    }

    /// Drop the Most Played grid when this page is hidden, but keep the small cloud
    /// history snapshot so the Songs tab paints immediately on its next visit. Runs off the UI
    /// thread after the section leave marks its pending refresh.
    pub fn release_section_state(&self) {
        if self.section_active() {
            return;
        }
        {
            let _gate = self.gate();
            self.inner.most_played.lock().clear();
            // Keep the small NetEase recent-history snapshot warm between page visits; its next
            // entry immediately paints this list while a fresh server snapshot is requested.
            *self.inner.most_played_totals.lock() = MostPlayedTotals::default();
            self.inner.applied_selection.lock().clear();
        }
        // Re-arm the cloud ranking fetch beside the grid cache it emptied.
        self.mark_grid_dirty();
        netodia_platform::services::platform::allocator::trim();
    }

    pub(crate) fn state(&self) -> &RecentlyPlayedUiState {
        &self.inner
    }

    /// Reuse the ranking's metadata and server order in the cloud queue, with the same filter as
    /// the visible cards. Original 64-bit song identities never go through Slint integers.
    pub fn most_played_cloud_tracks(&self) -> Vec<netodia_core::entities::track::TrackSummary> {
        let needle = self.inner.filter.lock().clone();
        self.inner
            .most_played
            .lock()
            .iter()
            .filter(|row| crate::ui::row_match::most_played_matches(row, &needle))
            .map(|row| netodia_core::entities::track::TrackSummary {
                id: row.id,
                title: row.title.clone(),
                file_name: row.title.clone(),
                artist: row.artist.clone(),
                album: row.album.clone(),
                duration_ms: row.duration_ms,
                artwork_path: row.artwork_path.clone(),
                is_favorite: row.is_favorite,
                file_path: String::new(),
                track_number: None,
                disc_number: None,
                last_position: 0,
                rating: 0,
                replaygain_track_gain: None,
                replaygain_track_peak: None,
                replaygain_album_gain: None,
                replaygain_album_peak: None,
            })
            .collect()
    }

    /// Track ids of the post-filter Songs tab in **display order** — recency, less whatever the
    /// search bar has narrowed away — so `shuffle-all` / `play-row` enqueue what the user sees.
    pub fn filtered_track_ids(&self) -> Vec<i64> {
        let needle = self.inner.filter.lock().clone();
        self.inner
            .tracks_all
            .lock()
            .iter()
            .filter(|r| crate::ui::row_match::track_matches(r, &needle))
            .map(|r| r.id)
            .collect()
    }

    /// Reuses recent-history metadata, preserving its filtered display order in the queue.
    pub fn filtered_cloud_tracks(&self) -> Vec<netodia_core::entities::track::TrackSummary> {
        let needle = self.inner.filter.lock().clone();
        self.inner
            .tracks_all
            .lock()
            .iter()
            .filter(|row| crate::ui::row_match::track_matches(row, &needle))
            .map(crate::ui::cloud_queue::track_summary)
            .collect()
    }

    /// Flip `is_favorite` on a cached row so a single-row toggle lands without a
    /// re-fetch. Unlike Favorites it never removes the row — recency membership
    /// is independent of both flags.
    pub fn flip_track_favorite(&self, id: i64, fav: bool) {
        if let Some(r) = self.inner.tracks_all.lock().iter_mut().find(|r| r.id == id) {
            r.is_favorite = fav;
        }
        if let Some(r) = self.inner.most_played.lock().iter_mut().find(|r| r.id == id) {
            r.is_favorite = fav;
        }
    }
}

impl_section_state_helpers!(RecentlyPlayedUi);

// `const _` is type-checked but never dead-code-flagged, so no `#[allow]` is owed.
const _: fn() = || {
    fn check<T: Send + Sync>() {}
    check::<RecentlyPlayedUi>();
};

#[cfg(test)]
#[path = "tests/recently_played_tests.rs"]
mod tests;
