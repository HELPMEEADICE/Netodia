use super::{
    ChipLabels, ChipOwner, FavoritesFacts, RecentlyPlayedFacts, album_chips, artist_chips,
    favorites_chips, genre_chips, playlist_chips, recently_played_chips, should_clear,
};
use crate::ui::favorites::FavoritesTab;
use crate::ui::hero_folds::{HeroFold, MostPlayedTotals};
use crate::ui::recently_played::RecentlyPlayedTab;
use netodia_core::entities::album::AlbumStats;
use netodia_core::entities::artist::ArtistStats;
use netodia_core::entities::genre::GenreStats;
use netodia_core::entities::playlist::PlaylistStats;
use slint::SharedString;

const STRIP: &str = include_str!("../../../../netodia-ui/ui/components/meta-chip-strip.slint");
const HERO_CHIPS: &str = include_str!("../hero_chips.rs");

/// Every source that *draws* a hero band. **Two, for six banners** — the two
/// mosaic pages wear one shared component and the four My Library details wear
/// another, so each file stands for the pages under it, which is the stronger
/// subject: one file to hold to the contract rather than six that can drift.
/// Whether each page still wears its band is [`MOSAIC_HOSTS`]' and
/// [`BAND_HOSTS`]' job.
const HERO_VIEWS: [(&str, &str); 2] = [
    (
        include_str!("../../../../netodia-ui/ui/components/hero/mosaic-tab-hero.slint"),
        "mosaic-tab-hero.slint",
    ),
    (
        include_str!("../../../../netodia-ui/ui/components/hero/library-tab-band.slint"),
        "library-tab-band.slint",
    ),
];

/// The two pages wearing the shared mosaic banner. They own no band of their own
/// — a title, a chip strip or an artwork size appearing in either is the
/// extraction coming undone one binding at a time.
const MOSAIC_HOSTS: [(&str, &str); 2] = [
    (include_str!("../../../../netodia-ui/ui/views/favorites-view.slint"), "favorites-view.slint"),
    (
        include_str!("../../../../netodia-ui/ui/views/recently-played-view.slint"),
        "recently-played-view.slint",
    ),
];

/// [`MOSAIC_HOSTS`] for `LibraryTabBand`: the sheet that mounts it, and the four
/// detail bodies that must mount **nothing** — the band morphs into each of their
/// heroes, so a title, a chip strip or an artwork size reappearing in one of them
/// is the page wearing two banners again. The bodies are the likelier regression
/// of the two, which is why pinning the sheet alone would not be enough.
const BAND_HOSTS: [(&str, &str); 5] = [
    (
        include_str!("../../../../netodia-ui/ui/views/my-library-view.slint"),
        "my-library-view.slint",
    ),
    (
        include_str!("../../../../netodia-ui/ui/views/my-library/album-detail.slint"),
        "my-library/album-detail.slint",
    ),
    (
        include_str!("../../../../netodia-ui/ui/views/my-library/artist-detail.slint"),
        "my-library/artist-detail.slint",
    ),
    (
        include_str!("../../../../netodia-ui/ui/views/my-library/genre-detail.slint"),
        "my-library/genre-detail.slint",
    ),
    (
        include_str!("../../../../netodia-ui/ui/views/my-library/playlist-detail.slint"),
        "my-library/playlist-detail.slint",
    ),
];

/// Stands in for the `HeroChips` global. Production routes through Slint's
/// `@tr` so the plurals follow the running locale; here they only have to be
/// distinguishable, so the labels are the English singular-agnostic form.
struct EnglishLabels;

impl ChipLabels for EnglishLabels {
    fn tracks(&self, count: i32) -> SharedString {
        SharedString::from(format!("{count} tracks"))
    }
    fn albums(&self, count: i32) -> SharedString {
        SharedString::from(format!("{count} albums"))
    }
    fn artists(&self, count: i32) -> SharedString {
        SharedString::from(format!("{count} artists"))
    }
    fn favorites(&self, count: i32) -> SharedString {
        SharedString::from(format!("{count} favorites"))
    }
    fn discs(&self, count: i32) -> SharedString {
        SharedString::from(format!("{count} discs"))
    }
    fn plays(&self, count: i32) -> SharedString {
        SharedString::from(format!("{count} plays"))
    }
    fn compilation(&self) -> SharedString {
        SharedString::from("Compilation")
    }
}

fn texts(chips: &[SharedString]) -> Vec<&str> {
    chips.iter().map(SharedString::as_str).collect()
}

fn album(year: Option<i32>, disc_count: Option<i32>, is_compilation: bool) -> AlbumStats {
    AlbumStats {
        id: 1,
        name: "Kind of Blue".into(),
        sort_name: None,
        artist_id: 1,
        artist_name: "Miles Davis".into(),
        year,
        disc_count,
        is_compilation,
        musicbrainz_release_group_id: None,
        label: None,
        catalog_number: None,
        barcode: None,
        media: None,
        release_type: None,
        release_country: None,
        musicbrainz_id: None,
        artwork_path: None,
        track_count: 5,
        total_duration_ms: 2_733_000,
    }
}

#[test]
fn an_album_leads_with_its_year() {
    let chips = album_chips(&EnglishLabels, &album(Some(1959), None, false), None);
    assert_eq!(texts(&chips), vec!["1959", "5 tracks", "45:33"]);
}

#[test]
fn an_album_omits_the_facts_it_does_not_have() {
    // Year 0 is the "unknown" sentinel the projection writes, a single disc is
    // every album's default, and a non-compilation has nothing to say.
    let chips = album_chips(&EnglishLabels, &album(Some(0), Some(1), false), None);
    assert_eq!(texts(&chips), vec!["5 tracks", "45:33"]);

    let chips = album_chips(&EnglishLabels, &album(None, None, false), None);
    assert_eq!(texts(&chips), vec!["5 tracks", "45:33"]);
}

#[test]
fn an_album_states_its_discs_compilation_flag_and_genre_when_it_has_them() {
    // Genre last — the one chip here that isn't about this release, so it is
    // the one worth losing first when the band runs out of room.
    let chips = album_chips(&EnglishLabels, &album(Some(1998), Some(3), true), Some("Jazz"));
    assert_eq!(texts(&chips), vec!["1998", "5 tracks", "45:33", "3 discs", "Compilation", "Jazz"]);
}

/// The release facts sit between the compilation flag and the genre, which is the order the band
/// drops them in: the genre is the one chip that isn't about this release, so it goes first.
#[test]
fn an_album_states_the_release_facts_its_tags_carry() {
    let released = AlbumStats {
        release_type: Some("Album".into()),
        media: Some("CD".into()),
        label: Some("ECM".into()),
        ..album(Some(1998), None, true)
    };

    assert_eq!(
        texts(&album_chips(&EnglishLabels, &released, Some("Jazz"))),
        vec!["1998", "5 tracks", "45:33", "Compilation", "Album", "CD", "ECM", "Jazz"]
    );
}

/// **Blank rather than absent is the case to check.** A tag read off a file arrives as an empty
/// string about as often as it is missing, and a chip drawing one is a pill with nothing in it.
#[test]
fn a_release_fact_the_tag_left_blank_states_nothing() {
    let blank = AlbumStats {
        release_type: Some(String::new()),
        media: Some("CD".into()),
        label: Some(String::new()),
        ..album(Some(1998), None, false)
    };

    assert_eq!(
        texts(&album_chips(&EnglishLabels, &blank, None)),
        vec!["1998", "5 tracks", "45:33", "CD"]
    );
}

#[test]
fn an_artist_states_albums_only_when_it_has_some() {
    let mut artist = ArtistStats {
        id: 1,
        name: "Miles Davis".into(),
        sort_name: None,
        musicbrainz_id: None,
        image_path: None,
        track_count: 12,
        album_count: 4,
        total_duration_ms: 3_600_000,
    };
    assert_eq!(
        texts(&artist_chips(&EnglishLabels, &artist, Some((1957, 1963)))),
        vec!["12 tracks", "4 albums", "1:00:00", "1957–1963"]
    );

    artist.album_count = 0;
    assert_eq!(texts(&artist_chips(&EnglishLabels, &artist, None)), vec!["12 tracks", "1:00:00"]);
}

#[test]
fn a_one_year_discography_reads_as_a_year_not_a_span() {
    let artist = ArtistStats {
        id: 1,
        name: "One Hit".into(),
        sort_name: None,
        musicbrainz_id: None,
        image_path: None,
        track_count: 3,
        album_count: 1,
        total_duration_ms: 600_000,
    };
    assert_eq!(
        texts(&artist_chips(&EnglishLabels, &artist, Some((1999, 1999)))),
        vec!["3 tracks", "1 albums", "10:00", "1999"]
    );
}

#[test]
fn a_genre_states_its_count_running_time_and_spread() {
    let genre =
        GenreStats { id: 1, name: "Jazz".into(), track_count: 30, total_duration_ms: 5_400_000 };
    assert_eq!(
        texts(&genre_chips(&EnglishLabels, &genre, HeroFold { artists: 12, albums: 18 })),
        vec!["30 tracks", "1:30:00", "12 artists", "18 albums"]
    );
}

#[test]
fn a_spread_of_one_is_not_worth_a_chip() {
    // A single-artist, single-album list says nothing by saying "1 artist".
    let genre =
        GenreStats { id: 1, name: "Jazz".into(), track_count: 5, total_duration_ms: 600_000 };
    assert_eq!(
        texts(&genre_chips(&EnglishLabels, &genre, HeroFold { artists: 1, albums: 1 })),
        vec!["5 tracks", "10:00"]
    );
}

#[test]
fn a_playlist_does_not_repeat_the_smart_badge() {
    // The title already carries `auto_awesome` when `is_smart`, so the chips
    // stay the same either way.
    let mut playlist = PlaylistStats {
        id: 1,
        name: "Focus".into(),
        description: None,
        thumbnail_path: None,
        is_smart: true,
        smart_criteria: None,
        created_at: String::new(),
        updated_at: String::new(),
        custom_thumbnail: false,
        track_count: 8,
        total_duration_ms: 1_800_000,
    };
    let fold = HeroFold { artists: 6, albums: 7 };
    let smart = texts(&playlist_chips(&EnglishLabels, &playlist, fold)).join("|");
    playlist.is_smart = false;
    let plain = texts(&playlist_chips(&EnglishLabels, &playlist, fold)).join("|");
    assert_eq!(smart, plain);
    assert_eq!(smart, "8 tracks|30:00|6 artists|7 albums");
}

#[test]
fn an_empty_entity_says_nothing_about_its_running_time() {
    let playlist = PlaylistStats {
        id: 1,
        name: "New Playlist".into(),
        description: None,
        thumbnail_path: None,
        is_smart: false,
        smart_criteria: None,
        created_at: String::new(),
        updated_at: String::new(),
        custom_thumbnail: false,
        track_count: 0,
        total_duration_ms: 0,
    };
    assert_eq!(
        texts(&playlist_chips(&EnglishLabels, &playlist, HeroFold::default())),
        vec!["0 tracks"]
    );
}

fn favorites_facts(tab: FavoritesTab) -> FavoritesFacts {
    FavoritesFacts {
        tab,
        tracks: 142,
        duration_ms: 33_273_000, // 9:14:33
        songs: HeroFold { artists: 37, albums: 51 },
        most_played: MostPlayedTotals { tracks: 88, duration_ms: 20_462_000, plays: 1204 },
        artists: 9,
    }
}

/// The same for Recently Played. Its Most Played tab is the whole library where
/// its Songs tab is the last 200 played, so the two totals are unrelated on
/// purpose — the sums-itself pin below reads them apart.
fn recently_played_facts(tab: RecentlyPlayedTab) -> RecentlyPlayedFacts {
    RecentlyPlayedFacts {
        tab,
        tracks: 200,
        duration_ms: 43_451_000, // 12:04:11
        songs: HeroFold { artists: 44, albums: 60 },
        most_played: MostPlayedTotals {
            tracks: 512,
            duration_ms: 118_800_000, // 33:00:00
            plays: 4096,
        },
    }
}

#[test]
fn the_favorites_chips_follow_the_tab() {
    assert_eq!(
        texts(&favorites_chips(&EnglishLabels, &favorites_facts(FavoritesTab::Songs))),
        vec!["142 favorites", "9:14:33", "37 artists", "51 albums"]
    );
    assert_eq!(
        texts(&favorites_chips(&EnglishLabels, &favorites_facts(FavoritesTab::Artists))),
        vec!["9 artists"]
    );
}

/// Most Played is `is_favorite AND play_count > 0` — a strict subset of Songs,
/// so it states its *own* duration. Borrowing the Songs one would overstate it
/// by every favourite never played, which is exactly the bug this pins.
#[test]
fn most_played_sums_itself_and_not_the_songs_tab() {
    let facts = favorites_facts(FavoritesTab::MostPlayed);
    assert_eq!(
        texts(&favorites_chips(&EnglishLabels, &facts)),
        vec!["88 tracks", "5:41:02", "1204 plays"]
    );
    let songs = texts(&favorites_chips(&EnglishLabels, &favorites_facts(FavoritesTab::Songs)))[1]
        .to_owned();
    assert_ne!(
        texts(&favorites_chips(&EnglishLabels, &facts))[1],
        songs,
        "Most Played must not reuse the Songs tab's duration"
    );
}

/// A favourite that has never been played is still on the tab (the query is
/// `play_count > 0`, so in practice it isn't — but the chip is gated on the sum,
/// not on the query). Zero plays says nothing worth a chip.
#[test]
fn a_never_played_most_played_tab_states_no_plays() {
    let facts = FavoritesFacts {
        most_played: MostPlayedTotals { tracks: 3, duration_ms: 600_000, plays: 0 },
        ..favorites_facts(FavoritesTab::MostPlayed)
    };
    assert_eq!(texts(&favorites_chips(&EnglishLabels, &facts)), vec!["3 tracks", "10:00"]);
}

/// Every empty band leaves the copy to the view — a sentence on the two Songs
/// heroes, a `GridEmptyState` on the three grids. A lone "0 …" chip beside one of
/// those is redundant at best.
#[test]
fn an_empty_hero_leaves_its_empty_state_to_the_view() {
    let empty = FavoritesFacts {
        tracks: 0,
        duration_ms: 0,
        songs: HeroFold::default(),
        most_played: MostPlayedTotals::default(),
        artists: 0,
        ..favorites_facts(FavoritesTab::Songs)
    };
    for tab in [FavoritesTab::Songs, FavoritesTab::MostPlayed, FavoritesTab::Artists] {
        let facts = FavoritesFacts { tab, ..empty };
        assert!(
            favorites_chips(&EnglishLabels, &facts).is_empty(),
            "the {tab:?} tab states a count over an empty list — the grid beneath it already \
             says so, and says it better"
        );
    }

    let empty = RecentlyPlayedFacts {
        tracks: 0,
        duration_ms: 0,
        songs: HeroFold::default(),
        most_played: MostPlayedTotals::default(),
        ..recently_played_facts(RecentlyPlayedTab::Songs)
    };
    for tab in [RecentlyPlayedTab::Songs, RecentlyPlayedTab::MostPlayed] {
        let facts = RecentlyPlayedFacts { tab, ..empty };
        assert!(
            recently_played_chips(&EnglishLabels, &facts).is_empty(),
            "the {tab:?} tab states a count over an empty list"
        );
    }
}

#[test]
fn recently_played_states_its_count_running_time_and_spread() {
    assert_eq!(
        texts(&recently_played_chips(
            &EnglishLabels,
            &recently_played_facts(RecentlyPlayedTab::Songs)
        )),
        vec!["200 tracks", "12:04:11", "44 artists", "60 albums"]
    );
}

/// The Most Played tab on this page is the *whole library*, where its Songs tab
/// is the last 200 played — so borrowing the Songs totals would understate it,
/// where on Favorites the same mistake overstates. Two different directions of
/// wrong, one rule: the tab sums itself.
#[test]
fn recently_playeds_most_played_sums_itself_too() {
    assert_eq!(
        texts(&recently_played_chips(
            &EnglishLabels,
            &recently_played_facts(RecentlyPlayedTab::MostPlayed)
        )),
        vec!["512 tracks", "33:00:00", "4096 plays"]
    );
}

/// `MetaChipStrip`'s brushes default to `Theme.*`, which is correct nowhere and wrong
/// invisibly: a theme token on a band whose colours are solved per artwork looks
/// plausible under Mocha and washes out under every light variant.
///
/// The six heroes discharge that through `HeroChipStrip`, which fixes both brushes so
/// the wrong mount can't be spelled — this pins that they mount the wrapper rather than
/// the raw strip, and that the wrapper still passes both. Now Playing keeps the raw
/// mount, its pair being `Player.np-accent-*`, and carries the obligation by hand.
#[test]
fn every_chip_strip_takes_its_brushes_from_its_backdrop() {
    const NOW_PLAYING: &str =
        include_str!("../../../../netodia-ui/ui/views/now-playing-view.slint");
    const WRAPPER: &str =
        include_str!("../../../../netodia-ui/ui/components/hero-chip-strip.slint");
    const CHIP: &str = include_str!("../../../../netodia-ui/ui/components/meta-chip.slint");

    for (src, name) in HERO_VIEWS {
        assert!(
            src.contains("HeroChipStrip {"),
            "{name} must mount `HeroChipStrip` — reaching past it to `MetaChipStrip` reopens the \
             `Theme.*` defaults one mount at a time"
        );
        assert!(
            !src.contains("MetaChipStrip {"),
            "{name} still mounts a raw `MetaChipStrip`; the hero wrapper is what fixes the brushes"
        );
    }

    // The six pages get theirs through whichever band they wear. A strip mounted
    // beside it is a second chip row on the same banner, which the `HeroChips`
    // global has no way to feed twice.
    for (src, name) in MOSAIC_HOSTS.iter().chain(BAND_HOSTS.iter()) {
        for mount in ["HeroChipStrip {", "MetaChipStrip {"] {
            assert!(
                !src.contains(mount),
                "{name} mounts `{mount}` itself — its chip row belongs to its shared band"
            );
        }
    }

    // Bounded by the `measured` handler rather than by a brace, so the
    // handler's own body can't end the window early.
    let mounts = [
        (WRAPPER, "hero-chip-strip.slint", "HeroBackdrop."),
        (NOW_PLAYING, "now-playing-view.slint", "Player.np-"),
    ];
    for (src, name, tier) in mounts {
        let mount = src
            .split_once("MetaChipStrip {")
            .and_then(|(_, rest)| rest.split_once("measured("))
            .map_or("", |(body, _)| body);
        assert!(
            !mount.is_empty(),
            "{name} no longer mounts a `MetaChipStrip` ahead of its `measured` handler"
        );
        for prop in ["chip-fill", "chip-label-color"] {
            assert!(
                mount.contains(&format!("{prop}: {tier}")),
                "{name}'s MetaChipStrip must pass `{prop}` a `{tier}` tier — omitting it falls \
                 back to the component's `Theme.*` default, a theme value on a solved backdrop"
            );
        }
    }

    // The layering the wrapper exists to preserve: the strip and the chip stay
    // global-free, which is what lets one component sit on a blurred cover
    // *and* on a blurred banner.
    for (src, name) in [(STRIP, "meta-chip-strip.slint"), (CHIP, "meta-chip.slint")] {
        // Import lines only — both files *mention* `HeroBackdrop` in the prose
        // arguing why they don't reach for it.
        let imports = src.lines().filter(|l| l.trim_start().starts_with("import "));
        for line in imports {
            assert!(
                !line.contains("hero-backdrop.slint") && !line.contains("player.slint"),
                "{name} must import neither hero nor player globals — the defaulted brushes are \
                 what let the same component paint on both, and the wrapper is where a tier is \
                 chosen"
            );
        }
    }
}

/// The chips *replace* the meta line rather than joining it. A half-done
/// conversion leaves the old property and its `Text` in place, and the band
/// then says the same thing twice.
#[test]
fn no_hero_view_still_declares_a_meta_line() {
    for (src, name) in HERO_VIEWS.iter().chain(MOSAIC_HOSTS.iter()).chain(BAND_HOSTS.iter()) {
        for prop in ["meta-line", "stats-line", "stats-text"] {
            assert!(
                !src.contains(&format!("property <string> {prop}")),
                "{name} still declares `{prop}` — the hero's counts are chips now, and a \
                 surviving meta line duplicates every one of them"
            );
        }
    }
}

/// The six banners are one band under different content, so the tile each leads
/// with is one number — `Theme.hero-artwork`.
///
/// Pinned beside the chip tests because `HERO_MAX_ROWS` is measured against that tile: a
/// view sizing its own artwork moves the slack the cap was picked for, and the band's
/// height with it. No hero spells a size — the details route through `LibraryTabBand`
/// and the mosaic pages through `MosaicTabHero`, neither exposing a knob to override the
/// token.
///
/// The tile check names `MosaicHeroTile` rather than the two views: the square lives
/// there, and each view's surviving `Theme.hero-artwork` is the *band height*, a
/// different fact. Asserted against the view alone, a size hardcoded in the component
/// passes.
#[test]
fn no_hero_view_sizes_its_own_artwork_tile() {
    const THEME: &str = include_str!("../../../../netodia-ui/ui/theme.slint");
    const MOSAIC_TILE: &str =
        include_str!("../../../../netodia-ui/ui/components/hero/mosaic-hero-tile.slint");

    assert!(
        THEME.contains("out property <length> hero-artwork:"),
        "Theme must own the hero tile size — it is the only thing keeping the six bands aligned"
    );
    for (src, name) in HERO_VIEWS.iter().chain(MOSAIC_HOSTS.iter()).chain(BAND_HOSTS.iter()) {
        assert!(
            !src.contains("artwork-size"),
            "{name} sizes its hero tile itself — it belongs on `Theme.hero-artwork`"
        );
        assert!(
            !src.contains("property <length> hero-artwork"),
            "{name} restates the hero tile size as a local property"
        );
    }
    // The mosaic square itself, which both heroes now mount rather than draw.
    for axis in ["width", "height"] {
        assert!(
            MOSAIC_TILE.contains(&format!("{axis}: Theme.hero-artwork;")),
            "MosaicHeroTile must take its {axis} from `Theme.hero-artwork` — a literal here \
             drifts both mosaic heroes away from the four detail bands at once, and every \
             view-level check still passes because each one keeps the token for its band height"
        );
    }

    // The banner the two mosaic pages share. It mounts the square rather than
    // drawing it — inline is what put the same chrome in two files once — and
    // derives its own band height from the same token.
    let band = HERO_VIEWS
        .iter()
        .find(|(_, name)| *name == "mosaic-tab-hero.slint")
        .map_or("", |(src, _)| *src);
    assert!(band.contains("MosaicHeroTile {"), "mosaic-tab-hero.slint must mount `MosaicHeroTile`");
    assert!(
        band.contains("Theme.hero-artwork"),
        "mosaic-tab-hero.slint derives its band height from the tile token, so it must read it"
    );

    // And that both pages still wear it. Asserted here rather than left implicit:
    // a page that reinlines its own band passes every check above, because the
    // shared file it stopped using is still correct.
    for (src, name) in MOSAIC_HOSTS {
        assert!(
            src.contains("MosaicTabHero {"),
            "{name} must mount `MosaicTabHero` — a page drawing its own band is how the two \
             drifted apart before, and the shared file goes on passing its own pins"
        );
    }

    // The My Library band draws the tile itself rather than through
    // `MosaicHeroTile` — the mosaic square and an artwork tile are different
    // things — so it owes the token directly.
    let library_band = HERO_VIEWS
        .iter()
        .find(|(_, name)| *name == "library-tab-band.slint")
        .map_or("", |(src, _)| *src);
    assert!(
        library_band.contains("tile-size: Theme.hero-artwork;"),
        "library-tab-band.slint must size its artwork tile from the token"
    );
    assert!(
        library_band.contains("Theme.hero-artwork"),
        "library-tab-band.slint derives its hero height from the tile token, so it must read it"
    );

    // The sheet wears the band; the four bodies wear nothing. The bodies are the
    // half worth pinning — a detail regrowing a header of its own is the shape
    // this phase removed, and every check above still passes while it does.
    for (src, name) in BAND_HOSTS {
        let expected = name == "my-library-view.slint";
        assert_eq!(
            src.contains("LibraryTabBand {"),
            expected,
            "{name} must {} mount `LibraryTabBand` — the page wears exactly one banner, and the \
             band morphs into each detail's hero rather than sitting above a second one",
            if expected { "" } else { "not" }
        );
    }
}

/// Favorites is assembled from three fetches and `kick_full_refresh` runs them
/// concurrently, so no ordering can be assumed: each folds its own answer and
/// republishes, which puts the fold and the republish in the same function as the fill.
///
/// The grid path alone doesn't cover it — `write_filtered_grids` publishes past a
/// signature early-return, and `mounted_content` is a constant `0` on the Songs tab, so
/// there that publish fires only on a column change.
#[test]
fn each_favorites_fetch_folds_and_republishes_what_it_feeds() {
    // The fold call and the store are checked apart, not as one glued
    // statement: the walk belongs on this worker but *outside* the section
    // gate, so the two are deliberately separate statements and a pin that
    // demanded them joined would fail the correct code. Nothing is lost by
    // splitting them — a fold whose result never reaches the store is an unused
    // binding, which `-D warnings` already refuses to compile.
    const FILLS: [(&str, &str, &str, &str, &str); 2] = [
        (
            include_str!("../favorites/songs.rs"),
            "favorites/songs.rs",
            "pub async fn refresh_tracks(",
            "crate::ui::hero_folds::fold_tracks(&rows)",
            "songs_fold.lock() =",
        ),
        (
            include_str!("../favorites/grids/fetch.rs"),
            "favorites/grids/fetch.rs",
            "pub async fn refresh_grids(",
            "crate::ui::hero_folds::fold_most_played(&rows)",
            "most_played_totals.lock() =",
        ),
    ];

    for (src, name, func, fold, store) in FILLS {
        let body = src
            .split_once(func)
            .and_then(|(_, rest)| rest.split_once("\n}"))
            .map_or("", |(body, _)| body);
        // Whitespace-normalised, so the pin holds a *statement* rather than the
        // line breaks it happens to be wrapped at — a fold long enough to wrap
        // is exactly the one this has to keep catching.
        let body: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            body.contains(fold),
            "{name}: `{func}` must fold `&rows` — the local it just awaited. Folding at publish \
             time instead puts the walk on the UI thread and makes the answer depend on which \
             sibling fetch landed first"
        );
        assert!(
            body.contains(store),
            "{name}: `{func}` folds `&rows` but stores the result nowhere the band can read it"
        );
        assert!(
            body.contains("republish_chips("),
            "{name}: `{func}` moves a number the Favorites band states but never republishes \
             it — the sibling fetch that did publish ran against the previous fold, and nothing \
             corrects it"
        );
    }
}

/// A fold follows its cache's lifetime: Most Played is released, while Songs retains its
/// rows, stats and spread for the immediate banner restore. Favorites' behavioral tests also
/// verify those retained values survive an actual call to `release_section_state`.
#[test]
fn the_section_leave_keeps_each_fold_with_the_cache_it_summarises() {
    const MOD: &str = include_str!("../favorites/mod.rs");

    let body = MOD
        .split_once("pub fn release_section_state(")
        .and_then(|(_, rest)| rest.split_once("\n    }"))
        .map_or("", |(body, _)| body);
    // Anchored on the statement the teardown ends with, so a window that
    // truncated early fails here rather than passing over nothing.
    assert!(
        body.contains("allocator::trim()"),
        "favorites/mod.rs no longer defines `release_section_state` as a body ending in \
         `allocator::trim()` — move this pin with it, or the checks below hold over an \
         empty window"
    );
    assert!(body.contains("most_played.lock().clear()"));
    assert!(body.contains("most_played_totals.lock() = MostPlayedTotals::default()"));
    assert!(!body.contains("tracks_all.clear()"));
    for field in ["songs_fold", "stats"] {
        assert!(
            !body.contains(&format!("{field}.lock() =")),
            "`release_section_state` retains the Songs cache, so `{field}` must survive with it"
        );
    }
}

/// No hero folds at publish time — each folds a local its own fetch just awaited, on
/// that worker. A fold whose argument is a `.lock()` is reading a cache some *other*
/// task fills, which is both a UI-thread walk and an ordering assumption.
///
/// `hero_chips.rs` is in the list because it is where the temptation lives, holding the
/// `FavoritesUi` handle with every cache one `.lock()` away.
#[test]
fn no_hero_folds_out_of_a_shared_cache() {
    const FOLDERS: [(&str, &str); 7] = [
        (include_str!("../albums/detail.rs"), "albums/detail.rs"),
        (include_str!("../artists/detail.rs"), "artists/detail.rs"),
        (include_str!("../genres/detail.rs"), "genres/detail.rs"),
        (include_str!("../playlists/detail.rs"), "playlists/detail.rs"),
        (include_str!("../recently_played/songs.rs"), "recently_played/songs.rs"),
        (include_str!("../recently_played/grid/fetch.rs"), "recently_played/grid/fetch.rs"),
        (HERO_CHIPS, "hero_chips.rs"),
    ];

    for (src, name) in FOLDERS {
        for fold in ["fold_tracks(", "fold_most_played(", "dominant_genre(", "year_span("] {
            for (i, m) in src.match_indices(fold) {
                let tail = &src[i + m.len()..];
                // Scan to the first `)`, which a `.lock()` argument reaches
                // *inside* itself — hence matching `.lock(` rather than the
                // full call, or the very shape being banned slips through.
                let args = tail.find(')').map_or(tail, |end| &tail[..end]);
                assert!(
                    !args.contains(".lock("),
                    "{name}: `{fold}` reads a shared cache — fold the rows this function \
                     awaited instead, or it inherits the Favorites ordering problem"
                );
            }
        }
    }
}

/// A publisher reads no Slint property it doesn't own.
///
/// Both mosaic heroes used to take their counts back off the global the caller
/// had just written, which made the write order load-bearing and left a
/// "after the counts, never before" comment standing in for a compiler check.
/// Facts arrive as arguments, or off the section's own handle.
#[test]
fn no_publisher_reads_its_facts_back_off_a_slint_global() {
    for publisher in ["pub fn publish_favorites(", "pub fn publish_recently_played("] {
        let body = HERO_CHIPS
            .split_once(publisher)
            .and_then(|(_, rest)| rest.split_once("\n}"))
            .map_or("", |(body, _)| body);
        // Anchored on the call every publisher ends with, so a window that
        // truncated early would fail here rather than pass by holding nothing.
        assert!(
            body.contains("publish(ui, ChipOwner::"),
            "hero_chips.rs no longer defines `{publisher}` as a body ending in `publish(...)` — \
             move this pin with it, or the checks below hold over an empty window"
        );
        for getter in ["get_track_count(", "get_duration_text(", "get_artist_count("] {
            assert!(
                !body.contains(getter),
                "`{publisher}` reads `{getter}` back off a Slint global. Take the fact as an \
                 argument or off the handle — reading it back makes the caller's write order \
                 part of the contract, and nothing enforces that"
            );
        }
    }
}

/// The strip owns the width mirror and the mount seed so that no host has to
/// restate either. Both are documented pitfalls: `changed` rejects a path
/// expression, and it does not fire on the first binding evaluation — so a
/// window opened at its final size would never report a width, and every hero
/// would collapse its chips into the single unlaid-out row forever.
#[test]
fn the_strip_reports_its_width_on_resize_and_at_mount() {
    assert!(
        STRIP.contains("property <length> watched-w: self.width;")
            && STRIP.contains("changed watched-w =>"),
        "MetaChipStrip must mirror its width into a local property — `changed` cannot watch \
         `self.width` through a path expression"
    );
    assert!(
        STRIP.contains("Timer {") && STRIP.contains("interval: 1ms;"),
        "MetaChipStrip must keep its 1 ms mount Timer — `changed` doesn't fire when the first \
         layout settles directly on the final value, which is every window opened at its own size"
    );
    assert_eq!(
        STRIP.matches("root.measured(").count(),
        2,
        "MetaChipStrip should report through `measured` exactly twice: once from the resize \
         handler and once from the mount seed"
    );
}

/// The strip's rows must sit under a plain layout, not directly under the root.
///
/// A root inheriting `Rectangle` folds a child *layout* into its own layout info but not
/// a conditional one, an `if` lowering to a repeater that `gen_layout_info_prop` skips.
/// With the `if` as the direct child the strip reports `preferred: 0`, takes its
/// `min-height` floor, and draws every row past the first *outside* that box. Nothing
/// fails to build, and it only shows once the chips actually wrap — a narrow window, or
/// a locale whose plurals run long.
///
/// Pinned as a shape rather than by counting braces: what matters is that something
/// non-conditional stands between the root and the `if`.
#[test]
fn the_strip_rows_hang_off_a_layout_not_off_the_root() {
    let normalized: String = STRIP.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        normalized.contains("VerticalLayout { if root.show-rows:"),
        "MetaChipStrip must wrap its conditional rows in a plain layout — without it the root \
         reports `preferred: 0` vertically and a wrapped second row paints outside the strip. \
         See the `if`-child entry in `.claude/rules/slint-pitfalls.md`"
    );
}

/// ...and that wrapper owes a `min-width: 0px`, which is the same decision seen
/// from the other axis.
///
/// A layout child folds into a `Rectangle` root's layout info in *both* orientations, so
/// the wrapper above hands the root a horizontal `min` equal to the widest published
/// row. A box layout never shrinks a cell past its `min`, so the strip keeps being
/// allotted the width it was chunked at — and keeps reporting it through `measured` —
/// while the panel narrows: the chunker never learns there is less room, and the row
/// clips instead of wrapping.
///
/// An explicit `min-width` replaces the merged constraint rather than maxing with it, as
/// the `min-height` beside it does, so one line is the whole fix.
#[test]
fn the_strip_leaks_no_width_floor() {
    assert!(
        STRIP.contains("min-width: 0px;"),
        "MetaChipStrip must pin `min-width: 0px` — its rows wrapper is a real layout, so without \
         it the widest chip row becomes a floor the hero column can't shrink past, and the chips \
         clip on a narrowing window instead of wrapping into a second row"
    );
}

/// The strip's two gaps are different numbers, and only the horizontal one
/// crosses into Rust.
///
/// `ui::chips::SPACING` restates the *inter-chip* gap because the wrap packs each row
/// against it; the gap between rows is Slint's alone. Collapsing the two into one
/// `pad-sm` is the tempting simplification and costs the hero band slack it doesn't
/// have, so a wrapped second row overruns the artwork tile. The other way is quieter and
/// worse: a narrower inter-chip gap leaves `SPACING` over-estimating every row, so the
/// strip wraps early and drops chips that would fit.
#[test]
fn the_strip_spaces_its_rows_tighter_than_its_chips() {
    // Split on the repeater, so each half holds exactly one `spacing:` — the
    // rows layout's before it, the row layout's after.
    let (rows, chips) = STRIP.split_once("for row in root.rows:").unwrap_or_default();

    let row_gap = rows.rsplit_once("spacing: Theme.").map_or("", |(_, gap)| gap);
    assert!(
        row_gap.starts_with("pad-xs;"),
        "the gap *between* chip rows must be `pad-xs` — at `pad-sm` a wrapped second row \
         overruns the hero's artwork tile and the band grows on every wrap"
    );

    let chip_gap = chips.split_once("spacing: Theme.").map_or("", |(_, gap)| gap);
    assert!(
        chip_gap.starts_with("pad-sm;"),
        "the gap *between chips within a row* must stay `pad-sm` — `ui::chips::SPACING` is 8.0 \
         and packs every row against exactly this number, so a change here desynchronises the \
         wrap silently"
    );
}

/// Album and Playlist are the two heroes carrying a second text line, and in
/// `LibraryTabBand` it is **one** line the sheet feeds from either — a single
/// `subtitle` row under the title, collapsing on `""` for the other two details
/// and for idle.
///
/// It sits on a row of its own, forced rather than chosen: the band's search box is up
/// in the tab row, so the line costs its full box plus a gap out of a meta column
/// bounded by `Theme.hero-artwork` less the pill band. What that buys back is that there
/// is exactly one of it. Three things stay pinned: the size, `HERO_MAX_ROWS`' slack being
/// measured against what the title block spends; that the line collapses when empty, an
/// always-mounted row spending it on the four heroes that state nothing there; and the
/// optical inset, whose wrapper reads as redundant to anyone reaching for the `Text` inside
/// it.
#[test]
fn the_subtitled_heroes_share_one_collapsing_line() {
    const SHEET: &str = include_str!("../../../../netodia-ui/ui/views/my-library-view.slint");
    const SUBTITLE: &str = "if root.subtitle != \"\": HorizontalLayout {";

    let band = HERO_VIEWS
        .iter()
        .find(|(_, name)| *name == "library-tab-band.slint")
        .map_or("", |(src, _)| *src);
    let normalized: String = band.split_whitespace().collect::<Vec<_>>().join(" ");

    assert_eq!(
        normalized.matches(SUBTITLE).count(),
        1,
        "library-tab-band.slint must carry exactly one collapsing subtitle row — an \
         always-mounted one spends a wrapped chip row's worth of the tile on the four heroes \
         that state nothing under the title. See `ui::hero_chips::HERO_MAX_ROWS`"
    );

    // Bounded on the chip strip rather than on a brace: the line's own `text`
    // opens with a `\u{200e}` LRM escape, so the first `}` after it is inside a
    // string literal. Bounding here pins the order too — the subtitle is above
    // the chips, which is what leaves the wrap its slack.
    let block = normalized
        .split_once(SUBTITLE)
        .and_then(|(_, rest)| rest.split_once("HeroChipStrip"))
        .map_or("", |(block, _)| block);
    assert!(
        !block.is_empty(),
        "library-tab-band.slint must declare its chip strip after the subtitle row"
    );
    assert!(
        block.contains("font-size: Theme.font-size-md;"),
        "the band's second line must stay at `font-size-md` — the meta column is bounded by the \
         hero tile, and a larger line spends the slack `HERO_MAX_ROWS` is measured against"
    );
    assert!(
        block.contains("padding-left:"),
        "the subtitle row must keep its optical inset. The wrapper exists only to carry it, so \
         folding it back to a bare `Text` reads as a simplification and silently puts the line \
         back on the title's *layout* edge, which is not the title's ink edge — the two run at \
         different sizes and weights, so their side bearings differ. Not an `x` on the `Text`: a \
         layout child that binds one is dropped from its parent's layout info"
    );

    // The two facts that reach it. The band is data-agnostic, so which entity
    // owns a second line is the sheet's ternary and nowhere else.
    let sheet: String = SHEET.split_whitespace().collect::<Vec<_>>().join(" ");
    for fact in ["AlbumDetail.album.artist_name", "PlaylistDetail.playlist.description"] {
        assert!(
            sheet.contains(&format!("? {fact}")),
            "my-library-view.slint must feed `{fact}` into the band's `subtitle` — the band \
             spells no entity of its own"
        );
    }
}

/// Every hero title is one number, and it lives on `Theme`.
///
/// The same argument `hero-artwork` makes: the banners are one band under different
/// content, so a title that changes size between them reads as the page shifting under a
/// nav. Not hypothetical — the six had already drifted to two sizes with nothing
/// recording which was intended. A literal is what reintroduces that, and it is
/// invisible in review because each view reads correctly on its own.
#[test]
fn every_hero_title_reads_the_same_token() {
    const THEME: &str = include_str!("../../../../netodia-ui/ui/theme.slint");
    // A hero title is the one `Text` in each view carrying
    // `page-title-weight`, and its size is always the binding directly above.
    // Counting the pair against the weight alone is what catches a literal:
    // the view still builds, still looks right on its own, and only diverges
    // from the other five.
    const WEIGHT: &str = "font-weight: Theme.page-title-weight;";
    const HERO_TITLE: &str =
        "font-size: Theme.hero-title-size; font-weight: Theme.page-title-weight;";
    // `LibraryTabBand`'s idle line, which is a *page* heading rather than a hero
    // one — it names the mounted tab's count, not an entity — so it reads the
    // page token. Admitted here so the literal check below stays exact.
    const IDLE_TITLE: &str =
        "font-size: Theme.page-title-size; font-weight: Theme.page-title-weight;";

    assert!(
        THEME.contains("out property <length> hero-title-size:"),
        "Theme must own the hero title size — it is the only thing keeping the six banners' \
         headings one size"
    );
    for (src, name) in HERO_VIEWS {
        let normalized: String = src.split_whitespace().collect::<Vec<_>>().join(" ");
        let headings = normalized.matches(WEIGHT).count();
        assert!(normalized.contains(HERO_TITLE), "{name} declares no hero title");
        assert_eq!(
            normalized.matches(HERO_TITLE).count() + normalized.matches(IDLE_TITLE).count(),
            headings,
            "{name} sizes a heading with something other than `Theme.hero-title-size` or \
             `Theme.page-title-size` — the six banners are one band under different content, and \
             they had already drifted to two sizes once"
        );
    }

    // The six pages take their heading from whichever band they wear, so a title
    // reappearing in one is a second one — and the copy that would be free to
    // drift.
    for (src, name) in MOSAIC_HOSTS.iter().chain(BAND_HOSTS.iter()) {
        assert!(
            !src.contains(WEIGHT),
            "{name} declares a hero title of its own; its shared band owns that heading"
        );
    }
}

/// **A teardown clears only chips it owns**, and the four cases are the four
/// things a leave can be. The record answers all of them; the departing view — which
/// is what used to be asked — can answer none.
///
/// The two `false` arms are the bugs this shape exists to stop, and they pull in
/// opposite directions, which is why neither a bare clear nor a bare hold works.
#[test]
fn a_teardown_clears_only_a_row_the_band_has_stopped_painting() {
    let album = ChipOwner::Album(7);
    let artist = ChipOwner::Artist(3);

    assert!(
        !should_clear(album, Some(album), true),
        "the band is still painting this row — Now Playing merely *covering* a page lands \
         here, and blinking its counts out and back is a flicker on every press of `F`"
    );
    assert!(
        !should_clear(album, None, true),
        "nothing took over and the owner's own id survives: the band is 400 ms into \
         collapsing over this banner, and `hero-collapsed` asks again at the end of it"
    );
    assert!(
        should_clear(album, None, false),
        "the detail really closed, so its counts are nobody's"
    );
    assert!(
        should_clear(album, Some(artist), true),
        "another hero owns the band and has not published yet — holding here states the \
         outgoing entity's facts under the incoming one's title"
    );

    // The case the drill fix created, and the one a `$departing`-shaped guard got
    // wrong: the destination published in the very tick that moved the tab, so the
    // leave that follows in the same change-handler drain must leave it alone.
    assert!(
        !should_clear(artist, Some(artist), false),
        "a cross-tab drill publishes before the departing tab's leave fires, so the row \
         the leave sees already belongs to the hero that took over"
    );
}

/// Which tab's id decides whose chips the band is painting.
///
/// The arm worth the test is **Songs**: it is the one tab with no detail, so a `match`
/// that folded it in with the rest would read whichever id happened to sit in the
/// default arm and hold the previous entity's counts over a plain track list. The
/// `-1` cases are the other half — every close and every failed re-fetch clears its id
/// first, and that is what keeps the teardown from being a leak.
#[test]
fn only_the_mounted_tabs_own_id_names_the_hero() {
    use crate::ui::my_library::MyLibraryTab::{Albums, Artists, Genres, Playlists, Songs};

    // One id set at a time, so a tab reading a sibling's is a failure rather than a
    // coincidence — the shape a `match` arm gets wrong. Boot makes that reachable
    // rather than theoretical: `seed_detail_from_settings` restores one detail per
    // view whichever tab is resumed, so several ids are `>= 0` at once.
    for (tab, ids, playlist, owner) in [
        (Albums, [7, -1, -1], -1, ChipOwner::Album(7)),
        (Artists, [-1, 7, -1], -1, ChipOwner::Artist(7)),
        (Genres, [-1, -1, 7], -1, ChipOwner::Genre(7)),
        (Playlists, [-1, -1, -1], 3_000_000_000_i64, ChipOwner::Playlist(3_000_000_000)),
    ] {
        let [album, artist, genre] = ids;
        assert_eq!(
            super::my_library_owner(tab, album, artist, genre, playlist),
            Some(owner),
            "{tab:?} must name its own detail"
        );
        assert_eq!(
            super::my_library_owner(tab, -1, -1, -1, -1),
            None,
            "{tab:?} must report nothing once its id is cleared"
        );
        assert_eq!(
            super::my_library_owner(Songs, album, artist, genre, playlist),
            None,
            "Songs has no detail — it must not answer for {tab:?}'s id"
        );
    }

    // `0` is a real row id, so the boundary is `>= 0` and not `> 0`.
    assert_eq!(super::my_library_owner(Albums, 0, -1, -1, -1), Some(ChipOwner::Album(0)));
}
