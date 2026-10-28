//! Shared exact-ID cloud row actions for Favorites, history and playlist details.

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use netodia_app::state::AppState;
use netodia_core::entities::cloud::Song;
use netodia_core::entities::track::TrackListRow as RsTrackListRow;
use netodia_core::error::describe;
use netodia_engine::player::engine::state::lock_state;
use netodia_engine::player::engine::state::with_state_emit;
use netodia_ui::{
    AppWindow, CloudEntityDetail, CloudTrackActions, Favorites, Player, PlaylistDetail, Queue,
    RecentlyPlayed, TrackListRow as UiTrackListRow,
};

static ENTITY_DETAIL_REQUEST: AtomicU64 = AtomicU64::new(0);

pub(super) fn wire(ui: &AppWindow, state: &AppState) {
    wire_track_actions(ui, state);
    wire_entity_actions(ui, state);
    wire_entity_detail_actions(ui, state);
}

fn wire_track_actions(ui: &AppWindow, state: &AppState) {
    let actions = ui.global::<CloudTrackActions>();
    let s = state.clone();
    actions.on_enqueue(move |remote_id, play_next| {
        let Some(id) = crate::ui::util::parse_cloud_id(remote_id.as_str()) else { return };
        let s = s.clone();
        s.runtime.clone().spawn(async move {
            if let Err(error) = s.cloud_playback.enqueue(&[id], play_next).await {
                log::warn!("cloud_tracks::enqueue({id}): {}", describe(&error));
            }
        });
    });

    let s = state.clone();
    let weak = ui.as_weak();
    actions.on_toggle_favorite(move |remote_id, liked| {
        let Some(id) = crate::ui::util::parse_cloud_id(remote_id.as_str()) else { return };
        let s = s.clone();
        let weak = weak.clone();
        s.runtime.clone().spawn(async move {
            let generation = s.cloud.status().generation;
            if let Err(error) = s.cloud.like(id, liked).await {
                log::warn!("cloud_tracks::like({id}): {}", describe(&error));
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
            let cloud = s.cloud.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if cloud.status().generation != generation {
                    return;
                }
                patch_like(&ui.global::<Favorites>().get_tracks(), id, liked);
                patch_like(&ui.global::<RecentlyPlayed>().get_tracks(), id, liked);
                patch_like(&ui.global::<PlaylistDetail>().get_tracks(), id, liked);
                patch_like(&ui.global::<CloudEntityDetail>().get_tracks(), id, liked);
                let queue_rows = ui.global::<Queue>().get_rows();
                let key = id.to_string();
                for index in 0..queue_rows.row_count() {
                    if let Some(mut row) = queue_rows.row_data(index)
                        && row.is_cloud
                        && row.remote_id.as_str() == key
                    {
                        row.is_favorite = liked;
                        queue_rows.set_row_data(index, row);
                    }
                }
                let player = ui.global::<Player>();
                let mut vm = player.get_vm();
                if vm.track.file_path.is_empty() && vm.track.remote_id.as_str() == key {
                    vm.track.is_favorite = liked;
                    player.set_vm(vm);
                }
            });
            s.cloud_liked_changed.bump();
        });
    });
}

fn wire_entity_actions(ui: &AppWindow, state: &AppState) {
    let actions = ui.global::<CloudTrackActions>();
    let weak = ui.as_weak();
    let s = state.clone();
    actions.on_open_album(move |remote_id| {
        let Some(ui) = weak.upgrade() else { return };
        open_entity_detail(&ui, &s, "album", remote_id.to_string());
    });

    let weak = ui.as_weak();
    let s = state.clone();
    actions.on_open_artist(move |remote_id| {
        let Some(ui) = weak.upgrade() else { return };
        open_entity_detail(&ui, &s, "artist", remote_id.to_string());
    });
}

fn wire_entity_detail_actions(ui: &AppWindow, state: &AppState) {
    let weak = ui.as_weak();
    let s = state.clone();
    ui.global::<CloudEntityDetail>().on_retry(move || {
        let Some(ui) = weak.upgrade() else { return };
        let detail = ui.global::<CloudEntityDetail>();
        open_entity_detail(&ui, &s, detail.get_kind().as_str(), detail.get_remote_id().to_string());
    });

    let weak = ui.as_weak();
    ui.global::<CloudEntityDetail>().on_close(move || {
        ENTITY_DETAIL_REQUEST.fetch_add(1, Ordering::Relaxed);
        let Some(ui) = weak.upgrade() else { return };
        let detail = ui.global::<CloudEntityDetail>();
        if !detail.get_open() {
            return;
        }
        // Reveal the retained source while the shared header collapses. Its outgoing facts must
        // survive until `closed`, just like My Library's hero-collapsed teardown.
        detail.set_return_tick(detail.get_return_tick().wrapping_add(1));
        detail.set_open(false);
    });

    let weak = ui.as_weak();
    ui.global::<CloudEntityDetail>().on_closed(move || {
        let Some(ui) = weak.upgrade() else { return };
        let detail = ui.global::<CloudEntityDetail>();
        if detail.get_open() {
            return;
        }
        detail.set_loading(false);
        detail.set_error(SharedString::default());
        detail.set_tracks(empty_track_model());
        detail.set_title(SharedString::default());
        detail.set_subtitle(SharedString::default());
        detail.set_artwork_path(SharedString::default());
    });

    let weak = ui.as_weak();
    let s = state.clone();
    ui.global::<CloudEntityDetail>().on_play_cloud_row(move |remote_id, _index| {
        let Some(song_id) = crate::ui::util::parse_cloud_id(remote_id.as_str()) else { return };
        let Some(ui) = weak.upgrade() else { return };
        let rows = ui.global::<CloudEntityDetail>().get_tracks();
        let ids: Vec<i64> = (0..rows.row_count())
            .filter_map(|index| rows.row_data(index))
            .filter_map(|row| crate::ui::util::parse_cloud_id(row.remote_id.as_str()))
            .collect();
        if ids.is_empty() || !ids.contains(&song_id) {
            return;
        }
        let s = s.clone();
        s.runtime.clone().spawn(async move {
            let (volume, speed) = {
                let player = lock_state(&s.player_state);
                (player.effective_volume(), player.playback_speed)
            };
            if let Err(error) =
                s.cloud_playback.play_queue(&ids, song_id, false, volume, speed).await
            {
                log::warn!("cloud_tracks::play_entity_song({song_id}): {}", describe(&error));
            }
        });
    });
}

fn open_entity_detail(ui: &AppWindow, state: &AppState, kind: &str, remote_id: String) {
    let Some(id) = crate::ui::util::parse_cloud_id(&remote_id) else {
        log::warn!("cloud_tracks::open_entity: invalid NetEase entity ID");
        return;
    };
    if id <= 0 || !matches!(kind, "album" | "artist") {
        return;
    }

    let request = ENTITY_DETAIL_REQUEST.fetch_add(1, Ordering::Relaxed) + 1;
    let kind = kind.to_owned();
    let detail = ui.global::<CloudEntityDetail>();
    detail.set_kind(kind.clone().into());
    detail.set_remote_id(remote_id.clone().into());
    detail.set_title(SharedString::default());
    detail.set_subtitle(SharedString::default());
    detail.set_artwork_path(SharedString::default());
    detail.set_tracks(empty_track_model());
    detail.set_loading(true);
    detail.set_error(SharedString::default());

    detail.set_album_favorite(false);
    detail.set_album_favorite_known(false);
    detail.set_album_favorite_pending(false);
    detail.set_album_favorite_error(SharedString::default());
    detail.set_open(true);
    if kind == "album" {
        ui.global::<Favorites>().invoke_load_album_favorite(remote_id.clone().into());
    }

    let state = state.clone();
    let weak = ui.as_weak();
    state.runtime.clone().spawn(async move {
        let result = match kind.as_str() {
            "album" => state.cloud.album(id).await.map(|(album, songs)| {
                let subtitle =
                    album.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(" / ");
                let cover = remote_artwork(album.cover_url);
                (album.name, subtitle, cover, songs)
            }),
            "artist" => state.cloud.artist(id).await.map(|(artist, songs)| {
                let cover = remote_artwork(artist.image_url().map(str::to_owned));
                (artist.name, String::new(), cover, songs)
            }),
            _ => return,
        };

        let _ = weak.upgrade_in_event_loop(move |ui| {
            if ENTITY_DETAIL_REQUEST.load(Ordering::Relaxed) != request {
                return;
            }
            let detail = ui.global::<CloudEntityDetail>();
            if !detail.get_open() || detail.get_remote_id().as_str() != remote_id {
                return;
            }
            match result {
                Ok((title, subtitle, artwork, songs)) => {
                    let rows = cloud_song_rows(songs, &state);
                    detail.set_title(title.into());
                    detail.set_subtitle(subtitle.into());
                    detail.set_artwork_path(artwork.into());
                    detail.set_tracks(ModelRc::from(Rc::new(VecModel::from(rows))));
                    detail.set_loading(false);
                }
                Err(error) => {
                    log::warn!("cloud_tracks::open_{}({id}): {}", kind, describe(&error));
                    detail.set_loading(false);
                    detail.set_error(describe(&error).into());
                }
            }
        });
    });
}

fn empty_track_model() -> ModelRc<UiTrackListRow> {
    ModelRc::from(Rc::new(VecModel::default()))
}

fn remote_artwork(url: Option<String>) -> String {
    url.filter(|url| crate::ui::remote_covers::is_remote_artwork_url(url)).unwrap_or_default()
}

fn cloud_song_rows(songs: Vec<Song>, state: &AppState) -> Vec<UiTrackListRow> {
    let rows: Vec<RsTrackListRow> = songs
        .into_iter()
        .enumerate()
        .map(|(index, song)| {
            let artist = song.artist_names();
            let artwork_path = remote_artwork(song.album.cover_url.clone());
            RsTrackListRow {
                id: song.id,
                file_path: String::new(),
                file_name: song.name.clone(),
                title: song.name,
                artist: (!artist.is_empty()).then_some(artist),
                album_artist: None,
                album: (!song.album.name.is_empty()).then_some(song.album.name),
                genre: None,
                track_number: Some(i32::try_from(index + 1).unwrap_or(i32::MAX)),
                disc_number: Some(1),
                year: None,
                duration_ms: i64::try_from(song.duration_ms).unwrap_or(i64::MAX),
                artwork_path: (!artwork_path.is_empty()).then_some(artwork_path),
                is_favorite: state.cloud.cached_is_liked(song.id).unwrap_or(false),
                rating: 0,
                album_id: (song.album.id > 0).then_some(song.album.id),
                artist_id: song.artists.first().map(|artist| artist.id),
                genre_id: None,
                date_added: String::new(),
                sort_key: None,
            }
        })
        .collect();
    crate::ui::tracks::to_slint_track_list_rows(&rows)
}

fn patch_like(rows: &ModelRc<netodia_ui::TrackListRow>, id: i64, liked: bool) {
    let key = id.to_string();
    for index in 0..rows.row_count() {
        if let Some(mut row) = rows.row_data(index)
            && row.remote_id.as_str() == key
        {
            row.is_favorite = liked;
            rows.set_row_data(index, row);
        }
    }
}

#[cfg(test)]
#[path = "tests/cloud_tracks_tests.rs"]
mod tests;
