//! Playlists section lifecycle: the `section-active-changed` enter/leave
//! handler (cache release + re-fetch) and the `library_changed` subscriber
//! that keeps the grid + open detail fresh on watcher / scan / CRUD events.

use std::sync::Arc;

use async_compat::Compat;
use slint::ComponentHandle;

use crate::ui::callbacks::macros::spawn_logged;
use crate::ui::detail_view::release_detail_hero_images;
use crate::ui::grid_prewarm;
use crate::ui::model_diff::clear_vec_model;
use crate::ui::my_library::{MyLibraryTab, tab_is_mounted};
use crate::ui::playlists::{self as playlists_ui_mod, PlaylistsUi};
use crate::ui::signal::on_signal;
use crate::ui::tab_bar::UNFETCHED_COUNT;
use crate::ui::track_detail::TrackDetail;
use netodia_app::state::AppState;
use netodia_core::error::describe;
use netodia_ui::{
    AppWindow, CardSelection, PlaylistDetail, PlaylistGridRow as UiPlaylistGridRow,
    PlaylistRow as UiPlaylistRow, Playlists, TrackListRow as UiTrackListRow,
};

/// Wire the Playlists section-lifecycle callbacks. See
/// [`super::wire`].
pub(super) fn wire(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    playlists_ui.set_section_active(tab_is_mounted(ui, MyLibraryTab::Playlists));
    // See the matching seed in `albums/lifecycle.rs`: a boot pre-fetch for a
    // section that isn't on screen can't publish the shared hero globals, so
    // its first enter has to re-fetch rather than take the cheap path.
    if !playlists_ui.section_active() {
        playlists_ui.mark_dirty();
    }
    wire_section_gate(ui, state, playlists_ui);
    install_library_refresher(ui, state, playlists_ui);
    install_stats_refresher(ui, state, playlists_ui);
    install_account_refresher(ui, state, playlists_ui);
    install_liked_refresher(ui, state, playlists_ui);
}

fn install_liked_refresher(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    let s = state.clone();
    let pu = playlists_ui.clone();
    if let Err(error) =
        on_signal(&state.cloud_liked_changed, ui.as_weak(), "playlist-likes", move |ui| {
            let g = ui.global::<PlaylistDetail>();
            if !g.get_cloud_collection() {
                return;
            }
            if !pu.section_active() {
                pu.mark_dirty();
                return;
            }
            super::super::detail::refresh_cloud_likes(&g, &pu, &s);
        })
    {
        log::warn!("Playlists won't follow cloud likes: {}", describe(&error));
    }
}

/// Clear saved playlist rows as soon as the account changes, then reload for the active session.
fn install_account_refresher(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    let s = state.clone();
    let pu = playlists_ui.clone();
    let weak = ui.as_weak();
    let mut account_rx = state.cloud_playback.subscribe_account();
    state.runtime.clone().spawn(Compat::new(async move {
        loop {
            if account_rx.changed().await.is_err() {
                break;
            }
            pu.clear_account_data();
            pu.mark_dirty();
            let weak_ui = weak.clone();
            let _ = weak_ui.upgrade_in_event_loop(move |ui| {
                let playlists = ui.global::<Playlists>();
                playlists.set_total_count(UNFETCHED_COUNT);
                playlists.set_cloud_loading(false);
                playlists.set_cloud_error(slint::SharedString::default());
                clear_vec_model::<UiPlaylistGridRow>(
                    &playlists.get_grid_rows(),
                    "playlists: clear account grid",
                );
                clear_vec_model::<UiPlaylistRow>(
                    &playlists.get_rows(),
                    "playlists: clear account rows",
                );
                let detail = ui.global::<PlaylistDetail>();
                detail.set_playlist_id(-1);
                detail.set_cloud_collection(false);
                detail.set_loading(false);
                clear_vec_model::<UiTrackListRow>(
                    &detail.get_tracks(),
                    "playlists: clear account detail",
                );
                clear_vec_model::<i32>(
                    &detail.get_selected_ids(),
                    "playlists: clear account selection",
                );
                detail.set_selection_anchor(-1);
            });
            if !pu.section_active() {
                continue;
            }
            if let Err(error) = playlists_ui_mod::fetch_grid(&s, &pu, weak.clone()).await {
                log::warn!("playlists::account_changed: {}", describe(&error));
            }
        }
    }));
}

/// Mirrors the Albums implementation — on leave, wipe the Slint models (UI
/// thread) then release the Rust-side caches off-thread; on return,
/// [`reenter`].
fn wire_section_gate(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    let pu = playlists_ui.clone();
    let s = state.clone();
    let weak = ui.as_weak();
    ui.global::<Playlists>().on_section_active_changed(move |active| {
        pu.set_section_active(active);
        if !active {
            pu.mark_dirty();
        }
        if !active && let Some(ui) = weak.upgrade() {
            let g = ui.global::<Playlists>();
            // Rewound on the same tick as the model it numbers;
            // `Albums.total-count`'s declaration argues the sentinel.
            g.set_total_count(UNFETCHED_COUNT);
            clear_vec_model::<UiPlaylistGridRow>(&g.get_grid_rows(), "playlists: clear grid");

            let d = ui.global::<PlaylistDetail>();
            release_detail_hero_images(&ui, &d);
            clear_vec_model::<UiTrackListRow>(&d.get_tracks(), "playlists: clear detail tracks");
            clear_vec_model::<i32>(&d.get_selected_ids(), "playlists: clear detail selection");
            d.set_selection_anchor(-1);
            // The card grid's set goes back with the models it describes;
            // `card-selection.slint` argues why it cannot outlive the leave.
            ui.global::<CardSelection>().invoke_clear();
        }
        if active {
            let fresh = weak.upgrade().is_some_and(|ui| {
                super::super::grid::restore_cached_grid(&ui, &pu, s.cloud.status().generation)
            });
            s.runtime.spawn(reenter(s.clone(), pu.clone(), weak.clone(), fresh));
        } else {
            let pu = pu.clone();
            s.runtime.spawn_blocking(move || pu.release_section_state());
        }
    });
}

/// Restore fresh snapshots without a request; refresh missing or expired snapshots.
async fn reenter(s: AppState, pu: Arc<PlaylistsUi>, weak: slint::Weak<AppWindow>, fresh: bool) {
    let dirty = pu.take_dirty();
    if !dirty && fresh {
        grid_prewarm::prewarm_off_thread(&pu, PlaylistsUi::prewarm_visible_covers).await;
        return;
    }
    let open_id = pu.detail_playlist_id();
    if fresh {
        grid_prewarm::prewarm_off_thread(&pu, PlaylistsUi::prewarm_visible_covers).await;
    }
    if !fresh && let Err(e) = playlists_ui_mod::fetch_grid(&s, &pu, weak.clone()).await {
        log::warn!("playlists::section_enter fetch_grid: {}", describe(&e));
    }
    if open_id >= 0
        && let Err(e) = playlists_ui_mod::open_playlist(
            &s,
            &pu,
            weak.clone(),
            open_id,
            netodia_ui::NavEnterFrom::Right,
        )
        .await
    {
        log::warn!("playlists::section_enter open_playlist({open_id}): {}", describe(&e));
        pu.clear_detail();
        // Handed back warm, as a failed restore's grid is.
        grid_prewarm::prewarm_off_thread(&pu, PlaylistsUi::prewarm_visible_covers).await;
        let _ = weak.upgrade_in_event_loop(|ui| {
            let g = ui.global::<PlaylistDetail>();
            g.set_playlist_id(-1);
            release_detail_hero_images(&ui, &g);
        });
    }
}

/// Playlist mutations from elsewhere (CRUD, scans, watcher) bump
/// `library_changed`. Mirror the Albums pattern: re-fetch the grid (so new
/// playlists appear and removed ones disappear) and refresh an open detail.
fn install_library_refresher(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    let s = state.clone();
    let pu = playlists_ui.clone();
    let installed =
        on_signal(&state.library_changed, ui.as_weak(), "playlists-library-changed", move |ui| {
            pu.grid.categories.lock().clear();
            // Skip the in-place refresh when the section is hidden, but
            // mark the cached data dirty so the next section-enter
            // re-fetches from scratch. Without the `mark_dirty`, a
            // `library_changed` arriving while the section was never
            // visited (e.g. the first scan after a fresh-DB launch) would
            // be lost. Mirrors the same gate in `albums/callbacks/lifecycle.rs`.
            if !pu.section_active() {
                pu.mark_dirty();
                return;
            }
            let open_id = pu.detail_playlist_id();
            {
                let s = s.clone();
                let pu = pu.clone();
                let weak = ui.as_weak();
                spawn_logged!(
                    s,
                    "playlists::library_changed",
                    playlists_ui_mod::fetch_grid(&s, &pu, weak)
                );
            }
            if open_id >= 0 {
                let s = s.clone();
                let pu = pu.clone();
                let weak = ui.as_weak();
                spawn_logged!(
                    s,
                    "playlists::library_changed_detail",
                    playlists_ui_mod::refresh_detail(&s, &pu, weak, open_id)
                );
            }
        });
    if let Err(e) = installed {
        log::warn!("Playlists won't follow library changes: {}", describe(&e));
    }
}

/// Play-count / last-played flushes bump `stats_changed` (NOT
/// `library_changed`, by design, to avoid churning every view on each played
/// song). Only smart playlists whose rules depend on play stats ("most
/// played", "recently played", "never played") can change, so this is gated
/// on the presence of a *stat-dependent* smart playlist and the refresh
/// recounts only that subset — a static-rule ("Genre is Rock") smart playlist
/// is neither woken nor recounted here (it can't have moved).
fn install_stats_refresher(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    let s = state.clone();
    let pu = playlists_ui.clone();
    let installed =
        on_signal(&state.stats_changed, ui.as_weak(), "playlists-stats-changed", move |ui| {
            // Hidden section: the library_changed / section-enter path
            // already re-fetches on return, so nothing to do here. And
            // nothing to do unless a smart playlist could actually have
            // moved (stricter than a plain any-smart-playlist check).
            if !pu.section_active() || !pu.has_stat_dependent_smart_playlists() {
                return;
            }
            let open_id = pu.detail_playlist_id();
            let open_smart = open_id >= 0 && pu.is_playlist_smart_stat_dependent(open_id);
            {
                let s = s.clone();
                let pu = pu.clone();
                let weak = ui.as_weak();
                spawn_logged!(
                    s,
                    "playlists::stats_changed",
                    playlists_ui_mod::fetch_grid_stats(&s, &pu, weak)
                );
            }
            if open_smart {
                let s = s.clone();
                let pu = pu.clone();
                let weak = ui.as_weak();
                spawn_logged!(
                    s,
                    "playlists::stats_changed_detail",
                    playlists_ui_mod::refresh_detail(&s, &pu, weak, open_id)
                );
            }
        });
    if let Err(e) = installed {
        log::warn!("Smart playlists won't follow play counts: {}", describe(&e));
    }
}
