//! Account-scoped album collection, exact-ID membership actions and the virtualized projection.

use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_compat::Compat;
use parking_lot::Mutex;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use super::{FavoritesTab, FavoritesUi};
use crate::ui::row_match::Needle;
use netodia_app::state::AppState;
use netodia_core::entities::cloud::Album;
use netodia_core::error::describe;
use netodia_ui::{AppWindow, CloudAlbumGridRow, CloudAlbumRow, CloudEntityDetail, Favorites};

#[derive(Default)]
pub(crate) struct AlbumsState {
    cache: Mutex<Option<(u64, Vec<Album>)>>,
    // A membership write or account transition invalidates every older list response.
    revision: AtomicU64,
    pending: Mutex<HashSet<i64>>,
}

impl AlbumsState {
    fn clear(&self) {
        self.revision.fetch_add(1, Ordering::Relaxed);
        self.cache.lock().take();
        self.pending.lock().clear();
    }
}

pub(super) fn wire(ui: &AppWindow, state: &AppState, fav_ui: &Arc<FavoritesUi>) {
    wire_covers(ui, state);
    wire_load(ui, state, fav_ui);
    wire_membership(ui, state, fav_ui);
    wire_account_updates(ui, state, fav_ui);
}

fn wire_covers(ui: &AppWindow, state: &AppState) {
    let weak = ui.as_weak();
    let covers = Arc::new(crate::ui::remote_covers::RemoteCoverThumbs::new_for_cards(
        state.http_client().clone(),
        state.runtime.clone(),
        move || {
            let _ = weak.upgrade_in_event_loop(|ui| {
                let g = ui.global::<Favorites>();
                g.set_album_covers_generation(g.get_album_covers_generation().wrapping_add(1));
            });
        },
    ));
    ui.global::<Favorites>()
        .on_request_album_cover(move |source, _| covers.get_or_schedule(source.as_str()));
}

fn wire_load(ui: &AppWindow, state: &AppState, fav_ui: &Arc<FavoritesUi>) {
    let g = ui.global::<Favorites>();
    let weak = ui.as_weak();
    let s = state.clone();
    let fu = fav_ui.clone();
    let w = weak.clone();
    g.on_retry_albums(move || spawn_refresh(&s, &fu, &w));

    let s = state.clone();
    let fu = fav_ui.clone();
    let w = weak.clone();
    g.on_load_album_favorite(move |remote_id| {
        let Some(id) = crate::ui::util::parse_cloud_id(remote_id.as_str()) else { return };
        let Some(ui) = w.upgrade() else { return };
        let generation = s.cloud.status().generation;
        let cache = fu.state().albums.cache.lock();
        if let Some((cached_generation, albums)) = cache.as_ref()
            && *cached_generation == generation
        {
            publish_membership(&ui, id, albums.iter().any(|album| album.id == id));
            if fu.state().albums.pending.lock().contains(&id) {
                ui.global::<CloudEntityDetail>().set_album_favorite_pending(true);
            }
            return;
        }
        drop(cache);
        let detail = ui.global::<CloudEntityDetail>();
        detail.set_album_favorite_pending(true);
        detail.set_album_favorite_error(SharedString::default());
        spawn_refresh(&s, &fu, &w);
    });
}

fn wire_membership(ui: &AppWindow, state: &AppState, fav_ui: &Arc<FavoritesUi>) {
    let g = ui.global::<Favorites>();
    let s = state.clone();
    let fu = fav_ui.clone();
    let w = ui.as_weak();
    g.on_set_album_favorite(move |remote_id, saved| {
        let Some(id) = crate::ui::util::parse_cloud_id(remote_id.as_str()) else { return };
        let Some(ui) = w.upgrade() else { return };
        if !fu.state().albums.pending.lock().insert(id) {
            return;
        }
        fu.state().albums.revision.fetch_add(1, Ordering::Relaxed);
        let detail = ui.global::<CloudEntityDetail>();
        if matching_detail(&detail, id) {
            detail.set_album_favorite_pending(true);
            detail.set_album_favorite_error(SharedString::default());
        }
        ui.global::<Favorites>().set_albums_error(SharedString::default());
        apply_now(&ui, &fu);
        let s = s.clone();
        let fu = fu.clone();
        let w = w.clone();
        let generation = s.cloud.status().generation;
        s.runtime.clone().spawn(async move {
            let result = s.cloud.subscribe_album(id, saved).await;
            let w_refresh = w.clone();
            let _ = w.upgrade_in_event_loop(move |ui| {
                if s.cloud.status().generation != generation {
                    return;
                }
                fu.state().albums.pending.lock().remove(&id);
                fu.state().albums.revision.fetch_add(1, Ordering::Relaxed);
                let detail = ui.global::<CloudEntityDetail>();
                if matching_detail(&detail, id) {
                    detail.set_album_favorite_pending(false);
                }
                match result {
                    Ok(()) => {
                        {
                            let mut cache = fu.state().albums.cache.lock();
                            if saved {
                                // The next lookup must fetch the newly saved album's metadata.
                                cache.take();
                            } else if let Some((_, albums)) = cache.as_mut() {
                                albums.retain(|album| album.id != id);
                            }
                        }
                        publish_membership(&ui, id, saved);
                        apply_now(&ui, &fu);
                        spawn_refresh(&s, &fu, &w_refresh);
                    }
                    Err(error) => {
                        let message = describe(&error);
                        ui.global::<Favorites>().set_albums_loading(false);
                        ui.global::<Favorites>().set_albums_error(message.as_str().into());
                        if matching_detail(&detail, id) {
                            detail.set_album_favorite_error(message.into());
                        } else if detail.get_open()
                            && detail.get_kind() == "album"
                            && !detail.get_album_favorite_known()
                        {
                            // A different album may have been opened while this write ran.
                            spawn_refresh(&s, &fu, &w_refresh);
                        }
                        apply_now(&ui, &fu);
                    }
                }
            });
        });
    });
}

fn wire_account_updates(ui: &AppWindow, state: &AppState, fav_ui: &Arc<FavoritesUi>) {
    // Account changes also reach details opened outside the Favorites section.
    let s = state.clone();
    let fu = fav_ui.clone();
    let w = ui.as_weak();
    let mut account = state.cloud.subscribe();
    let mut generation = state.cloud.status().generation;
    let _ = slint::spawn_local(Compat::new(async move {
        while account.changed().await.is_ok() {
            let current = s.cloud.status().generation;
            if current == generation {
                continue;
            }
            generation = current;
            let Some(ui) = w.upgrade() else { break };
            fu.state().albums.clear();
            let g = ui.global::<Favorites>();
            g.set_album_rows(ModelRc::default());
            g.set_album_count(-1);
            g.set_filtered_album_count(0);
            g.set_albums_loading(false);
            g.set_albums_error(SharedString::default());
            let detail = ui.global::<CloudEntityDetail>();
            detail.set_album_favorite(false);
            detail.set_album_favorite_known(false);
            detail.set_album_favorite_pending(false);
            detail.set_album_favorite_error(SharedString::default());
            if (fu.section_active() && fu.active_tab() == FavoritesTab::Albums)
                || (detail.get_open() && detail.get_kind() == "album")
            {
                spawn_refresh(&s, &fu, &w);
            }
        }
    }));
}

pub(super) fn spawn_refresh(state: &AppState, fav_ui: &Arc<FavoritesUi>, weak: &Weak<AppWindow>) {
    let state = state.clone();
    let fav_ui = fav_ui.clone();
    let weak = weak.clone();
    state.runtime.clone().spawn(async move { refresh(&state, &fav_ui, &weak).await });
}

pub(super) fn paint_on_enter(weak: &Weak<AppWindow>, fav_ui: &FavoritesUi) {
    if fav_ui.active_tab() == FavoritesTab::Albums
        && let Some(ui) = weak.upgrade()
    {
        apply_now(&ui, fav_ui);
        super::hero::restore_now(&ui, fav_ui);
        ui.global::<Favorites>().set_albums_loading(true);
    }
}

pub(super) async fn refresh(state: &AppState, fav_ui: &Arc<FavoritesUi>, weak: &Weak<AppWindow>) {
    if !fav_ui.state().albums.pending.lock().is_empty() {
        return;
    }
    let generation = state.cloud.status().generation;
    let request = fav_ui.state().albums.revision.fetch_add(1, Ordering::Relaxed) + 1;
    let fu = fav_ui.clone();
    let cloud = state.cloud.clone();
    let _ = weak.upgrade_in_event_loop(move |ui| {
        if cloud.status().generation == generation
            && fu.state().albums.revision.load(Ordering::Relaxed) == request
            && fu.section_active()
            && fu.active_tab() == FavoritesTab::Albums
        {
            ui.global::<Favorites>().set_albums_loading(true);
            ui.global::<Favorites>().set_albums_error(SharedString::default());
        }
    });
    let result = state.cloud.all_saved_albums().await;
    let state = state.clone();
    let fav_ui = fav_ui.clone();
    let artwork_weak = weak.clone();
    let _ = weak.upgrade_in_event_loop(move |ui| {
        if state.cloud.status().generation != generation
            || fav_ui.state().albums.revision.load(Ordering::Relaxed) != request
        {
            return;
        }
        let g = ui.global::<Favorites>();
        g.set_albums_loading(false);
        let detail = ui.global::<CloudEntityDetail>();
        match result {
            Ok(albums) => {
                let paths = artwork_paths(&albums);
                if detail.get_open()
                    && detail.get_kind() == "album"
                    && let Some(id) =
                        crate::ui::util::parse_cloud_id(detail.get_remote_id().as_str())
                {
                    publish_membership(&ui, id, albums.iter().any(|album| album.id == id));
                }
                *fav_ui.state().albums.cache.lock() = Some((generation, albums));
                g.set_albums_error(SharedString::default());
                apply_now(&ui, &fav_ui);
                spawn_artwork(&state, &fav_ui, &artwork_weak, paths, generation, request);
            }
            Err(error) => {
                let message = describe(&error);
                g.set_albums_error(message.as_str().into());
                if detail.get_open() && detail.get_kind() == "album" {
                    detail.set_album_favorite_pending(false);
                    detail.set_album_favorite_error(message.into());
                }
            }
        }
    });
}

fn artwork_paths(albums: &[Album]) -> Vec<String> {
    let mut seen = HashSet::new();
    albums
        .iter()
        .filter_map(|album| album.cover_url.as_ref())
        .filter(|url| crate::ui::remote_covers::is_remote_artwork_url(url))
        .filter(|url| seen.insert(url.as_str()))
        .take(4)
        .cloned()
        .collect()
}

fn spawn_artwork(
    state: &AppState,
    fav_ui: &Arc<FavoritesUi>,
    weak: &Weak<AppWindow>,
    paths: Vec<String>,
    generation: u64,
    request: u64,
) {
    if !fav_ui.section_active()
        || fav_ui.active_tab() != FavoritesTab::Albums
        || !fav_ui.state().last_mosaic_paths.is_stale(&paths)
    {
        return;
    }
    let state = state.clone();
    let fav_ui = fav_ui.clone();
    let weak = weak.clone();
    state.runtime.clone().spawn(async move {
        let Some(pair) =
            crate::ui::mosaic_hero::compose_off_thread(&state, paths.clone(), fav_ui.hero_blur)
                .await
        else {
            return;
        };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if state.cloud.status().generation != generation
                || fav_ui.state().albums.revision.load(Ordering::Relaxed) != request
                || !fav_ui.section_active()
                || fav_ui.active_tab() != FavoritesTab::Albums
            {
                return;
            }
            super::hero::publish_artwork_now(&ui, &fav_ui, paths, pair, true);
        });
    });
}

fn matching_detail(detail: &CloudEntityDetail<'_>, id: i64) -> bool {
    detail.get_open()
        && detail.get_kind() == "album"
        && crate::ui::util::parse_cloud_id(detail.get_remote_id().as_str()) == Some(id)
}

fn publish_membership(ui: &AppWindow, id: i64, saved: bool) {
    let detail = ui.global::<CloudEntityDetail>();
    if matching_detail(&detail, id) {
        detail.set_album_favorite(saved);
        detail.set_album_favorite_known(true);
        detail.set_album_favorite_pending(false);
        detail.set_album_favorite_error(SharedString::default());
    }
}

fn album_matches(album: &Album, needle: &Needle) -> bool {
    needle.contains(&album.name) || album.artists.iter().any(|artist| needle.contains(&artist.name))
}

fn album_row(album: &Album, pending: bool) -> CloudAlbumRow {
    CloudAlbumRow {
        remote_id: album.id.to_string().into(),
        title: album.name.as_str().into(),
        artist: album
            .artists
            .iter()
            .map(|artist| artist.name.as_str())
            .collect::<Vec<_>>()
            .join(" / ")
            .into(),
        artwork_path: album
            .cover_url
            .as_deref()
            .filter(|url| crate::ui::remote_covers::is_remote_artwork_url(url))
            .unwrap_or_default()
            .into(),
        pending,
    }
}

pub(super) fn apply_now(ui: &AppWindow, fav_ui: &FavoritesUi) {
    if !fav_ui.section_active() || fav_ui.active_tab() != FavoritesTab::Albums {
        return;
    }
    let g = ui.global::<Favorites>();
    let cache = fav_ui.state().albums.cache.lock();
    let Some((_, albums)) = cache.as_ref() else { return };
    let filter = fav_ui.state().filter.lock().clone();
    let pending = fav_ui.state().albums.pending.lock();
    let rows = albums
        .iter()
        .filter(|album| album_matches(album, &filter))
        .map(|album| album_row(album, pending.contains(&album.id)))
        .collect::<Vec<_>>();
    g.set_album_count(crate::ui::util::len_as_i32(albums.len()));
    g.set_filtered_album_count(crate::ui::util::len_as_i32(rows.len()));
    let columns = usize::try_from(g.get_columns().max(1)).unwrap_or(1);
    let grid = rows
        .chunks(columns)
        .map(|row| CloudAlbumGridRow {
            albums: ModelRc::from(Rc::new(VecModel::from(row.to_vec()))),
        })
        .collect::<Vec<_>>();
    g.set_album_rows(ModelRc::from(Rc::new(VecModel::from(grid))));
    crate::ui::hero_chips::publish_favorites(ui, fav_ui);
}

#[cfg(test)]
#[path = "tests/albums_tests.rs"]
mod tests;
