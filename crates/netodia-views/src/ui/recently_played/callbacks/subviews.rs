//! `RecentlyPlayed.*` card actions and sub-view routing: the Most Played
//! play-track and shuffle actions, the tab switch, and the grid's column-count
//! push. See [`super::wire`].

use std::sync::Arc;

use slint::ComponentHandle;

use crate::ui::callbacks::index_persist::IndexPersist;
use crate::ui::model_diff::clear_vec_model;
use crate::ui::recently_played::{self as recently_played_ui_mod, RecentlyPlayedUi};
use crate::ui::tab_bar::UNFETCHED_COUNT;
use netodia_app::cloud_playback::CloudQueueShuffle;
use netodia_app::library;
use netodia_app::state::AppState;
use netodia_core::error::describe;
use netodia_ui::{AppWindow, RecentlyPlayed, TrackListRow as UiTrackListRow};

/// Wire the card-action and sub-view-routing callbacks.
pub(super) fn wire(ui: &AppWindow, state: &AppState, rp_ui: &Arc<RecentlyPlayedUi>) {
    let g = ui.global::<RecentlyPlayed>();
    let weak = ui.as_weak();

    // play-track: clicking a Most Played card loads that grid into the queue and
    // starts on that card — the grid is the context, not the recency list on the
    // other tab. The callback carries the full cloud identity as a string.
    {
        let s = state.clone();
        let ru = rp_ui.clone();
        g.on_play_track(move |id| {
            let Some(id) = crate::ui::util::parse_cloud_id(id.as_str()) else { return };
            let tracks = ru.most_played_cloud_tracks();
            if !tracks.iter().any(|track| track.id == id) {
                return;
            }
            super::tracklist::play_recent_queue(&s, tracks, id, CloudQueueShuffle::Preserve);
        });
    }

    // --- Hero pill: Shuffle the Most Played grid -------------------
    {
        let s = state.clone();
        let ru = rp_ui.clone();
        g.on_shuffle_most_played(move || {
            let tracks = ru.most_played_cloud_tracks();
            let Some(first) = tracks.first().map(|track| track.id) else { return };
            super::tracklist::play_recent_queue(&s, tracks, first, CloudQueueShuffle::Shuffle);
        });
    }

    // --- Tab switch -----------------------------------------------
    // The bar has already moved `tab-idx` and cleared the Slint-side filter by
    // the time this runs, so this is the catch-up: drop the Rust filter shadow to
    // match, build the entering tab's model, then off-thread persist the pick.
    {
        let s = state.clone();
        let ru = rp_ui.clone();
        let weak = weak.clone();
        // Ordered: a bounce queues a value per pick and two blocking tasks have
        // none of their own, so a reversed pair reopens the page on a tab the
        // user only passed through.
        let persist = Arc::new(IndexPersist::new(g.get_tab_idx()));
        g.on_tab_changed(move |tab| {
            let Some(ui) = weak.upgrade() else { return };
            let g = ui.global::<RecentlyPlayed>();
            let entering = recently_played_ui_mod::tab_from_index(&g, tab);
            persist.publish(tab);
            // A sub-view pick moves no nav index, so `record_current` never
            // hears about it. The bar has already written `tab-idx`, so the tag
            // reads the tab being entered.
            crate::ui::view_tag::log_current(&ui);

            // Shadow first: the model build below and every later fetch decide
            // which model to fill and which tier to warm from it, and both can
            // run off the UI thread where the global isn't reachable.
            ru.set_active_tab(entering);
            recently_played_ui_mod::set_filter(&ru, "");

            // Asked *before* the apply below, because the apply is what has to know:
            // the answer decides whether the count it writes stands for anything.
            let needs_grid_fetch = entering
                == recently_played_ui_mod::RecentlyPlayedTab::MostPlayed
                && ru.take_grid_dirty();

            // Fill the entering tab's model on this tick — it mounts on the next
            // frame, and the `_now` variants are what make "this tick" true: a
            // hidden tab's model is empty, and the count that gates its empty
            // state is not, so a grid that paints before its rows land shows a
            // bare panel rather than a placeholder, and a list does the same with
            // a header band over nothing.
            recently_played_ui_mod::apply_filtered_grid_now(&ui, &ru);
            if entering == recently_played_ui_mod::RecentlyPlayedTab::Songs {
                recently_played_ui_mod::restore_cached_tracks_now(&ui, &ru);
                let s_refresh = s.clone();
                let ru_refresh = ru.clone();
                let weak_refresh = weak.clone();
                s.runtime.clone().spawn(async move {
                    if let Err(error) = recently_played_ui_mod::refresh_tracks(
                        &s_refresh,
                        &ru_refresh,
                        &weak_refresh,
                    )
                    .await
                    {
                        log::warn!("recently_played::tab_refresh: {}", describe(&error));
                    }
                });
            } else {
                // Leaving empties the model rather than leaving it holding its
                // last rows — the same trade `write_filtered_grid` makes for the
                // grid it isn't mounting, and for the same reason: a row per
                // track is one `TrackListRow` of `SharedString`s pinned behind a
                // tab the user has left. `apply_filtered_tracks` refuses to
                // refill it while that's true, so nothing puts them back.
                clear_vec_model::<UiTrackListRow>(
                    &g.get_tracks(),
                    "recently_played: leave songs tab",
                );
            }

            // Fetch after the synchronous apply so an existing snapshot paints immediately.
            // An unfetched cache cannot assert "nothing played" while the cloud request runs.
            if needs_grid_fetch {
                g.set_most_played_count(UNFETCHED_COUNT);
                let s_fetch = s.clone();
                let ru_fetch = ru.clone();
                let weak_fetch = weak.clone();
                s.runtime.spawn(async move {
                    recently_played_ui_mod::refresh_grid(&s_fetch, &ru_fetch, &weak_fetch).await;
                });
            }

            let s_disk = s.clone();
            let persist_disk = Arc::clone(&persist);
            s.runtime.spawn_blocking(move || {
                // A superseded tab must not overwrite a later pick on disk.
                persist_disk.write_if_current(tab, || {
                    if let Err(e) = library::settings::set_recently_played_tab(&s_disk.paths, tab) {
                        log::warn!("recently_played::set_recently_played_tab: {}", describe(&e));
                    }
                });
            });
        });
    }

    // --- Grid column count ----------------------------------------
    // A resize only changes how the cached cards are chunked, so this re-walks
    // the in-memory cache rather than touching the DB.
    {
        let ru = rp_ui.clone();
        let weak = weak.clone();
        g.on_columns_changed(move |_cols| {
            let Some(ui) = weak.upgrade() else { return };
            recently_played_ui_mod::apply_filtered_grid_now(&ui, &ru);
        });
    }
}
