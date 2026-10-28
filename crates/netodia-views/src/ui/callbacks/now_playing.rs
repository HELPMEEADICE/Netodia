//! Actions on the current track, shared by the Now Playing view and the bottom bar.

use std::sync::Arc;

use slint::ComponentHandle;

use super::cross_tab_nav::{Origin, open_album_cross_tab};
use crate::ui::albums::AlbumsUi;
use netodia_app::library;
use netodia_app::state::AppState;
use netodia_core::entities::track::TrackSummary;
use netodia_core::error::describe;
use netodia_engine::player::engine::state::{lock_state, with_state_emit};
use netodia_ui::{
    AppWindow, ArtistPicker, CloudEntityDetail, CloudTrackActions, Nav, NavEnterFrom, Player,
};

/// Resolve from the clicked song's identity, rather than searching by its displayed album or
/// artist name. Cloud IDs stay decimal strings across the Slint boundary.
pub(super) fn wire_navigation(ui: &AppWindow, state: &AppState, albums_ui: &Arc<AlbumsUi>) {
    let weak = ui.as_weak();
    ui.global::<Player>().on_open_source_artist(move || {
        let Some(ui) = weak.upgrade() else { return };
        let player = ui.global::<Player>();
        if !player.get_can_open_source_artist() {
            return;
        }
        let track = player.get_vm().track;
        dismiss_track_overlays(&ui);
        let picker = ui.global::<ArtistPicker>();
        if track.file_path.is_empty() {
            picker.invoke_request_cloud(track.remote_id);
        } else {
            picker.invoke_request_local(track.id);
        }
    });

    let weak = ui.as_weak();
    let state = state.clone();
    let albums_ui = albums_ui.clone();
    ui.global::<Player>().on_open_source_album(move || {
        let Some(ui) = weak.upgrade() else { return };
        let player = ui.global::<Player>();
        if !player.get_can_open_source_album() {
            return;
        }
        let track = player.get_vm().track;
        let Some(id) = crate::ui::util::parse_cloud_id(track.remote_id.as_str()) else { return };
        let track_key = track.remote_id.to_string();
        let file_path = track.file_path.to_string();
        let cloud = track.file_path.is_empty();
        let origin = Origin::read(&ui);
        let was_now_playing = ui.global::<Nav>().get_now_playing_open();
        let generation = state.cloud.status().generation;
        let state = state.clone();
        let albums_ui = albums_ui.clone();
        let weak = weak.clone();
        state.runtime.clone().spawn(async move {
            let result = if cloud {
                state.cloud.songs(&[id]).await.map(|songs| {
                    songs.into_iter().find(|song| song.id == id).map(|song| song.album.id)
                })
            } else {
                library::tracks::get_track_links(&state.db, &[id]).await.map(|links| {
                    links.into_iter().find(|link| link.id == id).and_then(|link| link.album_id)
                })
            };
            let album_id = match result {
                Ok(Some(id)) if id > 0 => id,
                Ok(_) => return,
                Err(error) => {
                    log::warn!("now_playing::album({id}): {}", describe(&error));
                    return;
                }
            };
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let current = ui.global::<Player>().get_vm();
                if !origin.still_current(&ui)
                    || ui.global::<Nav>().get_now_playing_open() != was_now_playing
                    || !current.has_track
                    || current.track.remote_id.as_str() != track_key
                    || current.track.file_path.as_str() != file_path
                    || (cloud && state.cloud.status().generation != generation)
                {
                    return;
                }
                dismiss_track_overlays(&ui);
                if cloud {
                    ui.global::<CloudTrackActions>().invoke_open_album(album_id.to_string().into());
                } else {
                    open_album_cross_tab(
                        &state,
                        &albums_ui,
                        &ui.as_weak(),
                        album_id,
                        origin,
                        "now_playing::open_album",
                    );
                }
            });
        });
    });
}

fn dismiss_track_overlays(ui: &AppWindow) {
    crate::ui::nav_transition::mark(ui, NavEnterFrom::Right);
    ui.global::<Nav>().set_now_playing_open(false);
    if ui.global::<CloudEntityDetail>().get_open() {
        ui.global::<CloudEntityDetail>().invoke_close();
    }
}

fn favorite_target(track: &TrackSummary) -> Option<(i64, bool)> {
    (track.file_path.is_empty() && track.id > 0).then_some((track.id, !track.is_favorite))
}

/// Wire `Player.toggle-favorite` to the `NetEase` liked-song list. Local files are rejected
/// here as well as in the UI, so shortcuts cannot write their historical database flags.
pub fn wire_now_playing_favorite(ui: &AppWindow, state: &AppState) {
    let s = state.clone();
    ui.global::<Player>().on_toggle_favorite(move || {
        let s = s.clone();
        let Some(current) = lock_state(&s.player_state).source.clone() else {
            return;
        };
        let Some((id, liked)) = favorite_target(&current) else { return };
        s.runtime.clone().spawn(async move {
            let generation = s.cloud.status().generation;
            if let Err(error) = s.cloud.like(id, liked).await {
                log::warn!("netease_like({id}, {liked}): {}", describe(&error));
                return;
            }
            if s.cloud.status().generation != generation {
                return;
            }
            with_state_emit(&s.player_state, &s.sinks, |player| {
                if let Some(track) = player.source.as_mut()
                    && track.id == id
                    && track.file_path.is_empty()
                {
                    Arc::make_mut(track).is_favorite = liked;
                }
                for track in &mut player.queue.tracks {
                    if track.id == id && track.file_path.is_empty() {
                        Arc::make_mut(track).is_favorite = liked;
                    }
                }
            });
            s.cloud_liked_changed.bump();
        });
    });
}

#[cfg(test)]
#[path = "tests/now_playing_tests.rs"]
mod tests;
