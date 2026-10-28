//! Account-library categories and the three continuous roaming entry points.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use slint::ComponentHandle;
use tokio_util::sync::CancellationToken;

use netodia_app::cloud_roaming::{self, RoamingRequest};
use netodia_app::services::view_state::mutate_view_state;
use netodia_app::state::AppState;
use netodia_core::entities::cloud::{DAILY_PLAYLIST_ID, PERSONAL_RADAR_ID, RoamingMode};
use netodia_core::error::describe;
use netodia_engine::player::engine::state::lock_state;
use netodia_ui::{AppWindow, MyLibrary, PlaylistDetail, Playlists};

use super::super::{PlaylistsUi, fetch_grid, open_playlist};
use crate::ui::callbacks::index_persist::IndexPersist;

pub(super) fn wire(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    let weak = ui.as_weak();
    let s = state.clone();
    let pu = playlists_ui.clone();
    let persist = Arc::new(IndexPersist::new(ui.global::<Playlists>().get_category()));
    ui.global::<Playlists>().on_category_changed(move |category| {
        let Some(ui) = weak.upgrade() else { return };
        let category =
            crate::ui::tab_bar::clamp_tab(category, ui.global::<Playlists>().get_category_count());
        let grid_request = {
            let _gate = pu.section.gate();
            pu.category.store(category, Ordering::Release);
            pu.grid_request.fetch_add(1, Ordering::AcqRel).wrapping_add(1)
        };
        persist.publish(category);
        pu.detail_request.fetch_add(1, Ordering::AcqRel);
        let detail = ui.global::<PlaylistDetail>();
        if detail.get_playlist_id() >= 0 {
            detail.invoke_close_detail();
        }
        let library = ui.global::<MyLibrary>();
        library.set_tab_idx(library.get_tab_playlists());
        library.invoke_tab_changed(library.get_tab_playlists());
        let playlists = ui.global::<Playlists>();
        playlists.set_cloud_error(slint::SharedString::default());
        playlists.set_retry_special(-1);
        let fresh = super::super::grid::restore_cached_grid(&ui, &pu, s.cloud.status().generation);
        playlists.set_cloud_loading(!fresh);
        let s = s.clone();
        let pu = pu.clone();
        let weak = weak.clone();
        let persist = persist.clone();
        s.runtime.clone().spawn(async move {
            let paths = s.paths.clone();
            let saved = tokio::task::spawn_blocking(move || {
                persist.write_if_current(category, || {
                    if let Err(error) = mutate_view_state(&paths, |view| {
                        view.music_library_category = category;
                    }) {
                        log::warn!("library category persistence: {}", describe(&error));
                    }
                });
            })
            .await;
            if let Err(error) = saved {
                log::warn!("library category persistence: {}", describe(&error));
            }
            if !fresh
                && pu.grid_request_current(grid_request)
                && let Err(error) = fetch_grid(&s, &pu, weak).await
            {
                log::warn!("library category: {}", describe(&error));
            }
        });
    });

    let weak = ui.as_weak();
    let s = state.clone();
    let pu = playlists_ui.clone();
    ui.global::<Playlists>().on_open_special(move |kind| {
        let id = match kind {
            0 => DAILY_PLAYLIST_ID,
            1 => PERSONAL_RADAR_ID,
            _ => return,
        };
        let weak = weak.clone();
        let s = s.clone();
        let pu = pu.clone();
        if let Some(ui) = weak.upgrade() {
            ui.global::<Playlists>().set_cloud_loading(true);
            ui.global::<Playlists>().set_cloud_error(slint::SharedString::default());
            ui.global::<Playlists>().set_retry_special(kind);
        }
        s.runtime.clone().spawn(async move {
            let request = pu.detail_request.load(Ordering::Acquire).wrapping_add(1);
            let result =
                open_playlist(&s, &pu, weak.clone(), id, netodia_ui::NavEnterFrom::Right).await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if pu.detail_request.load(Ordering::Acquire) != request {
                    return;
                }
                let playlists = ui.global::<Playlists>();
                playlists.set_cloud_loading(false);
                match result {
                    Err(error) => playlists.set_cloud_error(describe(&error).into()),
                    Ok(()) => playlists.set_retry_special(-1),
                }
            });
        });
    });

    wire_roaming(ui, state, playlists_ui);
}

fn wire_roaming(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    let s = state.clone();
    let pu = playlists_ui.clone();
    let weak = ui.as_weak();
    ui.global::<Playlists>().on_start_roaming(move |index| {
        let mode = match index {
            0 => RoamingMode::Personal,
            1 => RoamingMode::Heart,
            2 => RoamingMode::Similar,
            _ => return,
        };
        let Some(ui) = weak.upgrade() else { return };
        let cancellation = CancellationToken::new();
        pu.roaming_cancel.lock().cancel();
        *pu.roaming_cancel.lock() = cancellation.clone();
        let request_id = pu.roaming_request.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        let seed = lock_state(&s.player_state)
            .source
            .as_ref()
            .filter(|track| track.file_path.is_empty())
            .map(|track| track.id);
        let playlists = ui.global::<Playlists>();
        playlists.set_roaming_mode(index);
        playlists.set_roaming_loading(true);
        playlists.set_roaming_error(slint::SharedString::default());
        let (queue_generation, playback_generation) = s.cloud_playback.reserve_roaming_playback();
        let s = s.clone();
        let pu = pu.clone();
        let weak = weak.clone();
        s.runtime.clone().spawn(async move {
            let started_ui = weak.clone();
            let started_pu = pu.clone();
            let result = cloud_roaming::run(
                &s,
                RoamingRequest { mode, seed, cancellation, queue_generation, playback_generation },
                move || {
                    let _ = started_ui.upgrade_in_event_loop(move |ui| {
                        if started_pu.roaming_request.load(Ordering::Acquire) == request_id {
                            ui.global::<Playlists>().set_roaming_loading(false);
                        }
                    });
                },
            )
            .await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if pu.roaming_request.load(Ordering::Acquire) != request_id {
                    return;
                }
                let playlists = ui.global::<Playlists>();
                playlists.set_roaming_loading(false);
                playlists.set_roaming_mode(-1);
                if let Err(error) = result {
                    playlists.set_roaming_error(describe(&error).into());
                }
            });
        });
    });
}
