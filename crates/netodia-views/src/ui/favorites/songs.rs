//! Songs tab: fetch, sort persistence, in-memory filter, model apply.
//!
//! The Tracks view's fetch shape — one SQL fetch of the whole set per `library_changed` tick,
//! with both the per-keystroke filter walk and the per-header-click re-sort running in memory off
//! [`crate::ui::track_list_cache`].

use std::path::PathBuf;
use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString, VecModel, Weak};

use super::{FavoritesTab, FavoritesUi};
use crate::ui::row_match::{self, Needle};
use crate::ui::track_sort;
use netodia_app::services::settings::{SortDir, ViewSort};
use netodia_app::state::AppState;
use netodia_core::entities::cloud::LikedSong;
use netodia_core::entities::track::TrackListRow as RsTrackListRow;
use netodia_core::error::{AppResult, describe};
use netodia_ui::{AppWindow, Favorites, TrackListRow as UiTrackListRow};

/// The active sort. Slint mirrors it in `Favorites.sort-*`, but this cache is the source of truth
/// — those properties are written from Rust, never back.
pub fn current_sort(fav_ui: &FavoritesUi) -> ViewSort {
    fav_ui.state().sort.lock().clone()
}

/// Read-and-return the active filter needle, folded by [`set_filter`] and ready to hand to a
/// `row_match` predicate.
pub fn current_filter(fav_ui: &FavoritesUi) -> Needle {
    fav_ui.state().filter.lock().clone()
}

/// Update the cached sort. Callers follow it with [`resort_and_apply`] and a `set_view_sort`
/// persist, so the next launch lands on the same order.
pub fn set_sort(fav_ui: &FavoritesUi, field: String, dir: SortDir) {
    *fav_ui.state().sort.lock() = ViewSort { field, dir };
}

/// `"asc"` / `"desc"` for the shared comparator.
fn dir_token(dir: SortDir) -> &'static str {
    match dir {
        SortDir::Asc => "asc",
        SortDir::Desc => "desc",
    }
}

/// Whether a header click moved the sort between two reads of the shadow. Compares the direction
/// through [`dir_token`] because `ViewSort` carries no `PartialEq`.
fn sort_changed(a: &ViewSort, b: &ViewSort) -> bool {
    a.field != b.field || dir_token(a.dir) != dir_token(b.dir)
}

/// Update the cached filter needle, so the live-refresh subscriber can re-walk on a background
/// thread without reading the Slint global. Folded on the way in, so all four readers share one.
pub fn set_filter(fav_ui: &FavoritesUi, filter: &str) {
    *fav_ui.state().filter.lock() = row_match::fold_needle(filter);
}

/// Fetch the full favourites list, cache it under the current sort's display order, then re-apply
/// the filter. Runs on whichever thread awaits it: a tokio worker from the section enter, the UI
/// thread from the live-refresh subscriber. The sort is resolved here rather than in SQL; see
/// [`resort_and_apply`].
pub async fn refresh_tracks(
    state: &AppState,
    fav_ui: &Arc<FavoritesUi>,
    weak: &Weak<AppWindow>,
) -> AppResult<()> {
    restore_cached_tracks(state, fav_ui, weak).await;
    let account_generation = state.cloud.status().generation;
    let liked_revision = state.cloud.liked_ids_revision();
    let sort = current_sort(fav_ui);
    let cached_count = fav_ui.state().tracks_all.snapshot().total();
    set_cloud_state(weak, true, None, cached_count);
    let rows = match fetch_cloud_favorites(state).await {
        Ok(rows) => rows,
        Err(error) => {
            if state.cloud.status().generation != account_generation {
                fav_ui.mark_songs_dirty();
                return Ok(());
            }
            fav_ui.mark_songs_dirty();
            let cached_count = fav_ui.state().tracks_all.snapshot().total();
            set_cloud_state(weak, false, Some(describe(&error)), cached_count);
            return Err(error);
        }
    };
    if state.cloud.status().generation != account_generation {
        fav_ui.mark_songs_dirty();
        return Ok(());
    }
    if state.cloud.liked_ids_revision() != liked_revision {
        fav_ui.mark_songs_dirty();
        return Ok(());
    }

    // A hidden view retains the last successful snapshot but must not rebuild its model.
    if !fav_ui.section_active() {
        fav_ui.mark_songs_dirty();
        return Ok(());
    }

    // Warm the row covers before the first apply, or a cold cache pays one synchronous decode per
    // unique cover at paint time. Walked through the display permutation so the prefix surviving
    // the cap is what paints first.
    //
    // The permutation is computed here rather than inside `store_in_order` because it is needed
    // *now*, ahead of the guard that decides whether the store may happen at all.
    let order = track_sort::compute_track_order(&rows, &sort.field, dir_token(sort.dir));
    let cover_paths: Vec<PathBuf> = crate::ui::grid_prewarm::unique_artwork_paths(
        order.iter().map(|&i| rows[i].artwork_path.as_deref()),
        fav_ui.cover_thumbs.capacity(),
    );
    if !cover_paths.is_empty() {
        let thumbs = fav_ui.cover_thumbs.clone();
        let _ = tokio::task::spawn_blocking(move || thumbs.prewarm(&cover_paths)).await;
    }

    if state.cloud.liked_ids_revision() != liked_revision {
        fav_ui.mark_songs_dirty();
        return Ok(());
    }

    // The decode burst is an `.await`, so the guard is asked again — after the slow part, because
    // before it the leave hasn't happened yet.
    if !fav_ui.section_active() {
        fav_ui.mark_songs_dirty();
        return Ok(());
    }

    // The Songs tab's chips, folded on the worker that already holds the rows. Outside the gate:
    // what must not straddle `release_section_state`'s wipe is the pair of *stores*, so the walk
    // has no business holding a lock the wipe and both sibling fetches queue behind.
    let fold = crate::ui::hero_folds::fold_tracks(&rows);

    super::hero::refresh_from_tracks(
        state,
        fav_ui,
        weak,
        &rows,
        account_generation,
        liked_revision,
        false,
    )
    .await;
    if state.cloud.status().generation != account_generation {
        fav_ui.mark_songs_dirty();
        return Ok(());
    }
    if state.cloud.liked_ids_revision() != liked_revision {
        fav_ui.mark_songs_dirty();
        return Ok(());
    }

    // A header click landing mid-fetch re-sorts the cache it finds and returns, with no second
    // query behind it to correct the order — so this store would overwrite it with the permutation
    // computed before the click. Asked twice because `store_in_order` converts every favourite: a
    // click landing inside it leaves the header naming one order and the list showing another.
    let sort_used = current_sort(fav_ui);
    let order = if sort_changed(&sort_used, &sort) {
        track_sort::compute_track_order(&rows, &sort_used.field, dir_token(sort_used.dir))
    } else {
        order
    };

    let count = crate::ui::util::len_as_i32(rows.len());
    {
        let _gate = fav_ui.gate();
        if !fav_ui.section_active()
            || state.cloud.status().generation != account_generation
            || state.cloud.liked_ids_revision() != liked_revision
        {
            fav_ui.mark_songs_dirty();
            return Ok(());
        }
        *fav_ui.state().songs_fold.lock() = fold;
        store_refreshed_tracks(fav_ui, rows, order, &sort_used);
    }
    let sort_now = current_sort(fav_ui);
    if sort_changed(&sort_now, &sort_used) {
        fav_ui.state().tracks_all.resort(&sort_now.field, dir_token(sort_now.dir));
    }
    // Refresh the band after the new fold has landed; the earlier hero update only had the
    // previous fold available while the cloud rows were being prepared.
    super::hero::republish_chips(fav_ui, weak);
    apply_filtered_tracks(fav_ui, weak);
    set_cloud_state(weak, false, None, count);
    Ok(())
}

fn store_refreshed_tracks(
    fav_ui: &FavoritesUi,
    rows: Vec<RsTrackListRow>,
    order: Vec<usize>,
    sort: &ViewSort,
) {
    let retained = fav_ui.state().retained_tracks.lock();
    if retained.is_empty() {
        fav_ui.state().tracks_all.store_in_order(rows, order);
    } else {
        fav_ui.state().tracks_all.store_retaining(
            rows,
            &retained,
            &sort.field,
            dir_token(sort.dir),
        );
    }
}

async fn restore_cached_tracks(
    state: &AppState,
    fav_ui: &Arc<FavoritesUi>,
    weak: &Weak<AppWindow>,
) {
    if !fav_ui.state().tracks_all.snapshot().is_empty() || !fav_ui.section_active() {
        return;
    }
    let generation = state.cloud.status().generation;
    let revision = state.cloud.liked_ids_revision();
    let Some(songs) = state.cloud.cached_liked_songs().await else { return };
    let rows: Vec<_> = songs.into_iter().map(cloud_song_row).collect();
    let stats = super::hero::stats_from_tracks(&rows);
    let sort = current_sort(fav_ui);
    let fold = crate::ui::hero_folds::fold_tracks(&rows);
    {
        let _gate = fav_ui.gate();
        if state.cloud.status().generation != generation
            || state.cloud.liked_ids_revision() != revision
            || !fav_ui.section_active()
            || !fav_ui.state().tracks_all.snapshot().is_empty()
        {
            return;
        }
        fav_ui.state().tracks_all.store(rows, &sort.field, dir_token(sort.dir));
        *fav_ui.state().songs_fold.lock() = fold;
        *fav_ui.state().stats.lock() = stats;
    }
    apply_filtered_tracks(fav_ui, weak);
    super::hero::republish_chips(fav_ui, weak);
    set_cloud_state(weak, true, None, fav_ui.state().tracks_all.snapshot().total());
}

/// Fetch the signed-in account's liked songs from `NetEase` and adapt their identities to the
/// shared track-list model. The local file path stays empty: cloud identities are never treated
/// as files in the local library.
async fn fetch_cloud_favorites(state: &AppState) -> AppResult<Vec<RsTrackListRow>> {
    let liked_songs = state.cloud.liked_songs().await?;
    Ok(liked_songs.into_iter().map(cloud_song_row).collect())
}

fn cloud_song_row(entry: LikedSong) -> RsTrackListRow {
    let LikedSong { song, favorite_order, .. } = entry;
    let artist = song.artist_names();
    let artist_id = song.artists.first().map(|artist| artist.id);
    let artwork_path =
        song.album.cover_url.filter(|url| crate::ui::remote_covers::is_remote_artwork_url(url));
    RsTrackListRow {
        id: song.id,
        file_path: String::new(),
        file_name: song.name.clone(),
        title: song.name,
        artist: (!artist.is_empty()).then_some(artist),
        album_artist: None,
        album: (!song.album.name.is_empty()).then_some(song.album.name),
        genre: None,
        // NetEase's liked-playlist `trackIds` order is authoritative. This position was captured
        // at the network boundary before song details were requested or sorted for display.
        track_number: Some(favorite_order),
        disc_number: Some(1),
        year: None,
        duration_ms: i64::try_from(song.duration_ms).unwrap_or(i64::MAX),
        artwork_path,
        is_favorite: true,
        rating: 0,
        album_id: (song.album.id > 0).then_some(song.album.id),
        artist_id,
        genre_id: None,
        date_added: String::new(),
        sort_key: None,
    }
}

fn set_cloud_state(weak: &Weak<AppWindow>, loading: bool, error: Option<String>, count: i32) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        let g = ui.global::<Favorites>();
        g.set_cloud_loading(loading);
        g.set_cloud_error(error.map_or_else(SharedString::default, SharedString::from));
        g.set_track_count(count);
    });
}

/// Re-walk the cached `tracks_all` through the active filter and push the result into
/// `Favorites.tracks`. Selected ids are re-stamped before the swap, so a filter change or library
/// refresh doesn't visually drop a selection.
///
/// A hidden section is never written to, and the check must sit in [`write_filtered_tracks`]
/// rather than out here because the leave can land while the post is in flight.
///
/// **An unmounted tab is refused for the same reason, and asked about twice.** `Favorites.tracks`
/// feeds one element under `if tab-idx == tab-songs`, so on the other two tabs every row built
/// here reaches nothing — [`build_filtered_tracks`]' check skips the cost, and
/// [`write_filtered_tracks`]' stops a pick landing mid-post from pinning a row per favourite
/// behind a tab the user just left.
pub fn apply_filtered_tracks(fav_ui: &Arc<FavoritesUi>, weak: &Weak<AppWindow>) {
    let Some(rows) = build_filtered_tracks(fav_ui) else {
        return;
    };

    let fav_ui = fav_ui.clone();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        write_filtered_tracks(&ui, &fav_ui, rows);
    });
}

/// Apply from the UI thread, with no event-loop hop — the rows land in the model before Slint
/// re-evaluates the `if` that mounts the entering tab.
///
/// `grids::apply::apply_filtered_grids_now`'s twin: `invoke_from_event_loop` posts even when
/// called *from* the UI thread, so a redraw can win the race — and the tab-leave emptied this
/// model, so what a lost race paints is a `TrackList` of headers over nothing.
pub fn apply_filtered_tracks_now(ui: &AppWindow, fav_ui: &FavoritesUi) {
    if let Some(rows) = build_filtered_tracks(fav_ui) {
        write_filtered_tracks(ui, fav_ui, rows);
    }
}

/// Walk the cached `tracks_all` through the active filter, or `None` when Songs isn't the mounted
/// tab and the walk would feed nothing.
///
/// Runs on the calling thread off one `Arc` snapshot and allocates nothing per row — the cache
/// holds converted rows, so a survivor is cloned, not rebuilt.
fn build_filtered_tracks(fav_ui: &FavoritesUi) -> Option<Vec<UiTrackListRow>> {
    if fav_ui.active_tab() != FavoritesTab::Songs {
        return None;
    }
    let needle = current_filter(fav_ui);
    Some(fav_ui.state().tracks_all.snapshot().visible(&needle))
}

/// Push the rows into `Favorites.tracks`. UI thread only.
///
/// Both gates are re-asked here rather than trusted from the build: on the posting path a section
/// leave or a tab pick can land while the closure is in flight, and either has already emptied
/// this model on purpose.
fn write_filtered_tracks(ui: &AppWindow, fav_ui: &FavoritesUi, mut rendered: Vec<UiTrackListRow>) {
    if !fav_ui.section_active() || fav_ui.active_tab() != FavoritesTab::Songs {
        return;
    }
    let g = ui.global::<Favorites>();
    let model = g.get_tracks();
    let Some(vec) = model.as_any().downcast_ref::<VecModel<UiTrackListRow>>() else {
        log::warn!("Favorites.tracks: VecModel<TrackListRow> downcast failed");
        return;
    };
    super::selection::restamp_rows(&g, &mut rendered);
    // A per-row rewrite where the identities align — a library tick — keeps the `ListView`'s
    // delegates instead of tearing them all down; a structural change falls back to `set_vec`.
    crate::ui::model_diff::apply_rows_keyed(vec, rendered, crate::ui::tracks::model_key);
}
