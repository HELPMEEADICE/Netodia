//! Account-scoped listening ranking and its virtualized card projection.

use std::sync::Arc;

use slint::{ComponentHandle, Weak};

use super::apply::apply_filtered_grid;
use crate::ui::recently_played::RecentlyPlayedUi;
use netodia_app::state::AppState;
use netodia_core::entities::cloud::PlayedSong;
use netodia_core::entities::track::MostPlayedFavorite;
use netodia_core::error::describe;
use netodia_ui::{AppWindow, RecentlyPlayed};

/// Fetch the account's all-time ranking. Failed refreshes retain the previous snapshot and
/// expose a retry; a superseded account or visit may never repopulate its replacement's cache.
pub async fn refresh_grid(state: &AppState, rp_ui: &Arc<RecentlyPlayedUi>, weak: &Weak<AppWindow>) {
    let generation = rp_ui.begin_grid_refresh();
    let account_generation = state.cloud.status().generation;
    publish_status(rp_ui, weak, generation, true, None);
    let most_played = state.cloud.most_played_songs().await;

    if state.cloud.status().generation != account_generation
        || !rp_ui.grid_refresh_is_current(generation)
    {
        return;
    }

    let most_played = match most_played {
        Ok(songs) => songs,
        Err(error) => {
            let message = describe(&error);
            log::warn!("recently_played::refresh_grid: {message}");
            rp_ui.mark_grid_dirty();
            publish_status(rp_ui, weak, generation, false, Some(message));
            return;
        }
    };
    let most_played = most_played
        .into_iter()
        .map(|entry| {
            let liked = state.cloud.cached_is_liked(entry.song.id).unwrap_or(false);
            most_played_row(entry, liked)
        })
        .collect::<Vec<_>>();

    // A leave that landed while the query was in flight has already cleared this
    // cache (and emptied the model), so storing now would undo the teardown
    // behind a view nobody can see. Nothing is lost by dropping the result: the
    // leave re-arms both flags, and a leave whose wipe bailed because the user was
    // already back re-arms nothing — hence the same mark here.
    if !rp_ui.section_active() {
        rp_ui.mark_grid_dirty();
        return;
    }

    let totals = crate::ui::hero_folds::fold_most_played(&most_played);

    // Serialize the store against `release_section_state`'s wipe through the
    // section gate: without it a fast leave → re-enter can let the wipe land
    // *between* this fresh-data write and the repaint below, and the grid paints
    // empty. Held only across the synchronous stores — never across an `.await`.
    {
        let _gate = rp_ui.gate();
        if !rp_ui.section_active()
            || !rp_ui.grid_refresh_is_current(generation)
            || state.cloud.status().generation != account_generation
        {
            return;
        }
        *rp_ui.state().most_played_totals.lock() = totals;
        *rp_ui.state().most_played.lock() = most_played;
    }
    // The grid write below publishes too, but only past a signature taken over
    // the *filtered* rows — so a play-count bump on a track the filter excludes
    // moves the totals the band states without moving that hash, and on the Songs
    // tab `mounted_content` is a constant `0` besides.
    crate::ui::recently_played::hero::republish_chips(rp_ui, weak);

    // Remote covers schedule their own bounded download/decode when a card becomes visible.
    apply_filtered_grid(rp_ui, weak);
    publish_status(rp_ui, weak, generation, false, None);
}

pub(super) fn most_played_row(entry: PlayedSong, is_favorite: bool) -> MostPlayedFavorite {
    let artist = entry.song.artist_names();
    MostPlayedFavorite {
        id: entry.song.id,
        title: entry.song.name,
        artist: (!artist.is_empty()).then_some(artist),
        album_artist: None,
        album: (!entry.song.album.name.is_empty()).then_some(entry.song.album.name),
        genre: None,
        year: None,
        artwork_path: entry
            .song
            .album
            .cover_url
            .filter(|url| crate::ui::remote_covers::is_remote_artwork_url(url)),
        play_count: entry.play_count,
        duration_ms: i64::try_from(entry.song.duration_ms).unwrap_or(i64::MAX),
        is_favorite,
    }
}

fn publish_status(
    rp_ui: &Arc<RecentlyPlayedUi>,
    weak: &Weak<AppWindow>,
    generation: u64,
    loading: bool,
    error: Option<String>,
) {
    let rp_ui = rp_ui.clone();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if !rp_ui.section_active() || !rp_ui.grid_refresh_is_current(generation) {
            return;
        }
        let Some(ui) = weak.upgrade() else { return };
        let g = ui.global::<RecentlyPlayed>();
        g.set_most_played_loading(loading);
        g.set_most_played_error(error.unwrap_or_default().into());
    });
}
