//! The Playlists grid's filter + sort pass — the last of the four entity
//! grids to get one. Same three cases its siblings pin, since all four run
//! the identical `row_match::Needle::contains` walk over a single name.
//!
//! Plus the four things drag-to-reorder needs to stay alive, none of which any
//! other pin can see go missing.

use super::detail::{POSITION_FIELD, is_manual_order, sort_playlist_tracks};
use super::*;
use netodia_core::entities::track::TrackListRow;
use netodia_testkit::code_tokens;

const DRAGGABLE_LIST: &str =
    include_str!("../../../../../netodia-ui/ui/components/track-list/draggable-track-list.slint");
const QUEUE_SHEET: &str = include_str!("../../../../../netodia-ui/ui/views/queue-sheet.slint");
const DETAIL_VIEW: &str =
    include_str!("../../../../../netodia-ui/ui/views/my-library/playlist-detail.slint");
const SHARED_DETAIL_WIRING: &str = include_str!("../../callbacks/track_detail.rs");
const DETAIL: &str = include_str!("../detail.rs");
const CLOUD_TRACKS: &str =
    include_str!("../../../../../netodia-ui/ui/views/my-library/cloud-playlist-tracks.slint");
const PLAYLISTS_TAB: &str =
    include_str!("../../../../../netodia-ui/ui/views/my-library/playlists-tab.slint");
const PLAYLIST_GRID: &str =
    include_str!("../../../../../netodia-ui/ui/components/grid/playlist-grid.slint");
const PLAYLIST_DETAIL_GLOBAL: &str =
    include_str!("../../../../../netodia-ui/ui/globals/playlists.slint");
const GRID_CALLBACKS: &str = include_str!("../callbacks/grid.rs");

/// Minimal `PlaylistStats` builder — only the fields the grid filter / sort
/// read matter. Regular playlist, so `smart_criteria` never parses.
fn playlist(id: i64, name: &str, track_count: i32, updated_at: &str) -> PlaylistStats {
    PlaylistStats {
        id,
        name: name.to_owned(),
        description: None,
        thumbnail_path: None,
        is_smart: false,
        smart_criteria: None,
        created_at: "2026-01-01T00:00:00Z".to_owned(),
        updated_at: updated_at.to_owned(),
        custom_thumbnail: false,
        track_count,
        total_duration_ms: 0,
    }
}

fn names(data: &GridData, indices: &[usize]) -> Vec<String> {
    indices.iter().map(|&i| data.playlists[i].name.clone()).collect()
}

fn cached_categories() -> PlaylistsUi {
    let ui = PlaylistsUi::new(Arc::new(CoverThumbs::new()), None);
    ui.grid.categories.lock().insert(
        0,
        state::CachedGrid {
            account_generation: 7,
            fetched_at: std::time::Instant::now(),
            data: Arc::new(GridData::new(vec![playlist(1, "Collected", 2, "")])),
        },
    );
    ui.grid.categories.lock().insert(
        1,
        state::CachedGrid {
            account_generation: 7,
            fetched_at: std::time::Instant::now(),
            data: Arc::new(GridData::new(vec![playlist(2, "Created", 3, "")])),
        },
    );
    ui
}

#[test]
fn switching_categories_restores_each_snapshot_without_releasing_it_on_leave() {
    let ui = cached_categories();
    ui.category.store(1, Ordering::Release);
    assert!(ui.restore_category(7));
    assert_eq!(ui.grid_stats_by_id(2).map(|row| row.name), Some("Created".to_owned()));

    ui.release_section_state();
    ui.category.store(0, Ordering::Release);
    assert!(ui.restore_category(7));
    assert_eq!(ui.grid_stats_by_id(1).map(|row| row.name), Some("Collected".to_owned()));
}

#[test]
fn expired_categories_still_paint_their_snapshot_but_require_refresh()
-> Result<(), Box<dyn std::error::Error>> {
    let ui = cached_categories();
    let expired = std::time::Instant::now()
        .checked_sub(grid::CATEGORY_CACHE_TTL)
        .ok_or("monotonic clock cannot represent the expired cache timestamp")?;
    if let Some(cached) = ui.grid.categories.lock().get_mut(&0) {
        cached.fetched_at = expired;
    }

    assert!(!ui.restore_category(7));
    assert_eq!(ui.grid_stats_by_id(1).map(|row| row.track_count), Some(2));
    Ok(())
}

#[test]
fn another_account_cannot_restore_the_previous_accounts_categories() {
    let ui = cached_categories();
    assert!(!ui.restore_category(8));
    assert!(ui.grid.data.lock().playlists.is_empty());

    ui.clear_account_data();
    assert!(ui.grid.categories.lock().is_empty());
    assert!(!ui.restore_category(7));
}

fn track(id: i64, title: &str, album: &str, favorite: bool, length: i64) -> TrackListRow {
    TrackListRow {
        id,
        file_path: String::new(),
        file_name: title.to_owned(),
        title: title.to_owned(),
        artist: Some("Artist".to_owned()),
        album_artist: None,
        album: Some(album.to_owned()),
        genre: None,
        track_number: None,
        disc_number: None,
        year: None,
        duration_ms: length,
        artwork_path: None,
        is_favorite: favorite,
        rating: 0,
        album_id: None,
        artist_id: None,
        genre_id: None,
        date_added: String::new(),
        sort_key: None,
    }
}

#[test]
fn measured_detail_stats_reach_the_card_without_changing_its_metadata_or_old_snapshot() {
    let original = Arc::new(GridData::new(vec![
        playlist(4_000_000_000, "Saved card title", 48, "2026-10-27"),
        playlist(4_000_000_001, "Another playlist", 26, "2026-10-26"),
    ]));
    let mut current = Arc::clone(&original);
    let mut detail = playlist(4_000_000_000, "Detail title", 47, "");
    detail.total_duration_ms = 8_491_000;

    assert!(Arc::make_mut(&mut current).update_track_stats(&detail));
    assert_eq!(original.playlists[0].total_duration_ms, 0);
    assert_eq!(current.playlists[0].total_duration_ms, 8_491_000);
    assert_eq!(current.playlists[0].track_count, 47);
    assert_eq!(current.playlists[0].name, "Saved card title");
    assert_eq!(current.keys[0].name_lc, "saved card title");
    assert_eq!(current.playlists[1].track_count, 26);
    assert_eq!(current.playlists[1].total_duration_ms, 0);
}

#[test]
fn a_detail_from_another_grid_does_not_insert_a_card_or_change_its_stats() {
    let mut data = GridData::new(vec![playlist(1, "Current category", 48, "")]);
    let mut other_detail = playlist(2, "Other category", 149, "");
    other_detail.total_duration_ms = 1_234_000;

    assert!(!data.update_track_stats(&other_detail));
    assert_eq!(data.playlists.len(), 1);
    assert_eq!(data.playlists[0].id, 1);
    assert_eq!(data.playlists[0].track_count, 48);
    assert_eq!(data.playlists[0].total_duration_ms, 0);
}

#[test]
fn compute_indices_with_empty_filter_keeps_all_playlists() {
    let data = GridData::new(vec![
        playlist(1, "Alpha", 1, "2026-01-02T00:00:00Z"),
        playlist(2, "Bravo", 1, "2026-01-01T00:00:00Z"),
    ]);
    let idx = compute_indices(&data, "name", "asc", "");
    assert_eq!(names(&data, &idx), ["Alpha", "Bravo"]);
}

#[test]
fn compute_indices_filter_matches_name_case_insensitively() {
    let data = GridData::new(vec![
        playlist(1, "Late Night Drive", 1, "2026-01-03T00:00:00Z"),
        playlist(2, "Night Shift", 1, "2026-01-02T00:00:00Z"),
        playlist(3, "Morning Coffee", 1, "2026-01-01T00:00:00Z"),
    ]);
    let by_name = compute_indices(&data, "name", "asc", "NIGHT");
    assert_eq!(names(&data, &by_name), ["Late Night Drive", "Night Shift"]);
}

#[test]
fn compute_indices_filter_ignores_accents_the_way_the_search_view_does() {
    // A playlist name is whatever the user typed, so it carries whatever
    // their keyboard makes easy — and the query later may not.
    let data = GridData::new(vec![
        playlist(1, "Café Sessions", 1, "2026-01-02T00:00:00Z"),
        playlist(2, "Morning Coffee", 1, "2026-01-01T00:00:00Z"),
    ]);
    let by_name = compute_indices(&data, "name", "asc", "cafe");
    assert_eq!(names(&data, &by_name), ["Café Sessions"]);
}

#[test]
fn compute_indices_default_sort_is_most_recently_updated_first() {
    // `playlist_stats` already returns `updated_at DESC`, so the default arm
    // re-derives that order rather than leaving whatever the filter produced.
    let data = GridData::new(vec![
        playlist(1, "Oldest", 1, "2026-01-01T00:00:00Z"),
        playlist(2, "Newest", 1, "2026-03-01T00:00:00Z"),
        playlist(3, "Middle", 1, "2026-02-01T00:00:00Z"),
    ]);
    let desc = compute_indices(&data, "updated", "desc", "");
    assert_eq!(names(&data, &desc), ["Newest", "Middle", "Oldest"]);
    let asc = compute_indices(&data, "updated", "asc", "");
    assert_eq!(names(&data, &asc), ["Oldest", "Middle", "Newest"]);
}

#[test]
fn cloud_playlist_rows_share_the_standard_header_and_position_is_the_default_sort() {
    assert!(PLAYLISTS_TAB.contains("open-playlist(id) => { Playlists.open-playlist(id); }"));
    assert!(PLAYLIST_GRID.contains("clicked => { root.open-playlist(playlist.remote_id); }"));
    assert!(PLAYLIST_DETAIL_GLOBAL.contains("callback open-playlist(string"));
    assert!(GRID_CALLBACKS.contains("playlist_id.to_string().parse::<i64>()"));
    assert!(PLAYLIST_DETAIL_GLOBAL.contains("in-out property <string> sort-field: \"position\";"));
    assert!(CLOUD_TRACKS.contains("TrackList {"));
    assert!(CLOUD_TRACKS.contains("columns <=> root.columns;"));
    assert!(CLOUD_TRACKS.contains("field == \"track_number\" ? root.position-field : field"));
    assert!(CLOUD_TRACKS.contains("root.play-track(id, index)"));
    assert!(DETAIL_VIEW.contains("PlaylistDetail.play-cloud-row(id, index)"));
}

#[test]
fn cloud_playlist_rows_keep_the_full_netease_id_across_the_slint_boundary() {
    let row =
        to_slint_playlist_row(&playlist(3_000_000_000, "Large ID", 0, "2026-01-01T00:00:00Z"));
    assert_eq!(row.id, i32::MAX, "legacy UI handle remains bounded");
    assert_eq!(row.remote_id.to_string(), "3000000000");
}

#[test]
fn cloud_track_rows_keep_distinct_full_width_song_ids_for_playback_and_sorting() {
    let first = crate::ui::tracks::to_slint_track_list_rows(&[track(
        2_147_483_648,
        "First",
        "Album",
        false,
        100,
    )]);
    let second = crate::ui::tracks::to_slint_track_list_rows(&[track(
        3_000_000_000,
        "Second",
        "Album",
        false,
        200,
    )]);

    assert_eq!(first[0].id, i32::MAX);
    assert_eq!(second[0].id, i32::MAX);
    assert_eq!(first[0].remote_id.to_string(), "2147483648");
    assert_eq!(second[0].remote_id.to_string(), "3000000000");
    assert_ne!(crate::ui::tracks::model_key(&first[0]), crate::ui::tracks::model_key(&second[0]));
}

#[test]
fn cloud_playlist_sort_covers_position_title_album_favorite_and_length() {
    let seed = || {
        vec![
            track(1, "Bravo", "Zulu", true, 300),
            track(2, "Alpha", "Able", false, 100),
            track(3, "Charlie", "Mango", true, 200),
        ]
    };
    let ids = |rows: &[TrackListRow]| rows.iter().map(|row| row.id).collect::<Vec<_>>();

    let mut rows = seed();
    sort_playlist_tracks(&mut rows, &[3, 1, 2], POSITION_FIELD, "asc");
    assert_eq!(ids(&rows), [3, 1, 2]);
    sort_playlist_tracks(&mut rows, &[3, 1, 2], POSITION_FIELD, "desc");
    assert_eq!(ids(&rows), [2, 1, 3]);

    let mut rows = seed();
    sort_playlist_tracks(&mut rows, &[], "title", "asc");
    assert_eq!(ids(&rows), [2, 1, 3]);
    sort_playlist_tracks(&mut rows, &[], "title", "desc");
    assert_eq!(ids(&rows), [3, 1, 2]);

    let mut rows = seed();
    sort_playlist_tracks(&mut rows, &[], "album", "asc");
    assert_eq!(ids(&rows), [2, 3, 1]);

    let mut rows = seed();
    sort_playlist_tracks(&mut rows, &[], "favorite", "asc");
    assert_eq!(ids(&rows), [2, 1, 3]);
    sort_playlist_tracks(&mut rows, &[], "favorite", "desc");
    assert_eq!(ids(&rows), [3, 1, 2]);

    let mut rows = seed();
    sort_playlist_tracks(&mut rows, &[], "length", "asc");
    assert_eq!(ids(&rows), [2, 3, 1]);
    sort_playlist_tracks(&mut rows, &[], "length", "desc");
    assert_eq!(ids(&rows), [1, 3, 2]);
}

/// A row drag and the list's own drag-pan are one gesture, and whichever element
/// owns it the other gets nothing: a pan intercepts the row's grab mid-drag, so
/// the drop never commits. The list around the rows opts out, and so does the
/// queue sheet's. Nothing else catches a regression here — it compiles,
/// reads clean, and only misbehaves on a list long enough to scroll.
///
/// The value was `!reorder-enabled`, which reads as leaving a `true` default alone
/// and is not one: every style but Material publishes this off, so that spelling
/// *enabled* the pan on every sort that retires the drag.
#[test]
fn every_draggable_list_opts_out_of_drag_panning() {
    // The binding rather than the token: the property reads as an *enable*, so
    // `mouse-drag-pan-enabled: true` is the likeliest wrong edit and a bare
    // occurrence count can't fail on it.
    let list = code_tokens(DRAGGABLE_LIST);
    assert_eq!(
        list.matches("mouse-drag-pan-enabled: false").count(),
        1,
        "`inner-list` must opt out, the only scroller the rows sit in"
    );
    assert!(
        code_tokens(QUEUE_SHEET).contains("mouse-drag-pan-enabled: false"),
        "every row in the queue sheet is draggable, so its ListView never gets the gesture"
    );
}

/// All four terms, since each fails differently and only the first is obvious.
/// `sort_playlist_tracks` reverses on `"desc"`, and the drag writes display
/// indices straight into `position_order`, so a reversed position sort lands
/// every drop at the mirrored slot; a filter makes those indices name a
/// different track entirely; and a smart playlist has no curated order to
/// reorder into. Dropping one of the last three costs no wrong write —
/// `apply_optimistic_reorder` refuses the filtered case and the query the empty
/// item set — but the drag then starts, paints a ghost and a drop line, and does
/// nothing on release, which is worse than a list that never armed.
#[test]
fn the_reorder_gate_reads_every_term_the_drag_depends_on() {
    let src = code_tokens(DETAIL_VIEW);
    assert!(
        src.contains(
            "reorder-enabled: PlaylistDetail.sort-field == \"position\" \
             && PlaylistDetail.sort-dir != \"desc\" \
             && PlaylistDetail.filter == \"\" \
             && !PlaylistDetail.playlist.is_smart"
        ),
        "playlist-detail.slint must gate reorder on an *ascending* position sort, \
         an unfiltered list and a playlist with a curated order"
    );
}

/// `"position"` is what drag-to-reorder is gated on and no header cell asks for
/// it, so the sort cycle is the only way back. Drop the natural field and the
/// first click on any column retires reordering for the whole install —
/// persisted, so it survives a restart.
///
/// Two halves in two files: the shared wiring has to hand the view's natural
/// order to the cycle, and this view has to name one.
#[test]
fn the_sort_cycle_still_offers_a_way_back_to_the_curated_order() {
    assert!(
        code_tokens(SHARED_DETAIL_WIRING)
            .contains("next_sort_with_natural(&field, &dir, &clicked, V::NATURAL_SORT)"),
        "the plain `next_sort` has two states and cannot reach `\"position\"`"
    );
    assert!(
        code_tokens(DETAIL)
            .contains("const NATURAL_SORT: Option<&'static str> = Some(POSITION_FIELD);"),
        "the cycle needs the curated order named as its third state"
    );
}

/// `is_manual_order` answers two of `reorder-enabled`'s terms; the filter is the
/// third the drag depends on, and the one that fails silently — filtered,
/// `tracks` is a subset of `position_order`, so a display index is still in
/// range and the DB write lands on a different track than the one dragged.
///
/// Scoped to the guard rather than searched for over the file, so a later site
/// reading the same field elsewhere in `detail.rs` can't satisfy the assertion
/// while the guard itself has stopped asking. Each needle carries its operator
/// for the same reason the drag-pan pin above matches a binding — a dropped `!`
/// or an `&&` in place of the `||` inverts the guard, and a bare token match
/// can't see either.
#[test]
fn the_optimistic_reorder_refuses_a_filtered_list() {
    let src = code_tokens(DETAIL);
    let body = src.split_once("pub fn apply_optimistic_reorder").map_or("", |(_, rest)| rest);
    let guard = body.split_once("let saved =").map_or("", |(head, _)| head);
    assert!(
        guard.contains("!is_manual_order(&field, &dir)"),
        "the order half must be asked before the display indices reach `position_order`"
    );
    assert!(
        guard.contains("|| playlists_ui.detail.cache.is_filtered()"),
        "so must the filter half — a filtered index is in range, so the write lands"
    );
}

#[test]
fn only_an_ascending_position_sort_is_the_manual_order() {
    assert!(is_manual_order(POSITION_FIELD, "asc"));
    assert!(!is_manual_order(POSITION_FIELD, "desc"));
    assert!(!is_manual_order("title", "asc"));
    assert!(!is_manual_order("title", "desc"));
    // `SortDir::from_token`'s rule — only `"desc"` is descending.
    assert!(is_manual_order(POSITION_FIELD, "sideways"));
}
