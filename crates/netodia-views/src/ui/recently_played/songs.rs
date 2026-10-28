//! Songs tab: fetch (recency order), in-memory filter, hero stats, and model apply.
//!
//! Unlike Favorites, the recency set is fetched once and both its membership and its order are
//! fixed — the list is mounted non-sortable. Filtering re-walks the cached `tracks_all` entirely
//! in memory and preserves that order.

use std::collections::HashSet;
use std::sync::Arc;

use slint::{ComponentHandle, Model, VecModel, Weak};

use super::state::SongsTotals;
use super::{RecentlyPlayedTab, RecentlyPlayedUi};
use crate::ui::row_match::{self, Needle};
use crate::ui::util::len_as_i32;
use netodia_app::state::AppState;
use netodia_core::entities::cloud::RecentSong;
use netodia_core::entities::track::TrackListRow as RsTrackListRow;
use netodia_core::error::{AppResult, describe};
use netodia_ui::{AppWindow, RecentlyPlayed, TrackListRow as UiTrackListRow};

/// Read-and-return the active filter needle, folded by [`set_filter`] and ready to hand to a
/// `row_match` predicate.
pub fn current_filter(rp_ui: &RecentlyPlayedUi) -> Needle {
    rp_ui.state().filter.lock().clone()
}

/// Update the cached filter needle. Folded on the way in, so the list and the Most Played grid
/// beside it share one needle.
///
/// Returns the token that names this needle. The Songs walk is bounded by the 200-row recency set
/// and stays on the UI thread; the Most Played walk is not, so it is built on a worker and carries
/// this back to prove it is still the answer being asked for.
pub fn set_filter(rp_ui: &RecentlyPlayedUi, filter: &str) -> u64 {
    *rp_ui.state().filter.lock() = row_match::fold_needle(filter);
    rp_ui.bump_filter_generation()
}

/// Fetch `NetEase`'s recent-play records, cache them, push the hero stats, then apply the in-memory
/// filter into the Slint model. Existing rows remain visible while the account-scoped snapshot is
/// refreshed. Runs on a tokio worker; the model write hops to the UI thread.
pub async fn refresh_tracks(
    state: &AppState,
    rp_ui: &Arc<RecentlyPlayedUi>,
    weak: &Weak<AppWindow>,
) -> AppResult<()> {
    let refresh_generation = rp_ui.begin_tracks_refresh();
    let account_generation = state.cloud.status().generation;
    let cached_count = len_as_i32(rp_ui.state().tracks_all.lock().len());
    set_cloud_state(rp_ui, weak, refresh_generation, true, None, cached_count);
    let rows = match fetch_cloud_recent_tracks(state).await {
        Ok(rows) => rows,
        Err(error) => {
            if state.cloud.status().generation != account_generation
                || !rp_ui.tracks_refresh_is_current(refresh_generation)
            {
                return Ok(());
            }
            if !rp_ui.section_active() {
                rp_ui.mark_dirty();
                return Ok(());
            }
            set_cloud_state(
                rp_ui,
                weak,
                refresh_generation,
                false,
                Some(describe(&error)),
                len_as_i32(rp_ui.state().tracks_all.lock().len()),
            );
            return Err(error);
        }
    };
    if state.cloud.status().generation != account_generation
        || !rp_ui.tracks_refresh_is_current(refresh_generation)
    {
        return Ok(());
    }

    // A leave or account switch can invalidate the request while it is in flight. Don't let an
    // old account response repopulate the retained list after either transition.
    if !rp_ui.section_active() || !rp_ui.tracks_refresh_is_current(refresh_generation) {
        return Ok(());
    }

    // Prewarm the row covers off-thread before the first model apply — a mounted row resolves its
    // thumbnail through `RowCovers.request` on the UI thread, so a cold cache would pay one
    // synchronous decode per unique cover at paint time. The rows are already in NetEase's server
    // order, so the prefix surviving the cap is the right one.
    let cover_paths = crate::ui::grid_prewarm::unique_artwork_paths(
        rows.iter().map(|r| r.artwork_path.as_deref()),
        rp_ui.cover_thumbs.capacity(),
    );
    if !cover_paths.is_empty() {
        let thumbs = rp_ui.cover_thumbs.clone();
        let _ = tokio::task::spawn_blocking(move || thumbs.prewarm(&cover_paths)).await;
    }

    // The cover decode is an `.await`, so recheck the generation before storing this response.
    if !rp_ui.section_active() || !rp_ui.tracks_refresh_is_current(refresh_generation) {
        return Ok(());
    }

    // Hero: count + total duration over the full (unfiltered) recency set, and the up-to-4
    // most-recently-played distinct covers for the mosaic. Folded here, on the worker that holds
    // the rows — a publish triggered from the *grid* has to be able to state them too, so they
    // outlive this function and can't be arguments.
    let totals = SongsTotals {
        tracks: len_as_i32(rows.len()),
        duration_ms: rows.iter().map(|r| r.duration_ms).sum(),
    };
    let fold = crate::ui::hero_folds::fold_tracks(&rows);
    let mosaic_paths = super::hero::mosaic_paths_from(&rows, 4);

    // Store the snapshot and hero values together so they always describe the same server
    // response. Held across synchronous stores only, never across an `.await`.
    {
        let _gate = rp_ui.gate();
        *rp_ui.state().songs_totals.lock() = totals;
        *rp_ui.state().songs_fold.lock() = fold;
        *rp_ui.state().tracks_all.lock() = rows;
    }

    super::hero::push_hero_stats(totals.tracks, rp_ui, weak);
    // Only recompose the banner when the covers differ from the ones on screen: a played-track
    // refresh usually yields the same top four, and recomposing them is wasted blocking-pool
    // work. The matching claim is `hero::publish_hero_artwork`'s, for `MosaicGuard`'s reason.
    if rp_ui.state().last_mosaic_paths.is_stale(&mosaic_paths) {
        let st = state.clone();
        let ru = rp_ui.clone();
        let weak = weak.clone();
        state.runtime.spawn(async move {
            super::hero::refresh_artwork(&st, &ru, mosaic_paths, &weak, /* animate */ true).await;
        });
    }

    apply_filtered_tracks(rp_ui, weak);
    set_cloud_state(rp_ui, weak, refresh_generation, false, None, totals.tracks);
    Ok(())
}

/// Restore the last cloud response before a fresh fetch. The caller is on the UI thread and has
/// already switched to the Songs tab, so this must apply synchronously before Slint mounts its
/// list branch.
pub fn restore_cached_tracks_now(ui: &AppWindow, rp_ui: &RecentlyPlayedUi) {
    let count = len_as_i32(rp_ui.state().tracks_all.lock().len());
    let g = ui.global::<RecentlyPlayed>();
    g.set_track_count(count);
    g.set_cloud_error(slint::SharedString::default());
    g.set_cloud_loading(true);
    apply_filtered_tracks_now(ui, rp_ui);
    crate::ui::hero_chips::publish_recently_played(ui, rp_ui);
}

/// Mirror the account's latest liked-song set into the recent-history cache. The player bar can
/// change a heart while this page is hidden, so the Rust snapshot is updated even when no Slint
/// row is mounted.
pub async fn refresh_liked_flags(
    state: &AppState,
    rp_ui: &Arc<RecentlyPlayedUi>,
    weak: &Weak<AppWindow>,
) -> AppResult<()> {
    let account_generation = state.cloud.status().generation;
    let liked = state.cloud.liked_ids().await?.into_iter().collect::<HashSet<_>>();
    if state.cloud.status().generation != account_generation {
        return Ok(());
    }
    let changes = {
        let _gate = rp_ui.gate();
        if state.cloud.status().generation != account_generation {
            return Ok(());
        }
        for row in rp_ui.state().most_played.lock().iter_mut() {
            row.is_favorite = liked.contains(&row.id);
        }
        let mut rows = rp_ui.state().tracks_all.lock();
        rows.iter_mut()
            .map(|row| {
                row.is_favorite = liked.contains(&row.id);
                (row.id, row.is_favorite)
            })
            .collect::<Vec<_>>()
    };
    if rp_ui.section_active() && rp_ui.active_tab() == RecentlyPlayedTab::Songs {
        for (id, is_favorite) in changes {
            apply_row_favorite(weak, id, is_favorite);
        }
    } else if rp_ui.section_active() {
        super::grid::apply_filtered_grid(rp_ui, weak);
    }
    Ok(())
}

/// Fetch `NetEase`'s recent list and liked membership together. The server order and timestamps are
/// preserved, while the shared row heart reflects the same account's favorites list.
async fn fetch_cloud_recent_tracks(state: &AppState) -> AppResult<Vec<RsTrackListRow>> {
    let (recent, liked_ids) = tokio::join!(state.cloud.recent_songs(), state.cloud.liked_ids());
    let recent = recent?;
    let liked_ids = match liked_ids {
        Ok(ids) => ids.into_iter().collect::<HashSet<_>>(),
        Err(error) => {
            log::debug!("recently_played::liked_ids: {}", describe(&error));
            HashSet::new()
        }
    };
    Ok(recent
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            let song_id = entry.song.id;
            let is_favorite = liked_ids.contains(&song_id)
                || state.cloud.cached_is_liked(song_id).unwrap_or(false);
            let artwork_path = entry
                .song
                .album
                .cover_url
                .clone()
                .filter(|url| crate::ui::remote_covers::is_remote_artwork_url(url));
            recent_song_row(
                entry,
                i32::try_from(index.saturating_add(1)).unwrap_or(i32::MAX),
                is_favorite,
                artwork_path,
            )
        })
        .collect())
}

fn recent_song_row(
    entry: RecentSong,
    server_position: i32,
    is_favorite: bool,
    artwork_path: Option<String>,
) -> RsTrackListRow {
    let RecentSong { song, played_at_ms: _ } = entry;
    let artist = song.artist_names();
    let artist_id = song.artists.first().map(|artist| artist.id);
    RsTrackListRow {
        id: song.id,
        file_path: String::new(),
        file_name: song.name.clone(),
        title: song.name,
        artist: (!artist.is_empty()).then_some(artist),
        album_artist: None,
        album: (!song.album.name.is_empty()).then_some(song.album.name),
        genre: None,
        track_number: Some(server_position),
        disc_number: Some(1),
        year: None,
        duration_ms: i64::try_from(song.duration_ms).unwrap_or(i64::MAX),
        artwork_path,
        is_favorite,
        rating: 0,
        album_id: (song.album.id > 0).then_some(song.album.id),
        artist_id,
        genre_id: None,
        date_added: String::new(),
        sort_key: None,
    }
}

fn set_cloud_state(
    rp_ui: &Arc<RecentlyPlayedUi>,
    weak: &Weak<AppWindow>,
    refresh_generation: u64,
    loading: bool,
    error: Option<String>,
    count: i32,
) {
    let rp_ui = rp_ui.clone();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if !rp_ui.section_active() || !rp_ui.tracks_refresh_is_current(refresh_generation) {
            return;
        }
        let Some(ui) = weak.upgrade() else { return };
        let g = ui.global::<RecentlyPlayed>();
        g.set_cloud_loading(loading);
        g.set_cloud_error(
            error.map_or_else(slint::SharedString::default, slint::SharedString::from),
        );
        g.set_track_count(count);
    });
}

/// Re-walk the cached `tracks_all` through the active filter and push the result into
/// `RecentlyPlayed.tracks`, still in recency order. Existing selection is re-stamped so a filter
/// change doesn't visually drop it.
///
/// A hidden section is never written to, and the check sits in [`write_filtered_tracks`] rather
/// than out here, because the leave can land while the post is in flight.
///
/// **An unmounted tab is refused for the same reason, and asked about twice.**
/// `RecentlyPlayed.tracks` feeds one element, under `if tab-idx == tab-songs`, so on the Most
/// Played tab every row built here reaches nothing. [`build_filtered_tracks`]' check skips the
/// cost on the paths that reach here without a tab pick; [`write_filtered_tracks`]' stops a pick
/// landing mid-post from pinning a row per track behind a tab the user just left.
pub fn apply_filtered_tracks(rp_ui: &Arc<RecentlyPlayedUi>, weak: &Weak<AppWindow>) {
    let Some(rows) = build_filtered_tracks(rp_ui) else {
        return;
    };

    let rp_ui = rp_ui.clone();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        write_filtered_tracks(&ui, &rp_ui, rows);
    });
}

/// Apply from the UI thread, with no event-loop hop — the rows land in the model before Slint
/// re-evaluates the `if` that mounts the entering tab.
///
/// The twin of `grid::apply::apply_filtered_grid_now`: `slint::invoke_from_event_loop` posts even
/// when called *from* the UI thread, so a redraw can win the race. The tab-leave empties this
/// model, so what a lost race paints is a `TrackList` of headers over nothing.
pub fn apply_filtered_tracks_now(ui: &AppWindow, rp_ui: &RecentlyPlayedUi) {
    if let Some(rows) = build_filtered_tracks(rp_ui) {
        write_filtered_tracks(ui, rp_ui, rows);
    }
}

/// Walk the cached `tracks_all` through the active filter, or `None` when Songs isn't the mounted
/// tab and the walk would feed nothing.
///
/// Filters and builds the rows on the calling thread, borrowing the cache in place.
fn build_filtered_tracks(rp_ui: &RecentlyPlayedUi) -> Option<Vec<UiTrackListRow>> {
    if rp_ui.active_tab() != RecentlyPlayedTab::Songs {
        return None;
    }
    let needle = current_filter(rp_ui);
    let all = rp_ui.state().tracks_all.lock();
    Some(crate::ui::tracks::to_slint_track_list_rows(
        all.iter().filter(|r| row_match::track_matches(r, &needle)),
    ))
}

/// Push the rows into `RecentlyPlayed.tracks`. UI thread only.
///
/// Both gates are re-asked here rather than trusted from the build: on the posting path a section
/// leave or a tab pick can land while the closure is in flight, and either has already emptied
/// this model on purpose.
fn write_filtered_tracks(ui: &AppWindow, rp_ui: &RecentlyPlayedUi, mut rows: Vec<UiTrackListRow>) {
    if !rp_ui.section_active() || rp_ui.active_tab() != RecentlyPlayedTab::Songs {
        return;
    }
    let g = ui.global::<RecentlyPlayed>();
    let model = g.get_tracks();
    let Some(vec) = model.as_any().downcast_ref::<VecModel<UiTrackListRow>>() else {
        log::warn!("RecentlyPlayed.tracks: VecModel<TrackListRow> downcast failed");
        return;
    };
    super::selection::restamp_rows(&g, &mut rows);
    crate::ui::model_diff::apply_rows_keyed(vec, rows, |r| r.id);
}

/// Flip `is_favorite` on a single row in the Slint `VecModel`. Recency membership is independent
/// of the favorite flag, so an in-place patch is always correct here — no re-fetch, no full-list
/// rebuild. Mirrors [`crate::ui::tracks::apply_row_favorite`].
pub fn apply_row_favorite(weak: &Weak<AppWindow>, id: i64, fav: bool) {
    let _ = weak.upgrade_in_event_loop(move |ui| {
        crate::ui::model_patch::patch_track_row_by_id(
            &ui.global::<RecentlyPlayed>().get_tracks(),
            id,
            |r| r.is_favorite = fav,
        );
    });
}
