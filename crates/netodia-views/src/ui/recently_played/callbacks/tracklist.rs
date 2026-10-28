//! Songs-tab callbacks for `NetEase`'s account-scoped recent history.

use std::sync::Arc;

use slint::{ComponentHandle, Global as _};

use crate::ui::callbacks::collect_track_ids;
use crate::ui::callbacks::track_list::wire_toggle_column;
use crate::ui::recently_played::{
    self as recently_played_ui_mod, RecentlyPlayedTab, RecentlyPlayedUi,
};
use netodia_app::cloud_playback::CloudQueueShuffle;
use netodia_app::state::AppState;
use netodia_core::entities::track::TrackSummary;
use netodia_core::error::describe;
use netodia_engine::player::engine::state::lock_state;
use netodia_ui::{AppWindow, RecentlyPlayed};

/// Wire the cloud-history list, its search, and account-aware row actions.
pub(super) fn wire(ui: &AppWindow, state: &AppState, rp_ui: &Arc<RecentlyPlayedUi>) {
    let g = ui.global::<RecentlyPlayed>();
    wire_play_row(&g, state, rp_ui);
    wire_row_flags(ui, state, rp_ui);
    // NetEase history rows must never be routed through the local database queue.
    g.on_play_next(|_| {});
    g.on_add_to_queue(|_| {});
    wire_retry(ui, state, rp_ui);
    wire_filter(ui, state, rp_ui);
    wire_toggle_column(&g.as_weak(), state);
    wire_selection(ui, rp_ui);
    wire_shuffle(&g, state, rp_ui);
}

fn wire_play_row(g: &RecentlyPlayed<'_>, state: &AppState, rp_ui: &Arc<RecentlyPlayedUi>) {
    let cloud_state = state.clone();
    let cloud_ui = rp_ui.clone();
    g.on_play_cloud_row(move |remote_id, _index| {
        let Some(song_id) = crate::ui::util::parse_cloud_id(remote_id.as_str()) else { return };
        let tracks = cloud_ui.filtered_cloud_tracks();
        if tracks.iter().any(|track| track.id == song_id) {
            play_recent_queue(&cloud_state, tracks, song_id, CloudQueueShuffle::Preserve);
        }
    });
    let state = state.clone();
    let rp_ui = rp_ui.clone();
    g.on_play_row(move |track_id, index| {
        let tracks = rp_ui.filtered_cloud_tracks();
        let selected = usize::try_from(index)
            .ok()
            .and_then(|position| tracks.get(position).map(|track| track.id))
            .filter(|id| crate::ui::util::clamp_i64_to_i32(*id) == track_id)
            .or_else(|| {
                tracks
                    .iter()
                    .map(|track| track.id)
                    .find(|id| crate::ui::util::clamp_i64_to_i32(*id) == track_id)
            });
        if let Some(selected) = selected {
            play_recent_queue(&state, tracks, selected, CloudQueueShuffle::Preserve);
        }
    });
}

pub(super) fn play_recent_queue(
    state: &AppState,
    tracks: Vec<TrackSummary>,
    selected: i64,
    shuffle: CloudQueueShuffle,
) {
    let state = state.clone();
    state.runtime.clone().spawn(async move {
        let (volume, speed) = {
            let player = lock_state(&state.player_state);
            (player.effective_volume(), player.playback_speed)
        };
        if let Err(error) =
            state.cloud_playback.play_queue_tracks(tracks, selected, shuffle, volume, speed).await
        {
            log::warn!("recently_played::play_cloud_queue({selected}): {}", describe(&error));
        }
    });
}

fn wire_row_flags(ui: &AppWindow, state: &AppState, rp_ui: &Arc<RecentlyPlayedUi>) {
    let g = ui.global::<RecentlyPlayed>();
    let weak = ui.as_weak();
    let state = state.clone();
    let runtime = state.runtime.clone();
    let cloud = state.cloud.clone();
    let rp_ui = rp_ui.clone();
    g.on_toggle_row_favorite(move |model_ids, liked| {
        let ids = collect_track_ids(&model_ids);
        let state = state.clone();
        let cloud = cloud.clone();
        let rp_ui = rp_ui.clone();
        let weak = weak.clone();
        runtime.spawn(async move {
            for id in ids {
                if let Err(error) = cloud.like(id, liked).await {
                    log::warn!("recently_played::netease_like({id}): {}", describe(&error));
                    state.cloud_liked_changed.bump();
                    return;
                }
                rp_ui.flip_track_favorite(id, liked);
                recently_played_ui_mod::apply_row_favorite(&weak, id, liked);
            }
            state.cloud_liked_changed.bump();
        });
    });
}

fn wire_retry(ui: &AppWindow, state: &AppState, rp_ui: &Arc<RecentlyPlayedUi>) {
    let state = state.clone();
    let rp_ui = rp_ui.clone();
    let weak = ui.as_weak();
    ui.global::<RecentlyPlayed>().on_retry_cloud(move || {
        if !rp_ui.section_active() {
            return;
        }
        let state = state.clone();
        let rp_ui = rp_ui.clone();
        let weak = weak.clone();
        state.runtime.clone().spawn(async move {
            if rp_ui.active_tab() == RecentlyPlayedTab::MostPlayed {
                recently_played_ui_mod::refresh_grid(&state, &rp_ui, &weak).await;
                return;
            }
            if let Err(error) = recently_played_ui_mod::refresh_tracks(&state, &rp_ui, &weak).await
            {
                log::warn!("recently_played::retry: {}", describe(&error));
            }
        });
    });
}

fn wire_filter(ui: &AppWindow, state: &AppState, rp_ui: &Arc<RecentlyPlayedUi>) {
    let state = state.clone();
    let rp_ui = rp_ui.clone();
    let weak = ui.as_weak();
    ui.global::<RecentlyPlayed>().on_filter_changed(move |text| {
        let generation = recently_played_ui_mod::set_filter(&rp_ui, &text);
        recently_played_ui_mod::apply_filtered_tracks(&rp_ui, &weak);

        let rp_ui = rp_ui.clone();
        let weak = weak.clone();
        state.runtime.spawn(async move {
            recently_played_ui_mod::apply_filtered_grid_settled(&rp_ui, &weak, generation);
        });
    });
}

fn wire_selection(ui: &AppWindow, rp_ui: &Arc<RecentlyPlayedUi>) {
    let g = ui.global::<RecentlyPlayed>();
    {
        let weak = ui.as_weak();
        let rp_ui = rp_ui.clone();
        g.on_select_row(move |idx, id, shift, ctrl| {
            let Some(ui) = weak.upgrade() else { return };
            recently_played_ui_mod::handle_select_row(&ui, &rp_ui, idx, id, shift, ctrl);
        });
    }
    {
        let weak = ui.as_weak();
        let rp_ui = rp_ui.clone();
        g.on_select_all(move || {
            let Some(ui) = weak.upgrade() else { return };
            recently_played_ui_mod::select_all(&ui, &rp_ui);
        });
    }
    {
        let weak = ui.as_weak();
        let rp_ui = rp_ui.clone();
        g.on_clear_selection(move || {
            let Some(ui) = weak.upgrade() else { return };
            recently_played_ui_mod::clear_selection(&ui, &rp_ui);
        });
    }
}

fn wire_shuffle(g: &RecentlyPlayed<'_>, state: &AppState, rp_ui: &Arc<RecentlyPlayedUi>) {
    let state = state.clone();
    let rp_ui = rp_ui.clone();
    g.on_shuffle_all(move || {
        let tracks = rp_ui.filtered_cloud_tracks();
        let Some(selected) = tracks.first().map(|track| track.id) else { return };
        play_recent_queue(&state, tracks, selected, CloudQueueShuffle::Shuffle);
    });
}
