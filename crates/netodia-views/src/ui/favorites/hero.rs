//! Hero stats + the banner artwork refresh.
//!
//! The Favorites view derives its count and artwork from fetched `NetEase` songs and composes their
//! first four distinct covers into one collage through [`crate::ui::mosaic_hero`].

use std::sync::Arc;

use slint::{ComponentHandle, Weak};

use super::FavoritesUi;
use crate::ui::detail_artwork::DetailPair;
use netodia_app::state::AppState;
use netodia_core::entities::track::FavoriteStats;
use netodia_core::entities::track::TrackListRow;
use netodia_ui::{AppWindow, Favorites};

/// Restore the banner on the same UI tick as its cached list.
pub(super) fn restore_now(ui: &AppWindow, fav_ui: &FavoritesUi) {
    if !fav_ui.section_active() {
        return;
    }
    let cached = fav_ui.state().hero_cache.lock().get(&fav_ui.active_tab()).cloned();
    if let Some((paths, pair)) = cached {
        let _ = fav_ui.state().last_mosaic_paths.claim(paths);
        crate::ui::detail_view::apply_detail_artwork(
            ui,
            &ui.global::<Favorites>(),
            pair,
            false,
            true,
        );
    } else {
        fav_ui.forget_mosaic();
        crate::ui::detail_view::release_hero_slots(&ui.global::<Favorites>());
        crate::ui::hero_backdrop::reset(ui);
    }
    crate::ui::hero_chips::publish_favorites(ui, fav_ui);
}

/// Keep the composed buffers with the rows, even when the Slint slots are released.
pub(super) fn publish_artwork_now(
    ui: &AppWindow,
    fav_ui: &FavoritesUi,
    paths: Vec<String>,
    pair: DetailPair,
    animate: bool,
) {
    fav_ui.state().hero_cache.lock().insert(fav_ui.active_tab(), (paths.clone(), pair.clone()));
    if fav_ui.state().last_mosaic_paths.claim(paths) {
        crate::ui::detail_view::apply_detail_artwork(
            ui,
            &ui.global::<Favorites>(),
            pair,
            animate,
            true,
        );
    }
}

pub(super) fn republish_chips(fav_ui: &Arc<FavoritesUi>, weak: &Weak<AppWindow>) {
    let fav_ui = fav_ui.clone();
    let _ = weak.upgrade_in_event_loop(move |ui| {
        crate::ui::hero_chips::publish_favorites(&ui, &fav_ui);
    });
}

pub(super) fn stats_from_tracks(rows: &[TrackListRow]) -> FavoriteStats {
    let mut artwork_paths = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut total_duration_ms = 0_i64;
    for row in rows {
        total_duration_ms = total_duration_ms.saturating_add(row.duration_ms.max(0));
        if artwork_paths.len() < 4
            && let Some(path) = row.artwork_path.as_deref()
            && !path.is_empty()
            && seen.insert(path.to_owned())
        {
            artwork_paths.push(path.to_owned());
        }
    }
    FavoriteStats {
        count: i64::try_from(rows.len()).unwrap_or(i64::MAX),
        total_duration_ms,
        artwork_paths,
    }
}

/// Derive the banner's count, running time and artwork from the `NetEase` rows already fetched for
/// the Songs tab. `generation` prevents a slow collage from an old account being published after
/// an account transition.
pub async fn refresh_from_tracks(
    state: &AppState,
    fav_ui: &Arc<FavoritesUi>,
    weak: &Weak<AppWindow>,
    rows: &[TrackListRow],
    generation: u64,
    liked_revision: u64,
    animate: bool,
) {
    let stats = stats_from_tracks(rows);

    // A hidden page retains its snapshot but must not repaint the shared banner.
    if !fav_ui.section_active()
        || state.cloud.status().generation != generation
        || state.cloud.liked_ids_revision() != liked_revision
    {
        return;
    }

    {
        // Account invalidation clears this under the same gate.
        let _gate = fav_ui.gate();
        if state.cloud.status().generation != generation
            || state.cloud.liked_ids_revision() != liked_revision
            || !fav_ui.section_active()
        {
            return;
        }
        *fav_ui.state().stats.lock() = stats.clone();
    }
    push_stats_to_slint(fav_ui, weak);

    let paths = stats.artwork_paths.clone();

    // Skip the compose when those covers are already the ones on screen — a library or
    // stats tick usually returns the same top four, and an unchanged banner is still
    // correct. The matching claim is in `publish_artwork_now`, for `MosaicGuard`'s
    // reason.
    if fav_ui.active_tab() != super::FavoritesTab::Songs
        || !fav_ui.state().last_mosaic_paths.is_stale(&paths)
    {
        return;
    }

    let Some(pair) =
        crate::ui::mosaic_hero::compose_off_thread(state, paths.clone(), fav_ui.hero_blur).await
    else {
        return;
    };
    if state.cloud.status().generation != generation
        || state.cloud.liked_ids_revision() != liked_revision
        || !fav_ui.section_active()
        || fav_ui.active_tab() != super::FavoritesTab::Songs
    {
        return;
    }
    let state = state.clone();
    let fav_ui = fav_ui.clone();
    let _ = weak.upgrade_in_event_loop(move |ui| {
        if state.cloud.status().generation == generation
            && state.cloud.liked_ids_revision() == liked_revision
            && fav_ui.section_active()
            && fav_ui.active_tab() == super::FavoritesTab::Songs
        {
            publish_artwork_now(&ui, &fav_ui, paths, pair, animate);
        }
    });
}

fn push_stats_to_slint(fav_ui: &Arc<FavoritesUi>, weak: &Weak<AppWindow>) {
    let fav_ui = fav_ui.clone();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        // The leave can land while this post is in flight, and it rewinds the count on its
        // way out — the same guard, in the same place, as `songs::apply_filtered_tracks`.
        if !fav_ui.section_active() {
            return;
        }
        let count = i32::try_from(fav_ui.state().stats.lock().count).unwrap_or(i32::MAX);
        ui.global::<Favorites>().set_track_count(count);
        // Order-free: the chips take their facts off the handle's own state,
        // not back off the properties written around them.
        crate::ui::hero_chips::publish_favorites(&ui, &fav_ui);
    });
}
