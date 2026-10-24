//! `PlaylistDetail.*` callbacks: close-detail, drag-reorder, the edit-artwork picker open and
//! track removal here, the rest through [`crate::ui::callbacks::track_detail`].

use std::sync::Arc;

use slint::{ComponentHandle, Global as _};

use super::artwork::open_edit_artwork;
use crate::ui::callbacks::track_detail;
use crate::ui::callbacks::{DialogClaim, collect_track_ids};
use crate::ui::playlists::{self as playlists_ui_mod, PlaylistsUi};
use netodia_app::library;
use netodia_app::state::AppState;
use netodia_core::error::describe;
use netodia_engine::player::engine::state::lock_state;
use netodia_engine::player::engine::types::PlaybackStatus;
use netodia_ui::{AppWindow, PlaylistDetail};

/// Wire the `PlaylistDetail` callbacks. See [`super::wire`].
pub(super) fn wire(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    track_detail::wire(&ui.global::<PlaylistDetail>().as_weak(), state, playlists_ui);
    wire_cloud_playback(ui, state, playlists_ui);
    wire_close(ui, state, playlists_ui);
    wire_reorder(ui, state, playlists_ui);
    wire_edit_artwork(ui, state, playlists_ui);
    wire_remove_track(ui, state, playlists_ui);
}

/// Cloud playlist rows do not exist in `SQLite`, so activation, Play All and Shuffle seat the
/// queue from the rows the detail is already showing — the same songs `song/detail` would return —
/// which leaves only the stream ticket between the click and the first sound.
fn wire_cloud_playback(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    let s = state.clone();
    let pu = playlists_ui.clone();
    ui.global::<PlaylistDetail>().on_play_cloud_row(move |remote_id, _index| {
        let Some(song_id) = crate::ui::util::parse_cloud_id(remote_id.as_str()) else {
            log::warn!("playlists::play_cloud_song: invalid NetEase song ID");
            return;
        };
        play_cloud_track(&s, &pu, song_id, false);
    });

    // Shuffle All: one row at random, shuffle forced on, and the pill row follows the choice it
    // just made so the next Play pressing the same button starts the same way.
    let s = state.clone();
    let pu = playlists_ui.clone();
    let weak = ui.as_weak();
    ui.global::<PlaylistDetail>().on_shuffle_all(move || {
        use rand::RngExt;
        let rows = pu.detail.cache.displayed_rows();
        if rows.is_empty() {
            return;
        }
        let first = rows[rand::rng().random_range(..rows.len())].id;
        set_play_mode(&weak, PLAY_MODE_SHUFFLE);
        play_cloud_track(&s, &pu, first, true);
    });

    // Play All: the list in its own order, from the first row.
    let s = state.clone();
    let pu = playlists_ui.clone();
    let weak = ui.as_weak();
    ui.global::<PlaylistDetail>().on_play_all(move || {
        let Some(first) = pu.detail.cache.displayed_rows().first().map(|row| row.id) else {
            return;
        };
        set_play_mode(&weak, PLAY_MODE_ORDERED);
        play_cloud_track(&s, &pu, first, false);
    });

    // The one button that has to know what "play" means right now: with one of this playlist's
    // rows already playing it toggles the transport, and otherwise it starts the list the way the
    // pill row is set. Stopped is the third case — a restored cloud queue with no source.
    let s = state.clone();
    let pu = playlists_ui.clone();
    let weak = ui.as_weak();
    ui.global::<PlaylistDetail>().on_toggle_playback(move || {
        let Some(ui) = weak.upgrade() else { return };
        let playing_here = {
            let player = lock_state(&s.player_state);
            let current = player.source.as_ref().map(|track| track.id);
            let holds_one_of_ours =
                current.is_some_and(|id| pu.detail.cache.track_ids().contains(&id));
            holds_one_of_ours && player.status != PlaybackStatus::Stopped
        };
        if playing_here {
            let state = s.clone();
            s.runtime.spawn(async move {
                if let Err(error) =
                    library::playback::player_toggle_play_pause(&state.playback_ctx())
                {
                    log::warn!("playlists::toggle_playback: {}", describe(&error));
                }
            });
            return;
        }
        let shuffle = ui.global::<PlaylistDetail>().get_play_mode() == PLAY_MODE_SHUFFLE;
        let rows = pu.detail.cache.displayed_rows();
        let Some(first) = rows.first().map(|row| row.id) else { return };
        let first = if shuffle {
            use rand::RngExt;
            rows[rand::rng().random_range(..rows.len())].id
        } else {
            first
        };
        play_cloud_track(&s, &pu, first, shuffle);
    });
}

/// Which way the detail's Play / Shuffle pills fill the queue. Slint shows the choice; this is the
/// spelling both sides compare, so a typo can't quietly make them disagree.
const PLAY_MODE_ORDERED: &str = "ordered";
const PLAY_MODE_SHUFFLE: &str = "shuffle";

fn set_play_mode(weak: &slint::Weak<AppWindow>, mode: &str) {
    if let Some(ui) = weak.upgrade() {
        ui.global::<PlaylistDetail>().set_play_mode(slint::SharedString::from(mode));
    }
}

fn play_cloud_track(state: &AppState, playlists_ui: &PlaylistsUi, song_id: i64, shuffle: bool) {
    if song_id <= 0 {
        return;
    }
    let tracks = super::super::track_summaries(&playlists_ui.detail.cache.displayed_rows());
    let state = state.clone();
    let runtime = state.runtime.clone();
    runtime.spawn(async move {
        let (volume, speed) = {
            let player = lock_state(&state.player_state);
            (player.effective_volume(), player.playback_speed)
        };
        if let Err(error) =
            state.cloud_playback.play_queue_tracks(tracks, song_id, shuffle, volume, speed).await
        {
            log::warn!("playlists::play_cloud_song({song_id}): {}", describe(&error));
        }
    });
}

/// The header's back button.
///
/// No `mark_drill_back`, where its three siblings have one: theirs precedes a `return_to_section`
/// that flips the nav index, and this detail records no origin at all. The body that mounts in
/// its place takes a fixed `below` and reads the global not at all; see `ui::nav_transition`.
fn wire_close(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    let s = state.clone();
    let pu = playlists_ui.clone();
    let weak = ui.as_weak();
    ui.global::<PlaylistDetail>().on_close_detail(move || {
        let Some(ui) = weak.upgrade() else { return };
        pu.detail_request.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        ui.global::<PlaylistDetail>().set_cloud_collection(false);
        track_detail::forget_closed(&ui, &s, &*pu, &ui.global::<PlaylistDetail>());

        let pu = pu.clone();
        s.runtime.spawn_blocking(move || {
            pu.release_detail_artwork();
            pu.prewarm_visible_covers();
        });
    });
}

/// A row drag released. Optimistically permutes the visible rows and the position-order cache,
/// then writes; a failed write rolls both back.
fn wire_reorder(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    let s = state.clone();
    let pu = playlists_ui.clone();
    let weak = ui.as_weak();
    ui.global::<PlaylistDetail>().on_reorder(move |from, to| {
        let Some(ui) = weak.upgrade() else { return };
        let Ok(from_u) = usize::try_from(from) else {
            return;
        };
        let Ok(to_u) = usize::try_from(to) else {
            return;
        };
        if from_u == to_u {
            return;
        }
        let playlist_id = pu.detail_playlist_id();
        if playlist_id < 0 {
            return;
        }
        let Some(previous_order) =
            playlists_ui_mod::apply_optimistic_reorder(&ui, &pu, from_u, to_u)
        else {
            return;
        };
        let s = s.clone();
        let pu = pu.clone();
        let weak = weak.clone();
        s.runtime.clone().spawn(async move {
            if let Err(e) = library::playlists::reorder_playlist(&s.db, playlist_id, from, to).await
            {
                log::warn!("playlists::reorder: {}", describe(&e));
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    playlists_ui_mod::rollback_reorder(&ui, &pu, previous_order);
                });
            }
        });
    });
}

/// Opens the Edit Artwork picker seeding `current-artwork` from `PlaylistDetail.cover`, already
/// decoded for the open detail, so the dialog opens on a preview of the saved state.
///
/// Rename and Delete need no handler here: their opens are populated inline in
/// `my-library/tab-pills.slint`, and their commits are `super::crud`'s.
fn wire_edit_artwork(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    let s = state.clone();
    let pu = playlists_ui.clone();
    let weak = ui.as_weak();
    ui.global::<PlaylistDetail>().on_request_edit_artwork(move || {
        let id = pu.detail_playlist_id();
        if id < 0 {
            return;
        }
        let Some(claim) = DialogClaim::take_from(&weak) else { return };
        open_edit_artwork(&s, &weak, claim, id, |ui| ui.global::<PlaylistDetail>().get_cover());
    });
}

/// The row menu's removal. One row and a whole selection both arrive as `[int]` and collapse to
/// one batched write; `refresh_detail` prunes the removed ids out of the selection on the way
/// back.
fn wire_remove_track(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    let s = state.clone();
    let pu = playlists_ui.clone();
    let weak = ui.as_weak();
    ui.global::<PlaylistDetail>().on_remove_track(move |track_ids| {
        let id_vec = collect_track_ids(&track_ids);
        let playlist_id = pu.detail_playlist_id();
        if playlist_id < 0 || id_vec.is_empty() {
            return;
        }
        let s = s.clone();
        let pu = pu.clone();
        let weak = weak.clone();
        s.runtime.clone().spawn(async move {
            if let Err(e) =
                library::playlists::remove_tracks_from_playlist_batch(&s.db, playlist_id, id_vec)
                    .await
            {
                log::warn!("playlists::remove_track: {}", describe(&e));
                return;
            }
            if let Err(e) =
                playlists_ui_mod::refresh_detail(&s, &pu, weak.clone(), playlist_id).await
            {
                log::warn!("playlists::remove_track refresh: {}", describe(&e));
            }
            if let Err(e) = playlists_ui_mod::fetch_grid(&s, &pu, weak).await {
                log::warn!("playlists::remove_track refetch grid: {}", describe(&e));
            }
        });
    });
}
