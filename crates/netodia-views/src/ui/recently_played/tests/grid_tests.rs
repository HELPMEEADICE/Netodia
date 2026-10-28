use super::apply::build_filtered_grid;
use super::warm::mounted_content;
use crate::ui::recently_played::{RecentlyPlayedTab, RecentlyPlayedUi};
use netodia_artwork::media::image::cover_thumbs::CoverThumbs;
use netodia_core::entities::track::MostPlayedFavorite;

/// An exhaustive struct literal on purpose: a new field on the entity fails this
/// file to compile, which is the prompt to check that it also belongs in the
/// derived `Hash` the grid compares against.
fn most_played(play_count: i32) -> MostPlayedFavorite {
    MostPlayedFavorite {
        id: 1,
        title: "Title".to_owned(),
        artist: Some("Artist".to_owned()),
        album_artist: None,
        album: Some("Album".to_owned()),
        genre: Some("Shoegaze".to_owned()),
        year: Some(1991),
        artwork_path: Some("/covers/track.jpg".to_owned()),
        play_count,
        duration_ms: 210_000,
        is_favorite: false,
    }
}

/// A handle with one played track cached and `tab` mounted.
fn seeded(tab: RecentlyPlayedTab, play_count: i32) -> RecentlyPlayedUi {
    let rp_ui = RecentlyPlayedUi::new(std::sync::Arc::new(CoverThumbs::new()), None);
    *rp_ui.state().most_played.lock() = vec![most_played(play_count)];
    rp_ui.set_active_tab(tab);
    rp_ui
}

/// The two sub-views are mutually exclusive `if`s, so a card row built for the
/// unmounted one reaches a grid nothing can scroll — one `EntityStripRow` of
/// `SharedString`s per played track in the library, built on the UI thread per
/// throttled keystroke and per `stats_changed` tick.
///
/// The mutation to catch is building the *wrong* tab, which the write side would
/// absorb in silence: it drops a `PreparedGrid` whose tab is no longer mounted,
/// so the grid would simply never fill and look like a fetch problem.
#[test]
fn only_the_mounted_tabs_rows_are_built() {
    assert_eq!(
        build_filtered_grid(&seeded(RecentlyPlayedTab::MostPlayed, 3)).most_played.len(),
        1,
        "the Most Played tab must build its own rows"
    );
    assert!(
        build_filtered_grid(&seeded(RecentlyPlayedTab::Songs, 3)).most_played.is_empty(),
        "the Songs tab mounts no grid, so building card rows for one is pure cost"
    );
}

/// **The cache isn't walked at all while the Songs tab is mounted.**
///
/// It used to be, for the count — which does gate the grid's `GridEmptyState`,
/// but only from inside the Most Played branch, and the band takes its facts
/// from `most_played_totals` rather than from here. So the walk answered nobody,
/// and it is not a cheap nobody: `get_most_played` is uncapped and library-wide,
/// and `apply_filtered_grid_now` reaches this **on the UI thread** from
/// `on_filter_changed` and `on_columns_changed` — a settled keystroke on the
/// Songs tab folding a needle against every played track and pushing six strings
/// each through a hasher.
///
/// The mutation to catch is restoring the both-tabs walk. Nothing observable
/// breaks — the count it computes is simply never rendered — so only the cost
/// comes back.
#[test]
fn nothing_is_walked_while_the_grid_is_unmounted() {
    let prepared = build_filtered_grid(&seeded(RecentlyPlayedTab::Songs, 3));
    assert_eq!(
        prepared.most_played_count, 0,
        "the Songs tab must not count a cache no mounted surface reads"
    );
    assert_eq!(
        prepared.most_played_content, 0,
        "the Songs tab must not hash it either — `mounted_content` already answers `0` there, \
         so the signature never depended on this walk"
    );
}

/// The mounted tab still publishes both, and a play-count flush still moves the
/// hash — the guard against a bail that swallowed the tab it was meant to serve.
#[test]
fn the_mounted_grid_publishes_its_count_and_hash() {
    let prepared = build_filtered_grid(&seeded(RecentlyPlayedTab::MostPlayed, 3));
    assert_eq!(prepared.most_played_count, 1);

    let bumped = build_filtered_grid(&seeded(RecentlyPlayedTab::MostPlayed, 4));
    assert_ne!(
        prepared.most_played_content, bumped.most_played_content,
        "a play-count flush must move the hash, else the apply that repaints the cards skips"
    );
}

/// The Songs tab mounts no grid, so nothing the grid could rebuild is on screen
/// there. A constant `0` is what stops a `stats_changed` tick — which reaches
/// both tabs — forcing a `set_vec` reset nobody can see. It is also what makes
/// the bail above safe: the signature was never reading this hash on Songs.
#[test]
fn the_songs_tab_contributes_no_mounted_content() {
    let on_songs = build_filtered_grid(&seeded(RecentlyPlayedTab::Songs, 3));
    assert_eq!(mounted_content(RecentlyPlayedTab::Songs, &on_songs), 0);

    let on_grid = build_filtered_grid(&seeded(RecentlyPlayedTab::MostPlayed, 3));
    assert_eq!(
        mounted_content(RecentlyPlayedTab::MostPlayed, &on_grid),
        on_grid.most_played_content,
        "the mounted grid must contribute its own hash"
    );
    assert_ne!(
        on_grid.most_played_content, 0,
        "…and that hash must be a real one, else this test passes on a grid that built nothing"
    );
}

/// The grid narrows with the hero search bar, so the ids a card hands to the
/// queue have to come through the same predicate the model build uses. Walking
/// the raw cache would enqueue cards that aren't on screen.
#[test]
fn the_queue_ids_follow_the_same_filter_as_the_cards() {
    let rp_ui = seeded(RecentlyPlayedTab::MostPlayed, 3);
    crate::ui::recently_played::set_filter(&rp_ui, "shoegaze");
    assert_eq!(
        build_filtered_grid(&rp_ui).most_played_count,
        rp_ui.most_played_cloud_tracks().len(),
        "the cards and the queue they load must agree about what's on screen"
    );

    crate::ui::recently_played::set_filter(&rp_ui, "nothing matches this");
    assert_eq!(build_filtered_grid(&rp_ui).most_played_count, 0);
    assert!(rp_ui.most_played_cloud_tracks().is_empty());
}

#[test]
fn every_cloud_card_keeps_its_identity_after_filtering() {
    let rp_ui = seeded(RecentlyPlayedTab::MostPlayed, 100);
    let row = |id, title: &str| MostPlayedFavorite { id, title: title.into(), ..most_played(42) };
    *rp_ui.state().most_played.lock() = vec![
        row(4_000_000_001, "Keep first"),
        row(4_000_000_002, "Other"),
        row(4_000_000_003, "Keep last"),
    ];

    let all = build_filtered_grid(&rp_ui);
    assert_eq!(all.most_played_count, 3);
    assert_eq!(all.most_played.iter().map(|row| row.id).collect::<Vec<_>>(), [1, 2, 3]);
    crate::ui::recently_played::set_filter(&rp_ui, "keep");
    let filtered = build_filtered_grid(&rp_ui);
    assert_eq!(filtered.most_played.iter().map(|row| row.id).collect::<Vec<_>>(), [1, 3]);
    assert_eq!(
        filtered.most_played.iter().map(|row| row.remote_id.as_str()).collect::<Vec<_>>(),
        ["4000000001", "4000000003"]
    );
    assert_eq!(
        rp_ui.most_played_cloud_tracks().iter().map(|track| track.id).collect::<Vec<_>>(),
        [4_000_000_001, 4_000_000_003]
    );
    assert!(rp_ui.most_played_cloud_tracks()[0].file_path.is_empty());
    assert_eq!(rp_ui.most_played_cloud_tracks()[0].duration_ms, 210_000);
}

#[test]
fn changing_accounts_clears_the_ranking_and_rejects_prepared_old_rows() {
    let rp_ui = seeded(RecentlyPlayedTab::MostPlayed, 100);
    let old = build_filtered_grid(&rp_ui);
    rp_ui.take_grid_dirty();
    *rp_ui.state().last_grid_signature.lock() = Some(123);

    rp_ui.clear_account_tracks();

    assert!(!rp_ui.grid_refresh_is_current(old.refresh_generation));
    assert!(rp_ui.most_played_cloud_tracks().is_empty());
    assert_eq!(rp_ui.state().most_played_totals.lock().tracks, 0);
    assert!(rp_ui.state().last_grid_signature.lock().is_none());
    assert!(rp_ui.take_grid_dirty());
}

#[test]
fn an_older_ranking_request_cannot_replace_a_newer_refresh() {
    let rp_ui = seeded(RecentlyPlayedTab::MostPlayed, 100);
    let first = rp_ui.begin_grid_refresh();
    let second = rp_ui.begin_grid_refresh();

    assert!(!rp_ui.grid_refresh_is_current(first));
    assert!(rp_ui.grid_refresh_is_current(second));
}

#[test]
fn ranking_rows_preserve_cloud_metadata_and_counts() {
    use netodia_core::entities::cloud::{Album, Artist, PlayedSong, Song};
    let row = super::fetch::most_played_row(
        PlayedSong {
            song: Song {
                id: 4_000_000_001,
                name: "Ranked song".into(),
                artists: vec![Artist { name: "Artist".into(), ..Artist::default() }],
                album: Album {
                    name: "Album".into(),
                    cover_url: Some("https://p1.music.126.net/a.jpg".into()),
                    ..Album::default()
                },
                duration_ms: 210_000,
                ..Song::default()
            },
            play_count: 123,
        },
        true,
    );

    assert_eq!(row.id, 4_000_000_001);
    assert_eq!(row.play_count, 123);
    assert_eq!(row.artist.as_deref(), Some("Artist"));
    assert_eq!(row.album.as_deref(), Some("Album"));
    assert_eq!(row.artwork_path.as_deref(), Some("https://p1.music.126.net/a.jpg"));
    assert_eq!(row.duration_ms, 210_000);
    assert!(row.is_favorite);
}
