//! Recently-Played section lifecycle: the `section-active-changed` enter/leave
//! handler (cache release + re-fetch), account and liked-song updates,
//! and the first-enter initial fetch. See
//! [`super::wire`].

use std::sync::Arc;

use async_compat::Compat;
use slint::ComponentHandle;

use crate::ui::detail_view::{release_hero_slots, release_shared_hero};
use crate::ui::model_diff::clear_vec_model;
use crate::ui::recently_played::NAV_RECENTLY_PLAYED;
use crate::ui::recently_played::{
    self as recently_played_ui_mod, RecentlyPlayedTab, RecentlyPlayedUi,
};
use crate::ui::tab_bar::UNFETCHED_COUNT;
use netodia_app::state::AppState;
use netodia_core::error::describe;
use netodia_ui::{
    AppWindow, CardSelection, EntityGridRow as UiEntityGridRow, Nav, RecentlyPlayed,
    TrackListRow as UiTrackListRow,
};

/// Wire the Recently-Played section-lifecycle callbacks.
pub(super) fn wire(ui: &AppWindow, state: &AppState, rp_ui: &Arc<RecentlyPlayedUi>) {
    let g = ui.global::<RecentlyPlayed>();
    let weak = ui.as_weak();

    // --- Section-active mirror + cache release / re-enter ---------
    // Seed the synchronous shadow from the current nav state: the gate's
    // `ChangeTracker` baselines inside `AppWindow::new()` and fires only on a
    // later difference, so a section the boot doesn't land on gets no edge at
    // all, and the one it does land on gets its edge a frame late — after
    // boot has already read this shadow. See the `SectionActiveGate` bullet
    // in `.claude/rules/ui-patterns.md`.
    rp_ui.set_section_active(ui.global::<Nav>().get_selected_index() == NAV_RECENTLY_PLAYED);
    {
        let ru = rp_ui.clone();
        let s = state.clone();
        let weak = weak.clone();
        g.on_section_active_changed(move |active| {
            ru.set_section_active(active);
            if !active {
                // Land synchronously before the release task is spawned so the
                // re-enter handler's `take_dirty()` never observes stale data.
                ru.mark_dirty();
                ru.begin_tracks_refresh();
                ru.begin_grid_refresh();
                ru.mark_grid_dirty();
            }
            if !active && let Some(ui) = weak.upgrade() {
                // UI-thread teardown: hand back the banner's cover and blur slots so their
                // `SharedPixelBuffer` Arcs release immediately, and empty the
                // models so their `SharedString`s drop on the same tick as the
                // LRU release below.
                let g = ui.global::<RecentlyPlayed>();
                release_hero_slots(&g);
                // Unconditional, on the same tick as the wipe — see the
                // matching call in `favorites/lifecycle.rs`.
                ru.forget_mosaic();
                // Same tick, same reason: the models are emptied below, so a
                // surviving signature would match the identical data on re-enter
                // and skip the refill that fills them back in.
                ru.forget_grid_signature();
                // Six heroes share one colour set and one chip row, so hand
                // both back rather than leaving this banner's solve and this
                // view's counts for the next hero to paint under.
                release_shared_hero(&ui);
                // Rewind both counts to "not fetched yet" on the same tick
                // as the models they number, for the reason the folds are reset
                // beside their caches: a count that outlives its model is the
                // one thing these surfaces can state that is *wrong* rather
                // than merely absent. `track-count` is the visible one — the
                // hero square reads it to pick its fallback glyph.
                g.set_track_count(UNFETCHED_COUNT);
                g.set_most_played_count(UNFETCHED_COUNT);
                g.set_cloud_loading(false);
                g.set_most_played_loading(false);
                g.set_most_played_error(slint::SharedString::default());
                g.set_cloud_error(slint::SharedString::default());
                clear_vec_model::<UiTrackListRow>(&g.get_tracks(), "recently_played: clear tracks");
                clear_vec_model::<UiEntityGridRow>(
                    &g.get_most_played_rows(),
                    "recently_played: clear most-played",
                );
                clear_vec_model::<i32>(
                    &g.get_selected_ids(),
                    "recently_played: clear selected-ids",
                );
                g.set_selection_anchor(-1);
                // The card grid's set goes back with the models it describes;
                // `card-selection.slint` argues why it cannot outlive the leave.
                ui.global::<CardSelection>().invoke_clear();
            }
            let ru = ru.clone();
            let s = s.clone();
            let weak = weak.clone();
            if active {
                if ru.active_tab() == RecentlyPlayedTab::Songs
                    && let Some(ui) = weak.upgrade()
                {
                    recently_played_ui_mod::restore_cached_tracks_now(&ui, &ru);
                }
                let runtime = s.runtime.clone();
                runtime.spawn(async move {
                    if ru.take_dirty() {
                        kick_full_refresh(&s, &ru, &weak).await;
                    }
                });
            } else {
                let runtime = s.runtime.clone();
                runtime.spawn_blocking(move || ru.release_section_state());
            }
        });
    }

    wire_account_updates(state, rp_ui, &weak);
    wire_liked_updates(state, rp_ui, &weak);

    // First section enter: kick an initial fetch so the page paints with data.
    // If the session starts elsewhere, `section-active-changed` triggers the
    // same fetch via `take_dirty()`.
    rp_ui.mark_dirty();
    if rp_ui.section_active() {
        let s = state.clone();
        let ru = rp_ui.clone();
        let weak = weak.clone();
        state.runtime.clone().spawn(async move {
            if ru.take_dirty() {
                kick_full_refresh(&s, &ru, &weak).await;
            }
        });
    }
}

/// Remove the prior account's private rows and load the newly signed-in account's history.
fn wire_account_updates(
    state: &AppState,
    rp_ui: &Arc<RecentlyPlayedUi>,
    weak: &slint::Weak<AppWindow>,
) {
    let state = state.clone();
    let rp_ui = rp_ui.clone();
    let weak = weak.clone();
    let mut account_rx = state.cloud_playback.subscribe_account();
    let _ = slint::spawn_local(Compat::new(async move {
        while account_rx.changed().await.is_ok() {
            clear_account_data(&rp_ui, &weak);
            if !rp_ui.section_active() {
                rp_ui.mark_dirty();
                continue;
            }
            kick_full_refresh(&state, &rp_ui, &weak).await;
        }
    }));
}

/// Keep recent-song hearts synchronized with changes from either the player bar or this list.
fn wire_liked_updates(
    state: &AppState,
    rp_ui: &Arc<RecentlyPlayedUi>,
    weak: &slint::Weak<AppWindow>,
) {
    let state = state.clone();
    let rp_ui = rp_ui.clone();
    let weak = weak.clone();
    let mut changed = state.cloud_liked_changed.subscribe();
    let runtime = state.runtime.clone();
    runtime.spawn(async move {
        while changed.changed().await.is_ok() {
            if rp_ui.state().tracks_all.lock().is_empty()
                && rp_ui.state().most_played.lock().is_empty()
            {
                continue;
            }
            if let Err(error) =
                recently_played_ui_mod::refresh_liked_flags(&state, &rp_ui, &weak).await
            {
                log::debug!("recently_played::liked_changed: {}", describe(&error));
            }
        }
    });
}

fn clear_account_data(rp_ui: &RecentlyPlayedUi, weak: &slint::Weak<AppWindow>) {
    rp_ui.clear_account_tracks();
    rp_ui.forget_mosaic();
    let weak = weak.clone();
    let _ = weak.upgrade_in_event_loop(|ui| {
        let g = ui.global::<RecentlyPlayed>();
        release_hero_slots(&g);
        release_shared_hero(&ui);
        g.set_track_count(UNFETCHED_COUNT);
        g.set_most_played_count(UNFETCHED_COUNT);
        g.set_most_played_loading(false);
        g.set_most_played_error(slint::SharedString::default());
        clear_vec_model::<UiEntityGridRow>(
            &g.get_most_played_rows(),
            "recently_played: clear account ranking",
        );
        ui.global::<CardSelection>().invoke_clear();
        g.set_cloud_loading(false);
        g.set_cloud_error(slint::SharedString::default());
        clear_vec_model::<UiTrackListRow>(&g.get_tracks(), "recently_played: clear account tracks");
        clear_vec_model::<i32>(&g.get_selected_ids(), "recently_played: clear account selection");
        g.set_selection_anchor(-1);
    });
}

/// Refresh account history on entry for the shared hero, and fetch the listening ranking only
/// when its tab is mounted. Local library statistics have no bearing on either cloud snapshot.
async fn kick_full_refresh(
    state: &AppState,
    rp_ui: &Arc<RecentlyPlayedUi>,
    weak: &slint::Weak<AppWindow>,
) {
    if rp_ui.active_tab() == RecentlyPlayedTab::MostPlayed {
        // This fetch settles the dirty flag for the ranking and refreshes cloud history for the
        // shared hero without mounting the Songs list.
        rp_ui.take_grid_dirty();
        let (_grid, history) = tokio::join!(
            recently_played_ui_mod::refresh_grid(state, rp_ui, weak),
            recently_played_ui_mod::refresh_tracks(state, rp_ui, weak),
        );
        if let Err(error) = history {
            log::warn!("recently_played::refresh_tracks: {}", describe(&error));
        }
    } else {
        rp_ui.mark_grid_dirty();
        if let Err(error) = recently_played_ui_mod::refresh_tracks(state, rp_ui, weak).await {
            log::warn!("recently_played::refresh_tracks: {}", describe(&error));
        }
    }
}
