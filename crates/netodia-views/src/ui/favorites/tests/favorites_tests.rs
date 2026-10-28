use std::sync::Arc;

use super::{FavoritesTab, FavoritesUi};
use crate::ui::row_match::Needle;
use netodia_artwork::media::image::cover_thumbs::CoverThumbs;
use netodia_core::entities::artist::FavoriteArtist;
use netodia_testkit::write_test_png;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn favorite_row(id: i64) -> netodia_core::entities::track::TrackListRow {
    netodia_core::entities::track::TrackListRow {
        id,
        file_path: String::new(),
        file_name: format!("Song {id}"),
        title: format!("Song {id}"),
        artist: None,
        album_artist: None,
        album: None,
        genre: None,
        track_number: Some(i32::try_from(id).unwrap_or_default()),
        disc_number: Some(1),
        year: None,
        duration_ms: 0,
        artwork_path: None,
        is_favorite: true,
        rating: 0,
        album_id: None,
        artist_id: None,
        genre_id: None,
        date_added: String::new(),
        sort_key: None,
    }
}

fn favorites_with_songs() -> FavoritesUi {
    let ui = FavoritesUi::new(Arc::new(CoverThumbs::new()), None);
    ui.set_section_active(true);
    ui.state().tracks_all.store(vec![favorite_row(1), favorite_row(2)], "track_number", "asc");
    ui
}

#[test]
fn leaving_retains_the_banner_stats_and_composed_artwork_with_the_songs() {
    let ui = favorites_with_songs();
    *ui.state().stats.lock() = netodia_core::entities::track::FavoriteStats {
        count: 2,
        total_duration_ms: 600_000,
        artwork_paths: vec!["cover.jpg".to_owned()],
    };
    let fold = crate::ui::hero_folds::HeroFold { artists: 2, albums: 2 };
    *ui.state().songs_fold.lock() = fold;
    ui.state().hero_cache.lock().insert(
        FavoritesTab::Songs,
        (vec!["cover.jpg".to_owned()], crate::ui::detail_artwork::DetailPair::default()),
    );
    ui.set_section_active(false);

    ui.release_section_state();

    let stats = ui.state().stats.lock();
    assert_eq!((stats.count, stats.total_duration_ms), (2, 600_000));
    assert_eq!(*ui.state().songs_fold.lock(), fold);
    assert!(ui.state().hero_cache.lock().contains_key(&FavoritesTab::Songs));
    assert_eq!(ui.filtered_track_ids(), [1, 2]);
}

#[test]
fn banner_duration_includes_songs_after_the_fourth_distinct_cover() {
    let rows = (1..=6)
        .map(|id| {
            let mut row = favorite_row(id);
            row.duration_ms = 60_000;
            row.artwork_path = Some(format!("cover-{id}.jpg"));
            row
        })
        .collect::<Vec<_>>();

    let stats = super::hero::stats_from_tracks(&rows);

    assert_eq!((stats.count, stats.total_duration_ms), (6, 360_000));
    assert_eq!(stats.artwork_paths, ["cover-1.jpg", "cover-2.jpg", "cover-3.jpg", "cover-4.jpg"]);
}

#[test]
fn unlike_keeps_the_row_and_restoring_it_survives_the_visit() {
    let ui = favorites_with_songs();
    assert!(!ui.reconcile_likes(&std::collections::HashSet::from([2])));
    let snapshot = ui.state().tracks_all.snapshot();
    let needle = Needle::default();
    assert_eq!(snapshot.ids_filtered(&needle), vec![1, 2]);
    assert!(!snapshot.visible(&needle)[0].is_favorite);

    assert!(!ui.reconcile_likes(&std::collections::HashSet::from([1, 2])));
    assert!(ui.state().tracks_all.snapshot().visible(&needle)[0].is_favorite);
    ui.finish_visit();
    assert_eq!(ui.state().tracks_all.snapshot().ids_filtered(&needle), vec![1, 2]);
}

#[test]
fn leaving_favorites_removes_unliked_rows_from_the_next_visits_cache() {
    let ui = favorites_with_songs();
    ui.reconcile_likes(&std::collections::HashSet::from([2]));
    ui.set_section_active(false);

    ui.finish_visit();

    assert_eq!(ui.state().tracks_all.snapshot().ids_filtered(&Needle::default()), vec![2]);
    assert!(ui.state().retained_tracks.lock().is_empty());
    assert!(!ui.reconcile_likes(&std::collections::HashSet::new()));
    assert!(ui.state().retained_tracks.lock().is_empty());
}

#[test]
fn removing_the_last_favorites_still_leaves_rows_available_for_undo() {
    let ui = favorites_with_songs();

    assert!(!ui.reconcile_likes(&std::collections::HashSet::new()));

    assert_eq!(ui.state().tracks_all.snapshot().ids_filtered(&Needle::default()), vec![1, 2]);
    ui.finish_visit();
    assert!(ui.state().tracks_all.snapshot().is_empty());
}

#[test]
fn liking_an_absent_song_requests_metadata_without_removing_unliked_rows() {
    let ui = favorites_with_songs();

    assert!(ui.reconcile_likes(&std::collections::HashSet::from([2, 3])));

    assert_eq!(ui.state().tracks_all.snapshot().ids_filtered(&Needle::default()), vec![1, 2]);
    assert_eq!(*ui.state().retained_tracks.lock(), std::collections::HashSet::from([1]));
}

const GLOBAL: &str = include_str!("../../../../../netodia-ui/ui/globals/curated.slint");
const VIEW: &str = include_str!("../../../../../netodia-ui/ui/views/favorites-view.slint");
const MOST_PLAYED_TAB: &str =
    include_str!("../../../../../netodia-ui/ui/views/favorites/most-played-tab.slint");
const ARTISTS_TAB: &str =
    include_str!("../../../../../netodia-ui/ui/views/favorites/artists-tab.slint");
const GRID: &str =
    include_str!("../../../../../netodia-ui/ui/components/grid/entity-card-grid.slint");
const SONGS: &str = include_str!("../songs.rs");
const SONGS_TAB: &str =
    include_str!("../../../../../netodia-ui/ui/views/favorites/songs-tab.slint");
const CLOUD_SONGS_TAB: &str =
    include_str!("../../../../../netodia-ui/ui/views/favorites/cloud-favorites-songs-tab.slint");
const CLOUD_TRACKS: &str =
    include_str!("../../../../../netodia-ui/ui/views/my-library/cloud-playlist-tracks.slint");
const SUBVIEWS: &str = include_str!("../callbacks/subviews.rs");

/// `Favorites.tracks` feeds one element in the whole tree, under the Songs
/// tab's `if` — so off that tab, every prepared row the Songs path builds
/// reaches nothing and every row it leaves in the model is pinned behind a view
/// nobody can see.
///
/// Four things hold that, and each fails differently. `build_filtered_tracks`'
/// bail is what skips the cost; drop it and the tab gate becomes decorative,
/// with the whole favourites list still prepared per keystroke on the UI thread.
/// `write_filtered_tracks`' is what survives a pick landing mid-post; drop it
/// and the retention comes back on exactly the race the first one can't see. The
/// tab-leave **clear** is what empties what the last visit left; drop it and the
/// rows simply stay. And the tab pick writing through the **non-hopping**
/// `apply_filtered_tracks_now` is what stops that clear becoming visible: the
/// entering tab mounts on the next frame, and a posted write that loses the race
/// paints a `TrackList` of headers over nothing.
///
/// None of the four shows on screen except the last, and that one only for a
/// frame — which is why they are pinned rather than trusted.
#[test]
fn the_songs_model_is_written_only_while_its_tab_is_mounted() {
    assert!(
        SONGS_TAB.contains("rows: Favorites.tracks;"),
        "songs-tab.slint is the sole reader of `Favorites.tracks` — if that moved, this gate needs re-deriving"
    );

    let build =
        SONGS.split_once("fn build_filtered_tracks").map(|(_, rest)| rest).unwrap_or_default();
    assert!(
        build.contains("if fav_ui.active_tab() != FavoritesTab::Songs {"),
        "`build_filtered_tracks` must bail before the prepare walk when Songs isn't mounted"
    );

    let write =
        SONGS.split_once("fn write_filtered_tracks").map(|(_, rest)| rest).unwrap_or_default();
    assert!(
        write.contains("|| fav_ui.active_tab() != FavoritesTab::Songs"),
        "`write_filtered_tracks` must re-check the tab — a pick can land while the post is in flight"
    );

    assert!(
        SUBVIEWS.contains(
            r#"clear_vec_model::<UiTrackListRow>(&g.get_tracks(), "favorites: leave songs tab")"#
        ),
        "leaving the Songs tab must empty `Favorites.tracks`, the way `write_filtered_grids` empties the grid it isn't mounting"
    );
    assert!(
        SUBVIEWS.contains("apply_filtered_tracks_now(&ui, &fu)"),
        "the tab pick must write without hopping the event loop, or the clear above shows as a bare list"
    );
    assert!(CLOUD_TRACKS.contains("root.play-track(id, index)"));
    assert!(VIEW.contains("Favorites.play-cloud-row(id, index)"));
    assert!(VIEW.contains("Favorites.toggle-cloud-favorite(id, liked)"));
}

/// The tab count Slint declares today. Kept local so a change to
/// `Favorites.tab-count` doesn't silently rewrite what this asserts.
const TABS: usize = 2;

/// The `N` in `Favorites`'s `tab-count: N;`.
fn declared_tab_count() -> Option<usize> {
    netodia_testkit::declared_tab_count(GLOBAL)
}

/// The body of an inline `name: [ … ];` array literal in `favorites-view.slint`.
fn array_body(marker: &str) -> Option<&'static str> {
    netodia_testkit::array_body(VIEW, marker)
}

/// `Favorites.tab-count` is the sole definition of how many sub-views exist —
/// `seed_tab` clamps the persisted `views.json` index against it instead of
/// carrying its own const. Nothing else in the build notices when it drifts
/// from the tabs actually declared: a fourth tab added without bumping it
/// stays clickable but can never be restored from `views.json`, and a bump
/// without a matching body branch leaves the page blank on that tab.
#[test]
fn tab_count_matches_the_tabs_slint_declares() {
    let declared = declared_tab_count();
    assert_eq!(
        declared,
        Some(TABS),
        "curated.slint's `Favorites` must declare `out property <int> tab-count: {TABS};`"
    );
    let count = declared.unwrap_or_default();

    assert_eq!(
        FavoritesTab::ALL.len(),
        count,
        "`FavoritesTab` needs one variant per tab the global declares"
    );

    // Anchored on the branch's own shape (`… : ViewTransition {`) rather than
    // on the comparison alone: the hero reads `tab-idx` several more times
    // for its stats line, its placeholder and its two pill gates, and those
    // are not sub-views. It also pins that every sub-view is wrapped — one
    // mounted bare would appear without the sideways enter the others play.
    let branches = VIEW
        .lines()
        .filter(|line| line.contains("if Favorites.tab-idx == Favorites.tab-"))
        .filter(|line| line.contains(": ViewTransition {"))
        .count();
    assert_eq!(
        branches, count,
        "favorites-view.slint must mount one `ViewTransition` body branch per tab — a tab with no \
         branch shows a blank page"
    );

    let labels = array_body("labels: [");
    let icons = array_body("icons: [");
    assert!(
        labels.is_some() && icons.is_some(),
        "the tab bar's `labels`/`icons` must stay inline `[ … ];` array literals in \
         favorites-view.slint"
    );
    let labels = labels.unwrap_or_default();
    assert_eq!(
        labels.split(',').count(),
        count,
        "the tab bar's `labels` array is the wrong length"
    );
    // Counting `@tr(` too pins the "inline literal, never Rust-seeded"
    // contract: `@tr` registers msgids at codegen, so a `[string]` filled from
    // Rust would render untranslated.
    assert_eq!(
        labels.matches("@tr(\"").count(),
        count,
        "every tab label must stay an inline `@tr(\"…\")` literal"
    );
    assert_eq!(
        icons.unwrap_or_default().split(',').count(),
        count,
        "the tab bar's `icons` array is the wrong length"
    );
}

/// A tab pick has to clear the filter it was made under, and clear it on *both*
/// sides. A Songs needle carried into the Artists grid silently hides cards, and
/// the two halves fail differently: leaving the Slint property set leaves the
/// box holding text the page is no longer filtered by, and leaving the Rust
/// shadow set filters the entering tab's model against it.
#[test]
fn a_tab_pick_clears_the_filter_on_both_sides() {
    let handler = VIEW
        .split_once("tab-selected(i) =>")
        .and_then(|(_, rest)| rest.split_once("Favorites.tab-changed(i);"))
        .map_or("", |(body, _)| body);
    assert!(
        handler.contains("Favorites.filter = \"\";"),
        "favorites-view.slint's `tab-selected` handler must clear the Slint-side filter before \
         handing the pick to Rust"
    );

    assert!(
        SUBVIEWS.contains("favorites_ui_mod::set_filter(&fu, \"\");"),
        "the tab-change handler must drop the Rust filter shadow to match — the model build and \
         every later fetch read that, not the Slint property"
    );
}

/// The cloud-backed Favorites page exposes `NetEase`'s liked order and table sorting.
#[test]
fn favorites_view_uses_cloud_rows_with_sortable_netease_columns() {
    assert!(
        VIEW.contains("CloudFavoritesSongsTab {")
            && VIEW.contains("rows: Favorites.tracks;")
            && VIEW.contains("sort-field: Favorites.sort-field;")
            && VIEW.contains("request-sort(field) => { Favorites.request-sort(field); }"),
        "favorites-view.slint must mount the cloud-backed song list"
    );
    assert!(CLOUD_SONGS_TAB.contains("position-field: \"track_number\";"));
    assert!(CLOUD_SONGS_TAB.contains("favorite-clickable: true;"));
    assert!(CLOUD_TRACKS.contains("TrackList {"));
    assert!(VIEW.contains("columns <=> Favorites.track-columns;"));
    assert!(CLOUD_TRACKS.contains("root.toggle-favorite(id, liked)"));
    assert!(GLOBAL.contains("sort-field: \"track_number\";"));
    assert!(SONGS.contains("track_number: Some(favorite_order)"));
    assert!(SONGS.contains("state.cloud.liked_songs().await?"));
    assert_eq!(
        VIEW.matches("if Favorites.tab-idx == Favorites.tab-").count(),
        2,
        "Favorites has cloud-backed Songs and Albums tabs and no local-only grid tabs"
    );
}

/// The album tab must have its own mount and index rather than replacing the song list.
#[test]
fn favorites_view_exposes_the_cloud_album_collection_beside_songs() {
    assert!(VIEW.contains("if Favorites.tab-idx == Favorites.tab-songs:"));
    assert!(VIEW.contains("if Favorites.tab-idx == Favorites.tab-albums:"));
    assert!(VIEW.contains("CloudFavoritesAlbumsTab {"));
    assert!(VIEW.contains("tab-labels: [@tr(\"Songs\"), @tr(\"Albums\")];"));
    assert!(GLOBAL.contains("out property <int> tab-songs: 0;"));
    assert!(GLOBAL.contains("out property <int> tab-albums: 1;"));
    assert!(GLOBAL.contains("out property <int> tab-count: 2;"));
}

/// A `pure` callback's result is cached until a dependency is dirtied, so the
/// card's `cover` binding has to *read* `covers-generation` for the prewarm's
/// bump to re-run it. Drop the read and everything still builds and still looks
/// right on a warm tier — a freshly-entered tab just never shows a cover.
#[test]
fn the_grid_card_reads_the_covers_generation() {
    let binding =
        GRID.lines().map(str::trim).find(|line| line.starts_with("cover:")).unwrap_or_default();

    assert!(
        binding.contains("root.covers-generation"),
        "entity-card-grid.slint's `cover` binding must read `covers-generation` — otherwise the \
         host's bump can't invalidate a `pure` callback's cached result"
    );
}

/// Both halves have to be wired at every grid mount: the property, so the bump
/// reaches the cards, and the second callback argument, so Rust knows whether
/// the tier is warm enough to decode on. A mount that forgets either one pins
/// its grid at generation 0 — permanently coverless.
///
/// Counted by arity rather than by name: the one-argument form is a live signature
/// elsewhere in the tree, so a mount wired to it builds and only misbehaves on a cold tab.
#[test]
fn every_grid_mount_forwards_the_covers_generation() {
    // The two grid tabs live in their own files under `views/favorites/`; the
    // page itself only mounts them. Concatenated so the counts below stay one
    // arithmetic rather than one per tab.
    let grids: String = [MOST_PLAYED_TAB, ARTISTS_TAB].concat();
    let mounts = grids.matches("EntityCardGrid {").count();
    assert!(mounts > 0, "the Favorites tabs must still mount an `EntityCardGrid` each");

    assert_eq!(
        grids.matches("covers-generation: Favorites.covers-generation;").count(),
        mounts,
        "every `EntityCardGrid` mount must forward `Favorites.covers-generation`"
    );

    let forwards = grids
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("request-cover("))
        .filter(|line| line.split_once("=>").is_some_and(|(params, _)| params.contains(',')))
        .count();
    assert_eq!(
        forwards, mounts,
        "every `EntityCardGrid` mount must wire a `request-cover` that takes the generation \
         alongside the path and passes it on"
    );
}

/// A leave keeps the picture and drops the pixels. Cleared instead, the re-entry paints the
/// fallback glyph on every card for as long as the re-decode takes, which is the whole reason
/// the leave shrinks rather than releases.
#[test]
fn a_leave_keeps_a_proxy_of_what_the_grid_painted() -> TestResult {
    let (_tmp, path) = write_test_png(512)?;
    let path = path.to_str().ok_or("temp path is not UTF-8")?;

    let fav_ui = FavoritesUi::new(Arc::new(CoverThumbs::new()), None);
    fav_ui.set_active_tab(FavoritesTab::Artists);
    *fav_ui.state().fav_artists.lock() = vec![FavoriteArtist {
        id: 1,
        name: "Artist".to_owned(),
        image_path: Some(path.to_owned()),
        favorite_count: 2,
    }];

    fav_ui.set_section_active(true);
    fav_ui.prewarm_tab_covers(FavoritesTab::Artists);
    let warm = crate::ui::grid_prewarm::grid_cover(path).size().width;
    assert!(warm > 0, "a prewarm must leave its covers in the tier");

    fav_ui.set_section_active(false);
    crate::ui::grid_prewarm::hand_back_covers();
    let proxy = crate::ui::grid_prewarm::grid_cover(path).size().width;
    assert!(
        proxy > 0 && proxy < warm,
        "a leave must leave a smaller picture behind, not an empty one \
         (warm {warm}, after the leave {proxy})"
    );
    Ok(())
}

/// The cloud song fetch consumes and re-arms its dirty flag beside its cache.
#[test]
fn favorites_cloud_fetch_is_armed_beside_its_cache() {
    const LIFECYCLE: &str = include_str!("../callbacks/lifecycle.rs");
    const HANDLE: &str = include_str!("../mod.rs");
    const SONGS: &str = include_str!("../songs.rs");

    let kick = LIFECYCLE
        .split_once("async fn kick_full_refresh(")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .unwrap_or_default();
    assert!(
        kick.contains("fav_ui.take_songs_dirty();"),
        "`kick_full_refresh` must consume the cloud song cache's dirty flag"
    );

    let release = HANDLE
        .split_once("pub fn release_section_state(")
        .and_then(|(_, rest)| rest.split_once("\n    }"))
        .map(|(body, _)| body)
        .unwrap_or_default();
    assert!(
        release.contains("self.mark_songs_dirty();"),
        "`release_section_state` must re-arm a refresh even while retaining its last successful rows"
    );
    assert!(
        !release.contains("tracks_all.clear();"),
        "the last liked-song snapshot must remain available for immediate paint on re-entry"
    );
    let enter = LIFECYCLE
        .split_once("g.on_section_active_changed")
        .and_then(|(_, rest)| rest.split_once("wire_account_updates"))
        .map(|(body, _)| body)
        .unwrap_or_default();
    assert!(
        enter.contains("apply_filtered_tracks_now(&ui, &fu)")
            && enter.contains("fu.active_tab() == favorites_ui_mod::FavoritesTab::Songs"),
        "re-enter must paint cached songs synchronously before starting its refresh"
    );

    // Two ways of storing nothing per fetch: a failed query and a leave landing
    // mid-flight. `refresh_tracks` asks the section guard twice — before and
    // after cover downloads and the banner compose, and also on account-generation changes.
    assert!(
        SONGS.matches("mark_songs_dirty()").count() >= 4,
        "`refresh_tracks` must re-arm on failed, stale-account and hidden-section paths"
    );
}

/// A grid pick that spawns a fetch takes back the count its synchronous apply
/// just wrote.
///
/// The apply walks the cache the skipped tick left empty, so the `0` it writes
/// is the one value meaning "there is nothing here" — `GridEmptyState` over a
/// library that has plenty, for the length of the query plus the cover-decode
/// burst it awaits. Songs owes no equivalent and that asymmetry is the point:
/// `Favorites.track-count` is written by `refresh_hero`, which is never gated.
#[test]
fn a_grid_pick_rewinds_the_count_it_could_not_answer() {
    let pick = SUBVIEWS
        .split_once("fn wire_tab_changed(")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .unwrap_or_default();

    let (before_fetch, after_fetch) = pick
        .split_once(
            "if (needs_fetch || songs) && entering != favorites_ui_mod::FavoritesTab::Albums {",
        )
        .unwrap_or_default();
    assert!(
        before_fetch.contains("fu.take_songs_dirty()")
            && before_fetch.contains("fu.take_grids_dirty()"),
        "the pick must consume the entering tab's flag before the applies — the answer is what \
         decides whether the count they write stands for anything"
    );
    assert!(
        before_fetch.contains("apply_filtered_grids_now(&ui, &fu)"),
        "and the apply must come first, so a warm cache still paints on this tick"
    );
    for rewound in ["set_most_played_count(UNFETCHED_COUNT)", "set_artist_count(UNFETCHED_COUNT)"] {
        assert!(
            after_fetch.contains(rewound),
            "a grid pick that spawns a fetch must rewind that tab's count ({rewound})"
        );
    }
    assert!(
        !after_fetch.contains("set_track_count(UNFETCHED_COUNT)"),
        "Songs must not rewind: its count comes from the ungated `refresh_hero`, so it is \
         answered on every tab either way"
    );
}

/// And the apply that answers that rewind writes its counts **above** the
/// signature guard, or the sentinel above is permanent.
///
/// The pick stamps a signature over the cache its skipped tick left in place and
/// only then rewinds; the fetch it spawned lands on that same signature whenever
/// the content hasn't moved — an empty grid on a library with no favourites, or a
/// `stats_changed` tick for a track this query doesn't rank — so a count written
/// past the guard is one that never arrives. `-1` misses `> 0` as well as `== 0`,
/// so what strands is not only the empty state: `favorites-view.slint` gates the
/// Most Played Shuffle pill and the Artists sort row the other way, and both
/// vanish over a grid that is full.
///
/// The mutation to check is moving either write back under the guard, where it
/// reads as belonging with the model write beside it and compiles.
#[test]
fn the_grid_counts_are_written_before_the_signature_can_skip_them() {
    const GRIDS_APPLY: &str = include_str!("../grids/apply.rs");

    let write = GRIDS_APPLY
        .split_once("fn write_filtered_grids(")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .unwrap_or_default();

    assert!(
        write.contains("last_grid_signature"),
        "`write_filtered_grids` must still take the signature guard — this pin is about where \
         the counts sit relative to it, not about retiring it"
    );
    let before_guard = write.split_once("last_grid_signature").map_or("", |(head, _)| head);
    for count in ["set_most_played_count(", "set_artist_count("] {
        assert!(
            before_guard.contains(count),
            "{count} must be written above the signature guard: the guard is what a tab pick's \
             own fetch lands on when the content hasn't moved, and it would leave \
             `UNFETCHED_COUNT` standing with no answer coming"
        );
    }
}
