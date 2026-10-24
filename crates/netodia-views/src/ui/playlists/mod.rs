//! Playlists-view glue between Rust and Slint.
//!
//! Two Slint globals are driven from here:
//!
//! * `Playlists` — the responsive playlist-card grid. Rust owns a flat
//!   `Vec<PlaylistStats>` (plus a pre-lowercased name sort key) behind
//!   `PlaylistsUi::grid.data`; the grid model is rebuilt from it on every
//!   filter / sort / column-count change *without* a DB hit. Per-card cover
//!   lookup is lazy via `request-cover`. The flat `Playlists.rows` model
//!   (consumed by the per-track Add-to-Playlist submenu) is kept in
//!   `updated_at DESC` order and rebuilt on every `fetch_grid`.
//!
//! * `PlaylistDetail` — the full playlist detail view. `open_playlist`
//!   fetches the header + track list; the rows in a [`DetailCache`] plus the
//!   canonical position-order id list let `play-row`, selection,
//!   drag-reorder and the `"position"` sort recover indices without reading
//!   the Slint model back.
//!
//! Cross-thread layout mirrors `albums.rs`: `PlaylistsUi` is `Send + Sync`,
//! cloned into callbacks / tokio tasks; Slint properties and models are
//! only touched from the UI thread via
//! `Weak<AppWindow>::upgrade_in_event_loop`.

mod callbacks;
mod detail;
mod grid;
mod state;

use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use parking_lot::Mutex;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::ui::artwork_cache::BlurSpec;
use crate::ui::detail_artwork::{self, DetailArtwork};
use crate::ui::section_state::{SectionState, impl_section_state_helpers};
use crate::ui::track_detail::DetailCache;
use crate::ui::util::{clamp_i64_to_i32, opt_shared};
use crate::ui::view_ctx::ViewCtx;
use netodia_artwork::media::image::cover_thumbs::CoverThumbs;
use netodia_core::entities::cloud::MusicLibraryCategory;
use netodia_core::entities::playlist::PlaylistStats;
use netodia_core::entities::track::{TrackListRow as RsTrackListRow, TrackSummary};
use netodia_ui::{
    AppWindow, PlaylistDetail, PlaylistGridRow as UiPlaylistGridRow, PlaylistRow as UiPlaylistRow,
    Playlists, TrackListRow as UiTrackListRow,
};

use state::{GridData, PlaylistDetailState, PlaylistGridState};

#[cfg(test)]
use grid::compute_indices;

// Reached from outside the slice: `ui::nav_history` replays a walk into a
// detail, `boot::ui_setup` seeds the persisted one, and its
// `initial_grid_fetch!` kicks the first fetch after the window is shown
// — which is why `fetch_grid` can't fold into `install` with the rest.
pub use detail::{open_playlist_with, seed_detail_from_settings};
pub use grid::fetch_grid;

// Reached from this slice's own `callbacks/`. `pub(super)` is `pub(in crate::ui)`
// here, one notch wider than that.
/// The M3U8 import / export and Add-to-Playlist wiring, kept out of [`install`] because each
/// needs the `Rc<NotificationsUi>` for its completion toasts and that is created after the
/// per-view wiring runs. Boot's `install_ui` calls it once the stack exists.
pub use callbacks::wire_files;
pub(super) use detail::{
    apply_optimistic_reorder, open_playlist, refresh_detail, rollback_reorder,
};
pub(super) use grid::{fetch_grid_stats, rebuild_grid};

/// Install the Playlists grid + detail models, build the handle, and wire every
/// `Playlists.*` / `PlaylistDetail.*` callback to it, bar the ones [`wire_files`] holds.
///
/// The returned handle is not a keepalive; see [`crate::ui::albums::install`].
pub fn install(cx: ViewCtx<'_>) -> Arc<PlaylistsUi> {
    install_models(cx.app);
    let playlists_ui =
        Arc::new(PlaylistsUi::new(cx.cover_thumbs.clone(), detail_artwork::blur_spec(cx.app)));
    playlists_ui.category.store(cx.app.global::<Playlists>().get_category(), Ordering::Release);
    callbacks::wire(cx.app, cx.state, &playlists_ui);
    playlists_ui
}

/// Rust-side state for the Playlists grid + detail views. Shared between
/// the UI callbacks (`callbacks::wire`) and the async fetchers. Mirrors
/// `AlbumsUi` field-for-field.
pub struct PlaylistsUi {
    category: AtomicI32,
    grid_request: AtomicU64,
    detail_request: AtomicU64,
    roaming_request: AtomicU64,
    roaming_cancel: Mutex<tokio_util::sync::CancellationToken>,
    grid: PlaylistGridState,
    detail: PlaylistDetailState,
    /// Row-tier cache — shared with Tracks / Browse — backs the
    /// detail track-list's artwork column.
    /// The grid cards draw from `ui::grid_prewarm::tier()`, which no view holds.
    cover_thumbs: Arc<CoverThumbs>,
    /// Detail-tier `(cover, blur)` pair cache for the Playlist Detail
    /// header. Released on detail-close and on section-leave.
    detail_artwork: Arc<DetailArtwork>,
    /// Section-visibility + staleness bookkeeping (on-screen shadow, dirty
    /// flag, and the wipe/fetch mutation gate). See [`SectionState`].
    section: SectionState,
}

impl PlaylistsUi {
    fn new(cover_thumbs: Arc<CoverThumbs>, hero_blur: Option<BlurSpec>) -> Self {
        Self {
            category: AtomicI32::new(0),
            grid_request: AtomicU64::new(0),
            detail_request: AtomicU64::new(0),
            roaming_request: AtomicU64::new(0),
            roaming_cancel: Mutex::new(tokio_util::sync::CancellationToken::new()),
            grid: PlaylistGridState {
                data: Mutex::new(Arc::new(GridData::new(Vec::new()))),
                index_cache: Mutex::new(None),
            },
            detail: PlaylistDetailState {
                cache: DetailCache::default(),
                position_order: Mutex::new(Vec::new()),
            },
            cover_thumbs,
            detail_artwork: Arc::new(DetailArtwork::new(hero_blur)),
            section: SectionState::new(),
        }
    }

    fn category(&self) -> MusicLibraryCategory {
        MusicLibraryCategory::from_index(self.category.load(Ordering::Acquire))
    }

    fn grid_request_current(&self, request: u64) -> bool {
        self.grid_request.load(Ordering::Acquire) == request
    }

    /// Drop the account-scoped grid and detail rows before another session is published.
    pub(crate) fn clear_account_data(&self) {
        self.grid_request.fetch_add(1, Ordering::AcqRel);
        self.detail_request.fetch_add(1, Ordering::AcqRel);
        self.roaming_cancel.lock().cancel();
        crate::ui::window_chrome::set_current_playlist_id(-1);
        self.detail_artwork.clear();
        let _gate = self.section.gate();
        *self.grid.data.lock() = Arc::new(GridData::new(Vec::new()));
        *self.grid.index_cache.lock() = None;
        self.detail.cache.clear();
        self.detail.position_order.lock().clear();
    }

    /// Drop everything resident in the Playlists section — grid covers,
    /// detail artwork pair, canonical grid data, memoized indices,
    /// cached detail tracks, applied-selection shadow — then return the
    /// freed pages to the OS. Race-correct against an in-flight
    /// `fetch_grid` via `mutation_gate` (see `AlbumsUi::release_section_state`
    /// for the contract).
    pub fn release_section_state(&self) {
        if self.section_active() {
            return;
        }
        crate::ui::grid_prewarm::shrink_covers();
        self.detail_artwork.clear();
        {
            let _gate = self.section.gate();
            if self.section_active() {
                return;
            }
            *self.grid.data.lock() = Arc::new(GridData::new(Vec::new()));
            *self.grid.index_cache.lock() = None;
            self.detail.cache.release_rows();
            self.detail.position_order.lock().clear();
        }
        netodia_platform::services::platform::allocator::trim();
    }

    /// Drop just the detail-tier `(cover, blur)` pair cache — called
    /// when the user closes a playlist detail.
    pub fn release_detail_artwork(&self) {
        self.detail_artwork.clear();
        netodia_platform::services::platform::allocator::trim();
    }

    /// Decodes the first screenful of grid covers into the grid tier, so the grid's first frame
    /// paints covers rather than placeholders. Every prewarm of this grid comes through here, and
    /// does nothing unless the grid is what is drawn ([`SectionState::grid_on_screen`]).
    pub fn prewarm_visible_covers(&self) {
        if !self.section.grid_on_screen(|| self.detail_playlist_id() >= 0) {
            return;
        }
        let data = self.grid.data.lock().clone();
        let unique = grid::first_screenful_paths(&data);
        if !unique.is_empty() {
            crate::ui::grid_prewarm::prewarm(&unique);
        }
    }

    /// Playlist id currently open in the detail view (`-1` = grid).
    pub fn detail_playlist_id(&self) -> i64 {
        self.detail.cache.id()
    }

    /// Whether the cached grid holds any smart playlist whose criteria depend on
    /// play stats. Gates the `stats_changed` subscriber so a play-count flush
    /// only refreshes when a smart playlist could actually have changed — a
    /// static-rule ("Genre is Rock") smart playlist is skipped entirely. Stricter
    /// than a plain "any smart playlist?" check, and the correct gate for the
    /// stats path (a static-rule smart playlist can't have moved on a play flush).
    pub fn has_stat_dependent_smart_playlists(&self) -> bool {
        self.grid.data.lock().has_stat_dependent()
    }

    /// Whether the playlist with `id` is a stat-dependent smart playlist. Gates
    /// the open-detail refresh on the `stats_changed` path — a play stat change
    /// can't move a static-rule smart playlist, so its detail need not re-evaluate.
    pub fn is_playlist_smart_stat_dependent(&self, id: i64) -> bool {
        self.grid
            .data
            .lock()
            .row_stats_by_id(id)
            .is_some_and(|(_, _, stat_dependent)| stat_dependent)
    }

    /// Lookup of a playlist's canonical stats by id, against the cached
    /// grid `PlaylistStats` list. Clones the hit so the call site doesn't
    /// hold a lock across UI work. Backs `Playlists.row-name` /
    /// `row-description` / `request-edit-artwork-for` — grid-overlay
    /// callbacks that only know the playlist id.
    pub fn grid_stats_by_id(&self, id: i64) -> Option<PlaylistStats> {
        let data = self.grid.data.lock().clone();
        data.playlists.iter().find(|p| p.id == id).cloned()
    }

    /// Row-tier cover lookup — backs
    /// `Playlists.request-row-cover`, used by the mosaic-picker
    /// dialog's 64 px candidate tiles and preview slots. Resolves
    /// against the shared `cover_thumbs` LRU (the same one the detail
    /// track-list artwork column uses), so the dialog doesn't pay for
    /// a full-size grid-tier decode for a 64 px tile.
    pub fn row_cover(&self, artwork_path: &str) -> slint::Image {
        self.cover_thumbs.get_or_load_opt(Some(artwork_path).filter(|s| !s.is_empty()))
    }
}

/// Build the empty `VecModel`s the Playlists grid (chunked + flat),
/// the detail track list, and the detail selection need, and hand them
/// to the Slint globals as `ModelRc`s.
fn install_models(ui: &AppWindow) {
    let grid: Rc<VecModel<UiPlaylistGridRow>> = Rc::new(VecModel::default());
    ui.global::<Playlists>().set_grid_rows(ModelRc::from(grid));

    let flat: Rc<VecModel<UiPlaylistRow>> = Rc::new(VecModel::default());
    ui.global::<Playlists>().set_rows(ModelRc::from(flat));

    let tracks: Rc<VecModel<UiTrackListRow>> = Rc::new(VecModel::default());
    ui.global::<PlaylistDetail>().set_tracks(ModelRc::from(tracks));

    let sel: Rc<VecModel<i32>> = Rc::new(VecModel::default());
    ui.global::<PlaylistDetail>().set_selected_ids(ModelRc::from(sel));
}

/// A detail row as the engine's queue entry. The rows the detail already holds *are* the
/// `song/detail` response — title through artwork — so a click that seats the queue from them
/// costs one stream ticket instead of a round trip for a list the screen is showing.
pub(super) fn track_summaries(rows: &[RsTrackListRow]) -> Vec<TrackSummary> {
    rows.iter()
        .map(|row| TrackSummary {
            id: row.id,
            file_path: String::new(),
            file_name: row.file_name.clone(),
            title: row.title.clone(),
            artist: row.artist.clone(),
            album: row.album.clone(),
            duration_ms: row.duration_ms,
            artwork_path: row.artwork_path.clone(),
            track_number: row.track_number,
            disc_number: row.disc_number,
            last_position: 0,
            is_favorite: row.is_favorite,
            rating: row.rating,
            replaygain_track_gain: None,
            replaygain_track_peak: None,
            replaygain_album_gain: None,
            replaygain_album_peak: None,
        })
        .collect()
}

/// Convert a `PlaylistStats` into the Slint `PlaylistRow` an `EntityCard`
/// (and the detail header) renders. Pure data — the cover is *not*
/// decoded here; it's pulled lazily per visible card via `request-cover`.
pub fn to_slint_playlist_row(p: &PlaylistStats) -> UiPlaylistRow {
    UiPlaylistRow {
        id: clamp_i64_to_i32(p.id),
        remote_id: SharedString::from(p.id.to_string()),
        name: SharedString::from(p.name.as_str()),
        description: opt_shared(p.description.as_deref()),
        artwork_path: opt_shared(p.thumbnail_path.as_deref()),
        track_count: p.track_count,
        total_duration_ms: i32::try_from(p.total_duration_ms.clamp(0, i64::from(i32::MAX)))
            .unwrap_or(i32::MAX),
        is_smart: p.is_smart,
    }
}

impl_section_state_helpers!(PlaylistsUi);

// `const _` is type-checked but never dead-code-flagged, so no `#[allow]` is owed.
const _: fn() = || {
    fn check<T: Send + Sync>() {}
    check::<PlaylistsUi>();
};

#[cfg(test)]
#[path = "tests/playlists_tests.rs"]
mod tests;
