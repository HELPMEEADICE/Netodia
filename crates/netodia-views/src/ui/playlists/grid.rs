//! Playlists grid: DB fetch + filter / sort / chunk / prewarm logic. Mirrors
//! `albums::grid`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use slint::{ComponentHandle, Model, ModelRc, VecModel, Weak};

use super::state::{CachedGrid, GRID_PREWARM_AHEAD, GridData, GridIndexCache};
use super::{PlaylistsUi, to_slint_playlist_row};
use crate::ui::grid_prewarm;
use crate::ui::grid_rows::chunk_rows;
use crate::ui::row_match;
use crate::ui::util::len_as_i32;
use netodia_app::state::AppState;
use netodia_core::entities::cloud::{MusicLibraryCategory, Playlist as CloudPlaylist};
use netodia_core::entities::playlist::PlaylistStats;
use netodia_core::error::{AppResult, describe};
use netodia_ui::{
    AppWindow, PlaylistGridRow as UiPlaylistGridRow, PlaylistRow as UiPlaylistRow, Playlists,
};

// Bound recommendation staleness while ordinary tab switches stay entirely in memory.
pub(super) const CATEGORY_CACHE_TTL: Duration = Duration::from_mins(5);

/// Paint the category's last successful snapshot before starting any background refresh.
/// Returns whether it is fresh enough to skip the request. UI thread only.
pub(super) fn restore_cached_grid(
    ui: &AppWindow,
    playlists_ui: &PlaylistsUi,
    account_generation: u64,
) -> bool {
    let fresh = playlists_ui.restore_category(account_generation);
    rebuild_grid(ui, playlists_ui);
    update_flat_rows(ui, playlists_ui);
    let count = ui.global::<Playlists>().get_total_count();
    set_cloud_state(ui, false, None, count);
    fresh
}

/// Fetch the playlist list from the DB, prewarm cover thumbnails, then
/// rebuild the grid model on the UI thread. The flat `Playlists.rows`
/// model (used by the Add-to-Playlist submenu) is repopulated in the same
/// UI hop so the submenu stays in sync with the grid.
pub async fn fetch_grid(
    state: &AppState,
    playlists_ui: &Arc<PlaylistsUi>,
    weak: Weak<AppWindow>,
) -> AppResult<()> {
    fetch_grid_inner(state, playlists_ui, weak).await
}

/// `stats_changed`-driven grid refresh: recount only the stat-dependent smart
/// playlists (others keep their previous counts, which a play stat change can't
/// move). Gated upstream by `has_stat_dependent_smart_playlists`.
pub async fn fetch_grid_stats(
    state: &AppState,
    playlists_ui: &Arc<PlaylistsUi>,
    weak: Weak<AppWindow>,
) -> AppResult<()> {
    fetch_grid_inner(state, playlists_ui, weak).await
}

async fn fetch_grid_inner(
    state: &AppState,
    playlists_ui: &Arc<PlaylistsUi>,
    weak: Weak<AppWindow>,
) -> AppResult<()> {
    let account_generation = state.cloud.status().generation;
    let (request, category_index) = {
        let _gate = playlists_ui.section.gate();
        (
            playlists_ui.grid_request.fetch_add(1, Ordering::AcqRel).wrapping_add(1),
            playlists_ui.category.load(Ordering::Acquire),
        )
    };
    let category = MusicLibraryCategory::from_index(category_index);
    let playlists_ui_for_clear = playlists_ui.clone();
    let _ = weak.upgrade_in_event_loop(move |ui| {
        if !playlists_ui_for_clear.grid_request_current(request) {
            return;
        }
        let count = ui.global::<Playlists>().get_total_count();
        set_cloud_state(&ui, true, None, count);
    });

    let playlists = match fetch_saved_playlists(state, category).await {
        Ok(playlists) => playlists,
        Err(error) => {
            if state.cloud.status().generation != account_generation
                || !playlists_ui.grid_request_current(request)
            {
                playlists_ui.mark_dirty();
                return Ok(());
            }
            let playlists_ui = playlists_ui.clone();
            let message = describe(&error);
            let cloud = state.cloud.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if !playlists_ui.grid_request_current(request)
                    || cloud.status().generation != account_generation
                {
                    return;
                }
                rebuild_grid(&ui, &playlists_ui);
                update_flat_rows(&ui, &playlists_ui);
                let count = ui.global::<Playlists>().get_total_count();
                set_cloud_state(&ui, false, Some(message), count);
            });
            return Err(error);
        }
    };
    if state.cloud.status().generation != account_generation
        || !playlists_ui.grid_request_current(request)
    {
        playlists_ui.mark_dirty();
        return Ok(());
    }

    let data = Arc::new(GridData::new(playlists));
    {
        let _gate = playlists_ui.section.gate();
        if !playlists_ui.grid_request_current(request)
            || state.cloud.status().generation != account_generation
        {
            return Ok(());
        }
        playlists_ui.grid.categories.lock().insert(
            category_index,
            CachedGrid { account_generation, fetched_at: Instant::now(), data: data.clone() },
        );
        *playlists_ui.grid.data.lock() = data;
        *playlists_ui.grid.index_cache.lock() = None;
    }

    grid_prewarm::prewarm_off_thread(playlists_ui, PlaylistsUi::prewarm_visible_covers).await;
    if state.cloud.status().generation != account_generation
        || !playlists_ui.grid_request_current(request)
    {
        playlists_ui.mark_dirty();
        return Ok(());
    }

    let playlists_ui = playlists_ui.clone();
    let cloud = state.cloud.clone();
    let _ = weak.upgrade_in_event_loop(move |ui| {
        if !playlists_ui.grid_request_current(request)
            || cloud.status().generation != account_generation
        {
            return;
        }
        rebuild_grid(&ui, &playlists_ui);
        update_flat_rows(&ui, &playlists_ui);
        let count = ui.global::<Playlists>().get_total_count();
        set_cloud_state(&ui, false, None, count);
    });
    Ok(())
}

async fn fetch_saved_playlists(
    state: &AppState,
    category: MusicLibraryCategory,
) -> AppResult<Vec<PlaylistStats>> {
    const PAGE_SIZE: u64 = 30;
    const MAX_OFFSET: u64 = 100_000;
    let mut offset = 0_u64;
    let mut playlists = Vec::new();
    loop {
        let (page, more) = state.cloud.library_playlists_page(category, offset).await?;
        playlists.extend(page);
        if !more || offset >= MAX_OFFSET {
            break;
        }
        offset = offset.saturating_add(PAGE_SIZE);
    }

    let covers = cache_playlist_covers(state, &playlists).await;
    Ok(playlists
        .into_iter()
        .map(|playlist| {
            let cover = covers.get(&playlist.id).cloned();
            let duration = state.cloud.cached_playlist_duration(playlist.id);
            cloud_playlist_stats(playlist, cover, duration)
        })
        .collect())
}

fn cloud_playlist_stats(
    playlist: CloudPlaylist,
    thumbnail_path: Option<String>,
    duration_ms: Option<u64>,
) -> PlaylistStats {
    PlaylistStats {
        id: playlist.id,
        name: playlist.name,
        description: playlist.description,
        thumbnail_path,
        is_smart: false,
        smart_criteria: None,
        created_at: String::new(),
        updated_at: String::new(),
        custom_thumbnail: false,
        track_count: i32::try_from(playlist.track_count).unwrap_or(i32::MAX),
        total_duration_ms: duration_ms
            .map_or(0, |duration| i64::try_from(duration).unwrap_or(i64::MAX)),
    }
}

async fn cache_playlist_covers(
    state: &AppState,
    playlists: &[CloudPlaylist],
) -> HashMap<i64, String> {
    const COVER_LIMIT: usize = 60;
    // Include the decode/store step in the bound: queued compressed bodies otherwise accumulate
    // while the blocking pool waits for its oversized-image gate.
    const COVER_CONCURRENCY: usize = 4;
    let mut urls = Vec::new();
    let mut seen = HashSet::new();
    for playlist in playlists {
        if let Some(url) = playlist.cover_url.as_deref()
            && seen.insert(url.to_owned())
        {
            urls.push(url.to_owned());
            if urls.len() == COVER_LIMIT {
                break;
            }
        }
    }
    let client = state.http_client().clone();
    let dir = state.paths.data_dir.join("netease-artwork");
    let cached: Vec<_> = stream::iter(urls.into_iter().map(|url| {
        let client = client.clone();
        let dir = dir.clone();
        async move { (url.clone(), download_cover(&client, &url, &dir).await) }
    }))
    .buffer_unordered(COVER_CONCURRENCY)
    .collect()
    .await;
    let by_url: HashMap<String, String> =
        cached.into_iter().filter_map(|(url, path)| path.map(|path| (url, path))).collect();
    playlists
        .iter()
        .filter_map(|playlist| {
            let url = playlist.cover_url.as_ref()?;
            Some((playlist.id, by_url.get(url)?.clone()))
        })
        .collect()
}

async fn download_cover(client: &reqwest::Client, url: &str, dir: &Path) -> Option<String> {
    let bytes = crate::ui::remote_covers::fetch_image_bytes(client, url).await?;
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        netodia_artwork::media::image::artwork::store_image(&bytes, "jpg", &dir)
    })
    .await
    .ok()
    .flatten()
}

fn set_cloud_state(ui: &AppWindow, loading: bool, error: Option<String>, count: i32) {
    let g = ui.global::<Playlists>();
    g.set_cloud_loading(loading);
    g.set_cloud_error(error.map_or_else(slint::SharedString::default, slint::SharedString::from));
    g.set_total_count(count);
}

/// Rebuild the grid model from the cached grid data — no DB hit. Same
/// memoization contract as `albums::grid::rebuild_grid`.
pub fn rebuild_grid(ui: &AppWindow, playlists_ui: &PlaylistsUi) {
    let g = ui.global::<Playlists>();
    let sort_field = g.get_sort_field().to_string();
    let sort_dir = g.get_sort_dir().to_string();
    let filter = g.get_filter().to_string();
    let columns = g.get_columns().max(1);

    let data = playlists_ui.grid.data.lock().clone();

    let rows = {
        let mut cache = playlists_ui.grid.index_cache.lock();
        let stale =
            !matches!(cache.as_ref(), Some(c) if c.matches(&filter, &sort_field, &sort_dir));
        if stale {
            let indices = compute_indices(&data, &sort_field, &sort_dir, &filter);
            *cache = Some(GridIndexCache { filter, sort_field, sort_dir, indices });
        }
        let indices = cache.as_ref().map_or(&[][..], |c| c.indices.as_slice());
        chunk_indices(&data, indices, columns)
    };
    let total = len_as_i32(data.playlists.len());

    g.set_total_count(total);
    let model = g.get_grid_rows();
    if let Some(vm) = model.as_any().downcast_ref::<VecModel<UiPlaylistGridRow>>() {
        vm.set_vec(rows);
    } else {
        g.set_grid_rows(ModelRc::new(VecModel::from(rows)));
    }
}

/// Rebuild the flat `Playlists.rows` model (used by the per-track
/// Add-to-Playlist submenu). Always in `updated_at DESC` order so the
/// submenu lists most-recently-touched playlists first, regardless of
/// the grid's current sort selection.
pub fn update_flat_rows(ui: &AppWindow, playlists_ui: &PlaylistsUi) {
    let g = ui.global::<Playlists>();
    let data = playlists_ui.grid.data.lock().clone();
    let rows: Vec<UiPlaylistRow> = data.playlists.iter().map(to_slint_playlist_row).collect();
    let model = g.get_rows();
    if let Some(vm) = model.as_any().downcast_ref::<VecModel<UiPlaylistRow>>() {
        vm.set_vec(rows);
    } else {
        g.set_rows(ModelRc::new(VecModel::from(rows)));
    }
}

pub(super) fn compute_indices(
    data: &GridData,
    sort_field: &str,
    sort_dir: &str,
    filter: &str,
) -> Vec<usize> {
    let needle = row_match::fold_needle(filter);
    let mut indices: Vec<usize> = if needle.is_empty() {
        (0..data.playlists.len()).collect()
    } else {
        data.playlists
            .iter()
            .enumerate()
            .filter(|(_, p)| needle.contains(&p.name))
            .map(|(i, _)| i)
            .collect()
    };
    sort_playlist_indices(&mut indices, data, sort_field, sort_dir);
    indices
}

fn chunk_indices(data: &GridData, indices: &[usize], columns: i32) -> Vec<UiPlaylistGridRow> {
    chunk_rows(
        indices,
        columns,
        |&i| to_slint_playlist_row(&data.playlists[i]),
        |playlists| UiPlaylistGridRow { playlists },
    )
}

/// Sort `indices` into the grid data by the chosen field. `playlist_stats`
/// is fetched in `updated_at DESC` order, so any other sort must be done
/// in memory.
///
/// Supported fields:
/// * `"name"` — case-insensitive, with name as the deterministic tiebreaker.
/// * `"track_count"` — primary key; name (asc) as a stable tiebreaker.
/// * `"updated"` (default) — `updated_at` RFC3339 string (lex = chrono).
fn sort_playlist_indices(indices: &mut [usize], data: &GridData, field: &str, dir: &str) {
    match field {
        "track_count" => indices
            .sort_by_cached_key(|&i| (data.playlists[i].track_count, data.keys[i].name_lc.clone())),
        "name" => indices.sort_by_cached_key(|&i| data.keys[i].name_lc.clone()),
        // "updated" and anything unrecognised fall through to the default
        // (most-recently-updated first).
        _ => {
            // Cloud rows have no update timestamp: equal keys must retain the server's order.
            if dir == "desc" {
                indices.sort_by_cached_key(|&i| {
                    std::cmp::Reverse(data.playlists[i].updated_at.clone())
                });
            } else {
                indices.sort_by_cached_key(|&i| data.playlists[i].updated_at.clone());
            }
            return;
        }
    }
    if dir == "desc" {
        indices.reverse();
    }
}

/// The first `GRID_PREWARM_AHEAD` distinct thumbnail paths in display order —
/// the covers first on screen, which `PlaylistsUi::prewarm_visible_covers`
/// warms. The cap counts kept *paths*, so a run of thumbnail-less playlists is
/// walked past rather than spending the budget on them.
pub(super) fn first_screenful_paths(data: &GridData) -> Vec<PathBuf> {
    grid_prewarm::unique_artwork_paths(
        data.playlists.iter().map(|p| p.thumbnail_path.as_deref()),
        GRID_PREWARM_AHEAD,
    )
}
