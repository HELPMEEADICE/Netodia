//! Favorites section lifecycle: the `section-active-changed` enter/leave handler, the
//! `library_changed` subscriber, and the first-enter initial fetch. See [`super::wire`].

use std::sync::Arc;

use async_compat::Compat;
use slint::ComponentHandle;

use crate::ui::detail_view::{release_hero_slots, release_shared_hero};
use crate::ui::favorites::NAV_FAVORITES;
use crate::ui::favorites::{self as favorites_ui_mod, FavoritesUi};
use crate::ui::model_diff::clear_vec_model;
use crate::ui::tab_bar::UNFETCHED_COUNT;
use netodia_app::state::AppState;
use netodia_core::error::describe;
use netodia_ui::{
    AppWindow, CardSelection, EntityGridRow as UiEntityGridRow, Favorites, Nav,
    TrackListRow as UiTrackListRow,
};

/// Wire the Favorites section-lifecycle callbacks.
pub(super) fn wire(ui: &AppWindow, state: &AppState, fav_ui: &Arc<FavoritesUi>) {
    let g = ui.global::<Favorites>();
    let weak = ui.as_weak();

    // --- Section-active mirror + cache release / re-enter --------
    // Seed the synchronous shadow from the current nav state — it has to be right on its own, the
    // gate firing only on a later difference (`.claude/rules/ui-patterns.md`'s `SectionActiveGate`
    // bullet). `boot::ui_setup::views::install_views` hydrates the persisted nav index before any
    // `wire_*` runs, so the read below sees it; the sibling `active_tab` shadow is
    // `favorites::seed_tab`'s.
    fav_ui.set_section_active(ui.global::<Nav>().get_selected_index() == NAV_FAVORITES);
    {
        let fu = fav_ui.clone();
        let s = state.clone();
        let weak = weak.clone();
        g.on_section_active_changed(move |active| {
            fu.set_section_active(active);
            if !active {
                // Land synchronously on the UI thread *before* the release task is spawned — the
                // re-enter handler reads this through `take_dirty()` and must never observe stale
                // data because release hadn't run yet.
                fu.mark_dirty();
                fu.finish_visit();
            }
            if !active && let Some(ui) = weak.upgrade() {
                // UI-thread teardown: hand back the banner's cover and blur slots so their
                // `SharedPixelBuffer` Arcs release immediately — the dual blur slots hold their
                // own refs even after the LRU drops — and empty the models so their
                // `SharedString`s go on this tick.
                let g = ui.global::<Favorites>();
                release_hero_slots(&g);
                // Same tick as the wipe above, and unconditional: `release_section_state` bails
                // when the user has already come back, so leaving the guard to it can strand the
                // hero on the bare gradient floor until the next channel tick.
                fu.forget_mosaic();
                // Same tick, same reason: the models are emptied below, so a surviving signature
                // would match the identical data on re-enter and skip the refill.
                fu.forget_grid_signature();
                // Six heroes share one colour set and one chip row, so hand both back rather than
                // leaving this banner's solve for the next hero to paint under.
                release_shared_hero(&ui);
                // Rewind all three counts on the same tick as the models they number: a count
                // that outlives its model is the one thing these surfaces can state that is
                // *wrong* rather than merely absent. `track-count` is the visible one, the hero
                // square reading it to pick its fallback glyph.
                g.set_track_count(UNFETCHED_COUNT);
                g.set_most_played_count(UNFETCHED_COUNT);
                g.set_artist_count(UNFETCHED_COUNT);
                g.set_cloud_loading(false);
                g.set_cloud_error(slint::SharedString::default());
                g.set_album_rows(slint::ModelRc::default());
                g.set_albums_loading(false);
                g.set_albums_error(slint::SharedString::default());
                // The Slint projection is temporary; `FavoritesUi.tracks_all` retains the last
                // successful cloud snapshot for immediate paint on the next visit.
                clear_vec_model::<UiTrackListRow>(&g.get_tracks(), "favorites: clear tracks");
                clear_vec_model::<UiEntityGridRow>(
                    &g.get_most_played_rows(),
                    "favorites: clear most-played",
                );
                clear_vec_model::<UiEntityGridRow>(&g.get_artist_rows(), "favorites: clear artist");
                clear_vec_model::<i32>(&g.get_selected_ids(), "favorites: clear selected-ids");
                g.set_selection_anchor(-1);
                // The card grids' set goes back with the models it describes;
                // `card-selection.slint` argues why it cannot outlive the leave.
                ui.global::<CardSelection>().invoke_clear();
            }
            let fu = fu.clone();
            let s = s.clone();
            let weak = weak.clone();
            if active {
                if fu.active_tab() == favorites_ui_mod::FavoritesTab::Songs
                    && let Some(ui) = weak.upgrade()
                {
                    let g = ui.global::<Favorites>();
                    favorites_ui_mod::apply_filtered_tracks_now(&ui, &fu);
                    let count = fu.state().tracks_all.snapshot().total();
                    g.set_track_count(count);
                    favorites_ui_mod::hero::restore_now(&ui, &fu);
                    g.set_cloud_loading(true);
                    g.set_cloud_error(slint::SharedString::default());
                }
                let runtime = s.runtime.clone();
                favorites_ui_mod::albums::paint_on_enter(&weak, &fu);
                runtime.spawn(async move {
                    let dirty = fu.take_dirty();
                    if dirty || fu.active_tab() == favorites_ui_mod::FavoritesTab::Songs {
                        kick_full_refresh(&s, &fu, &weak).await;
                    }
                });
            } else {
                // Off-thread heavy release — runs after the UI-thread teardown so the LRU drop
                // pairs with already-released Slint properties and no Arc ref lingers.
                let runtime = s.runtime.clone();
                runtime.spawn_blocking(move || fu.release_section_state());
            }
        });
    }

    {
        let state = state.clone();
        let fav_ui = fav_ui.clone();
        let weak = weak.clone();
        g.on_retry_cloud(move || {
            if !fav_ui.section_active() {
                return;
            }
            let state = state.clone();
            let fav_ui = fav_ui.clone();
            let weak = weak.clone();
            let runtime = state.runtime.clone();
            runtime.spawn(async move {
                if let Err(error) = favorites_ui_mod::refresh_tracks(&state, &fav_ui, &weak).await {
                    log::warn!("favorites::retry: {}", describe(&error));
                }
            });
        });
    }

    wire_account_updates(state, fav_ui, &weak);
    wire_liked_updates(state, fav_ui, &weak);

    // First section enter: kick an initial fetch so the page paints with data rather than empty
    // models. Re-entering Songs always refreshes behind the cached rows; other tabs use dirty
    // flags to avoid fetching data the user did not revisit.
    fav_ui.mark_dirty();
    if fav_ui.section_active() {
        let s = state.clone();
        let fu = fav_ui.clone();
        let weak = weak.clone();
        state.runtime.clone().spawn(async move {
            let dirty = fu.take_dirty();
            if dirty || fu.active_tab() == favorites_ui_mod::FavoritesTab::Songs {
                kick_full_refresh(&s, &fu, &weak).await;
            }
        });
    }
}

/// Clear and reload private data whenever the signed-in `NetEase` account changes.
fn wire_account_updates(
    state: &AppState,
    fav_ui: &Arc<FavoritesUi>,
    weak: &slint::Weak<AppWindow>,
) {
    let state = state.clone();
    let fav_ui = fav_ui.clone();
    let weak = weak.clone();
    let mut account_rx = state.cloud_playback.subscribe_account();
    let _ = slint::spawn_local(Compat::new(async move {
        loop {
            if account_rx.changed().await.is_err() {
                break;
            }
            clear_account_data(&fav_ui, &weak);
            if !fav_ui.section_active() {
                fav_ui.mark_dirty();
                continue;
            }
            if fav_ui.active_tab() == favorites_ui_mod::FavoritesTab::Songs
                && let Err(error) = favorites_ui_mod::refresh_tracks(&state, &fav_ui, &weak).await
            {
                log::warn!("favorites::account_changed: {}", describe(&error));
            }
        }
    }));
}

/// Keep the liked-song cache current when the player bar changes membership in `NetEase`.
fn wire_liked_updates(state: &AppState, fav_ui: &Arc<FavoritesUi>, weak: &slint::Weak<AppWindow>) {
    let state = state.clone();
    let fav_ui = fav_ui.clone();
    let weak = weak.clone();
    let mut changed = state.cloud_liked_changed.subscribe();
    let runtime = state.runtime.clone();
    runtime.spawn(async move {
        while changed.changed().await.is_ok() {
            fav_ui.mark_songs_dirty();
            if !fav_ui.section_active()
                || fav_ui.active_tab() != favorites_ui_mod::FavoritesTab::Songs
            {
                continue;
            }
            if let Some(liked) = state.cloud.cached_liked_ids() {
                let needs_fetch = fav_ui.reconcile_likes(&liked);
                favorites_ui_mod::apply_filtered_tracks(&fav_ui, &weak);
                // A toggle of an existing row needs no whole-list request or row reordering.
                if !needs_fetch {
                    let fav_ui = fav_ui.clone();
                    let _ = weak.upgrade_in_event_loop(move |ui| {
                        if fav_ui.section_active() {
                            ui.global::<Favorites>().set_cloud_loading(false);
                        }
                    });
                    continue;
                }
            }
            if let Err(error) = favorites_ui_mod::refresh_tracks(&state, &fav_ui, &weak).await {
                log::warn!("favorites::liked_changed: {}", describe(&error));
            }
        }
    });
}

/// Remove the previous account's private rows before a new request can publish.
fn clear_account_data(fav_ui: &FavoritesUi, weak: &slint::Weak<AppWindow>) {
    {
        let _gate = fav_ui.gate();
        fav_ui.state().tracks_all.clear();
        fav_ui.state().hero_cache.lock().clear();
        fav_ui.state().retained_tracks.lock().clear();
        *fav_ui.state().songs_fold.lock() = crate::ui::hero_folds::HeroFold::default();
        *fav_ui.state().stats.lock() = netodia_core::entities::track::FavoriteStats {
            count: 0,
            total_duration_ms: 0,
            artwork_paths: Vec::new(),
        };
        fav_ui.state().applied_selection.lock().clear();
    }
    fav_ui.forget_mosaic();
    let weak = weak.clone();
    let _ = weak.upgrade_in_event_loop(|ui| {
        let g = ui.global::<Favorites>();
        release_hero_slots(&g);
        release_shared_hero(&ui);
        g.set_track_count(UNFETCHED_COUNT);
        g.set_cloud_loading(false);
        g.set_cloud_error(slint::SharedString::default());
        clear_vec_model::<UiTrackListRow>(&g.get_tracks(), "favorites: clear account tracks");
        clear_vec_model::<i32>(&g.get_selected_ids(), "favorites: clear account selection");
        g.set_selection_anchor(-1);
    });
}

/// Refresh the mounted cloud Favorites list and its hero state.
async fn kick_full_refresh(
    state: &AppState,
    fav_ui: &Arc<FavoritesUi>,
    weak: &slint::Weak<AppWindow>,
) {
    if fav_ui.active_tab() == favorites_ui_mod::FavoritesTab::Albums {
        favorites_ui_mod::albums::refresh(state, fav_ui, weak).await;
        return;
    }
    fav_ui.take_songs_dirty();
    if let Err(error) = favorites_ui_mod::refresh_tracks(state, fav_ui, weak).await {
        log::warn!("favorites::refresh_tracks: {}", describe(&error));
    }
}
