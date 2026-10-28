//! The Favorites page: cloud liked songs and saved albums under a shared tab bar.
//!
//! Songs filters and sorts an account's liked tracks in memory through
//! [`crate::ui::track_list_cache`]. [`albums`] fetches every page of saved albums and owns the
//! membership actions, account invalidation and cover grid. Each tab's banner uses its own
//! first four distinct covers. Legacy local-curation helpers remain in `grids/`.
//!
//! The page's shared contract — counts, dirty flags, signature skips, cover generations — is
//! `.claude/rules/ui-patterns.md`'s "three tabbed pages" block; the deltas below are this page's.
//!
//! This file owns the shared handle and teardown; `tabs.rs` owns sub-view restoration and
//! `callbacks/` owns the page lifecycle and interactions.

pub(super) mod albums;
mod callbacks;
mod covers;
mod grids;
mod hero;
mod rows;
mod selection;
mod songs;
mod state;
mod tabs;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::ui::artists::ArtistsUi;
use crate::ui::artwork_cache::BlurSpec;
use crate::ui::detail_artwork;
use crate::ui::hero_folds::MostPlayedTotals;
use crate::ui::row_match::Needle;
use crate::ui::section_state::{SectionState, impl_section_state_helpers};
use crate::ui::view_ctx::ViewCtx;
use netodia_artwork::media::image::cover_thumbs::CoverThumbs;

use state::FavoritesUiState;

/// This page's `Nav.selected-index`. **The single definition**, beside the view it names, the way
/// [`crate::ui::my_library::NAV_MY_LIBRARY`] sits beside its page — the cross-tab hand-off stamps
/// it as an origin, the lifecycle seeds the section-active shadow from it, and `hero_chips` asks
/// which band is up.
pub const NAV_FAVORITES: i32 = 2;

// `pub(super)` is `pub(in crate::ui)` here, exactly the reach these need: this slice's own
// `callbacks/`, plus `ui::hero_chips`.
pub(super) use grids::{apply_filtered_grids_now, refresh_grids, set_artist_sort};
pub(super) use rows::{to_slint_fav_artist_row, to_slint_most_played_row};
pub(super) use songs::{
    apply_filtered_tracks, apply_filtered_tracks_now, refresh_tracks, set_filter, set_sort,
};
pub use tabs::FavoritesTab;
pub(super) use tabs::{seed_tab, tab_from_index};

/// Install the Favorites models, build the handle, wire every `Favorites.*` callback, and seed the
/// persisted tab.
///
/// `artists_ui` is the cross-tab open-artist hand-off, taken as a parameter so the ordering is a
/// compile error rather than a comment.
///
/// **The tab seed folds in here** rather than sitting with its siblings in
/// `hydrate_ui_from_settings`, because seeding writes the Slint property *and* the handle's
/// synchronous shadow, which the off-thread fetchers read to decide which model to fill and which
/// tier to warm.
///
/// The returned handle is not a keepalive; see [`crate::ui::albums::install`].
pub fn install(cx: ViewCtx<'_>, artists_ui: &Arc<ArtistsUi>) -> Arc<FavoritesUi> {
    rows::install_favorites_models(cx.app);
    let favorites_ui =
        Arc::new(FavoritesUi::new(cx.cover_thumbs.clone(), detail_artwork::blur_spec(cx.app)));
    if let Some(vs) = cx.view_state {
        seed_tab(cx.app, &favorites_ui, vs.favorites_tab);
    }
    callbacks::wire(cx.app, cx.state, cx.view_state, &favorites_ui, artists_ui);
    favorites_ui
}

/// Rust-side state for the Favorites view, shared between the UI callbacks and the async fetchers
/// behind an `Arc`.
pub struct FavoritesUi {
    inner: FavoritesUiState,
    /// The shared row tier, for the Songs tab's `TrackList` column.
    pub(super) cover_thumbs: Arc<CoverThumbs>,
    /// The band's blur shape, or `None` under the aurora setting — read once at install, the
    /// setting being restart-gated, because the compose runs where no window handle is in reach.
    pub(super) hero_blur: Option<BlurSpec>,
    /// Visibility + staleness + the mutation gate, the unit every entity grid carries.
    section: SectionState,
    /// Synchronous shadow of `Favorites.tab-idx`. The off-thread fetchers pick which cover tier to
    /// warm from it — only one grid is ever mounted, so warming both is twice the resident buffers
    /// for nothing.
    active_tab: AtomicU8,
    /// [`SectionState`]'s dirty flag one level down, per fetch rather than per tab — the page has
    /// one `SectionActiveGate` for three mutually exclusive tabs, so without these a refresh tick
    /// ran all three fetches whatever was mounted. Two flags rather than three because one
    /// `refresh_grids` feeds both grid tabs.
    ///
    /// Gating is safe here only because `hero_chips::favorites_chips` is a per-tab match and
    /// Both seeded `true`, so the first pick onto a tab always fetches.
    songs_dirty: AtomicBool,
    grids_dirty: AtomicBool,
}

impl FavoritesUi {
    fn new(cover_thumbs: Arc<CoverThumbs>, hero_blur: Option<BlurSpec>) -> Self {
        Self {
            inner: FavoritesUiState::new(),
            cover_thumbs,
            hero_blur,
            section: SectionState::new(),
            active_tab: AtomicU8::new(FavoritesTab::Songs.as_code()),
            songs_dirty: AtomicBool::new(true),
            grids_dirty: AtomicBool::new(true),
        }
    }

    /// Mirror the mounted sub-view. Written on the UI thread, from the tab bar's pick and from the
    /// section-lifecycle seed.
    pub fn set_active_tab(&self, tab: FavoritesTab) {
        self.active_tab.store(tab.as_code(), Ordering::Relaxed);
    }

    pub fn active_tab(&self) -> FavoritesTab {
        FavoritesTab::from_code(self.active_tab.load(Ordering::Relaxed))
    }

    /// Remember that a refresh tick reached the page with Songs not mounted.
    pub fn mark_songs_dirty(&self) {
        self.songs_dirty.store(true, Ordering::Release);
    }

    /// `true` iff the Songs cache has missed a tick since its last fetch.
    pub fn take_songs_dirty(&self) -> bool {
        self.songs_dirty.swap(false, Ordering::AcqRel)
    }

    /// The grid tabs' equivalent — one flag, because one fetch fills both.
    pub fn mark_grids_dirty(&self) {
        self.grids_dirty.store(true, Ordering::Release);
    }

    pub fn take_grids_dirty(&self) -> bool {
        self.grids_dirty.swap(false, Ordering::AcqRel)
    }

    /// Serialize a bulk-state wipe against a data write. Held only around the write; never across
    /// an `.await`.
    pub(super) fn gate(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.section.gate()
    }

    /// Forget the collage recorded as on screen, so the next refresh recomposes instead of
    /// skipping on an unchanged cover set.
    ///
    /// The section leave is the only caller, and it sits beside the hero-slot wipe rather than in
    /// [`Self::release_section_state`] — that one bails when the user has already come back, where
    /// the wipe is unconditional and would leave the guard set against a hero with no artwork left
    /// to guard.
    pub fn forget_mosaic(&self) {
        self.inner.last_mosaic_paths.forget();
    }

    /// Forget what the grids last painted, so the next apply rebuilds instead of recognising its
    /// own output and skipping.
    ///
    /// Sits beside [`Self::forget_mosaic`] at the section-leave call site, and is unconditional for
    /// the same reason: the models are emptied there, so a signature that survived would match the
    /// identical data on re-enter and leave the grid blank.
    pub fn forget_grid_signature(&self) {
        self.inner.last_grid_signature.lock().take();
    }

    /// Release transient grids and selections while retaining the account's lists and banners.
    /// Runs off the UI thread on section leave, which set `mark_dirty()` synchronously.
    ///
    /// Release order matches `AlbumsUi::release_section_state`: per-section LRUs first, then
    /// mutated state under the section gate, then an `allocator::trim`.
    ///
    /// The gate serializes those stores but does **not** order them, so it is not what stops a
    /// fetch resolving *after* the leave from repopulating what this emptied. That is each
    /// fetcher's own `section_active()` bail — one after the query, one after the prewarm's
    /// `.await` — plus the one inside each apply's event-loop closure.
    pub fn release_section_state(&self) {
        if self.section_active() {
            return;
        }
        crate::ui::grid_prewarm::shrink_covers();
        {
            let _gate = self.gate();
            // Keep the liked songs as a stale-while-revalidate cache. The Slint model is emptied
            // on the UI thread, then rebuilt from these rows immediately when Songs is entered;
            // the lifecycle callback refreshes the account list in the background every time.
            if self.section_active() {
                return;
            }
            self.inner.most_played.lock().clear();
            self.inner.fav_artists.lock().clear();
            *self.inner.most_played_totals.lock() = MostPlayedTotals::default();
            self.inner.applied_selection.lock().clear();
        }
        // Re-arm the remote lists beside their released UI projections. Songs keeps its last
        // successful rows for immediate paint, but still owes a network refresh on the next entry.
        self.mark_songs_dirty();
        self.mark_grids_dirty();
        netodia_platform::services::platform::allocator::trim();
    }

    pub(crate) fn state(&self) -> &FavoritesUiState {
        &self.inner
    }

    /// Track ids of the post-filter Songs tab, in display order — how `shuffle-all` and `play-row`
    /// recover ids without touching the Slint model, callers being off the UI thread.
    pub fn filtered_track_ids(&self) -> Vec<i64> {
        let needle = self.inner.filter.lock().clone();
        self.inner.tracks_all.snapshot().ids_filtered(&needle)
    }

    /// Reuses displayed song metadata when replacing the cloud queue.
    pub fn filtered_cloud_tracks(&self) -> Vec<netodia_core::entities::track::TrackSummary> {
        let needle = self.inner.filter.lock().clone();
        self.inner.tracks_all.snapshot().cloud_tracks_filtered(&needle)
    }

    /// Track ids of the Most Played grid, in card order, so a card there loads that grid rather
    /// than the Songs tab's list. Filtered through the same predicate
    /// `grids::apply::build_filtered_grids` builds the model with — the grid narrows with the
    /// search bar, and the raw cache would enqueue cards that aren't on screen.
    pub fn most_played_track_ids(&self) -> Vec<i64> {
        let needle = self.inner.filter.lock().clone();
        self.inner
            .most_played
            .lock()
            .iter()
            .filter(|t| crate::ui::row_match::most_played_matches(t, &needle))
            .map(|t| t.id)
            .collect()
    }

    /// Patch membership without changing this visit's row positions; report newly liked songs
    /// whose metadata still needs fetching.
    pub(super) fn reconcile_likes(&self, liked: &HashSet<i64>) -> bool {
        let _gate = self.gate();
        if !self.section_active() {
            return false;
        }
        let snapshot = self.inner.tracks_all.snapshot();
        let needle = Needle::default();
        let ids = snapshot.ids_filtered(&needle);
        let displayed: HashSet<i64> = ids.iter().copied().collect();
        let mut retained = self.inner.retained_tracks.lock();
        for (id, row) in ids.into_iter().zip(snapshot.visible(&needle)) {
            let fav = liked.contains(&id);
            if fav {
                retained.remove(&id);
            } else {
                retained.insert(id);
            }
            if row.is_favorite != fav {
                self.inner.tracks_all.set_favorite(id, fav);
            }
        }
        !liked.is_subset(&displayed)
    }

    /// End the undo window before cached rows can paint on the next section entry.
    pub(super) fn finish_visit(&self) {
        let _gate = self.gate();
        for id in self.inner.retained_tracks.lock().drain() {
            self.inner.tracks_all.remove(id);
        }
    }

    /// Set `rating` on a cached row without changing its membership.
    pub fn flip_track_rating(&self, id: i64, rating: i32) {
        self.inner.tracks_all.set_rating(id, rating);
    }
}

impl_section_state_helpers!(FavoritesUi);

// `const _` is type-checked but never dead-code-flagged, so no `#[allow]` is owed.
const _: fn() = || {
    fn check<T: Send + Sync>() {}
    check::<FavoritesUi>();
};

#[cfg(test)]
#[path = "tests/favorites_tests.rs"]
mod tests;
