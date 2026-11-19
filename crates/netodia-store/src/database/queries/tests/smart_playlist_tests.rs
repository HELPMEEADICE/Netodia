use std::collections::HashSet;

use jiff::{SignedDuration, Timestamp, tz::Offset};
use sqlx::{QueryBuilder, Sqlite};

use crate::database::DbPool;
use crate::database::queries;
use crate::database::queries::fixtures::{
    insert_tagged_track, insert_test_track, make_test_metadata,
};
use netodia_core::entities::credits::{CreditRole, RoleCredit, RoleCredits};
use netodia_core::entities::genre::GenreList;
use netodia_core::entities::smart_criteria::{
    FIELDS, LimitOrder, MatchMode, Rule, RuleField, RuleOp, RuleValue, SmartCriteria, SmartLimit,
    ops_for,
};
use netodia_core::entities::track::TrackListRow;
use netodia_core::error::AppError;
use netodia_core::utils::format_rfc3339;

/// RFC-3339 timestamp `n` days before now — for staging `last_played`.
fn days_ago(n: i64) -> String {
    let stamp = Timestamp::now() - SignedDuration::from_secs(n * 86_400);
    format_rfc3339(stamp, Offset::UTC)
}

/// Stage playback stats the scan-insert helper leaves at defaults.
async fn set_stats(
    db: &DbPool,
    id: i64,
    rating: i32,
    play_count: i32,
    is_favorite: bool,
    last_played: Option<&str>,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE tracks SET rating = ?, play_count = ?, is_favorite = ?, \
         last_played = ? WHERE id = ?",
    )
    .bind(rating)
    .bind(play_count)
    .bind(is_favorite)
    .bind(last_played)
    .bind(id)
    .execute(db.write())
    .await?;
    Ok(())
}

struct Seed {
    db: DbPool,
    t1: i64, // Rock, rating 5, plays 10, favorite, played 1 day ago
    t2: i64, // Rock, rating 2, plays 1,  not fav,   played 400 days ago
    t3: i64, // Pop,  rating 4, plays 5,  favorite,  never played
    t4: i64, // Jazz, rating 0, plays 0,  not fav,   never played
}

async fn seed() -> Result<Seed, AppError> {
    let db = DbPool::test_pool().await?;
    queries::folder::insert_folder(&db, "/music", true).await?;

    let t1 = insert_test_track(&db, "/music/1.mp3", "One", "Artist A", "Album A", "Rock").await?;
    let t2 = insert_test_track(&db, "/music/2.mp3", "Two", "Artist B", "Album B", "Rock").await?;
    let t3 = insert_test_track(&db, "/music/3.mp3", "Three", "Artist C", "Album C", "Pop").await?;
    let t4 = insert_test_track(&db, "/music/4.mp3", "Four", "Artist D", "Album D", "Jazz").await?;

    set_stats(&db, t1, 5, 10, true, Some(&days_ago(1))).await?;
    set_stats(&db, t2, 2, 1, false, Some(&days_ago(400))).await?;
    set_stats(&db, t3, 4, 5, true, None).await?;
    set_stats(&db, t4, 0, 0, false, None).await?;

    Ok(Seed { db, t1, t2, t3, t4 })
}

fn one(field: RuleField, op: RuleOp, value: Option<RuleValue>) -> SmartCriteria {
    SmartCriteria { rules: vec![Rule { field, op, value }], ..SmartCriteria::default() }
}

fn ids(rows: &[TrackListRow]) -> HashSet<i64> {
    rows.iter().map(|r| r.id).collect()
}

async fn resolve(db: &DbPool, c: &SmartCriteria) -> Result<Vec<TrackListRow>, AppError> {
    queries::smart_playlist::get_smart_playlist_tracks(db, c).await
}

#[tokio::test]
async fn genre_contains() -> Result<(), AppError> {
    let s = seed().await?;
    let c = one(RuleField::Genre, RuleOp::Contains, Some(RuleValue::Text("Rock".to_owned())));
    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.t1, s.t2]));
    Ok(())
}

#[tokio::test]
async fn rating_gte() -> Result<(), AppError> {
    let s = seed().await?;
    let c = one(RuleField::Rating, RuleOp::Gte, Some(RuleValue::Number(4.0)));
    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.t1, s.t3]));
    Ok(())
}

#[tokio::test]
async fn play_count_gt() -> Result<(), AppError> {
    let s = seed().await?;
    let c = one(RuleField::PlayCount, RuleOp::Gt, Some(RuleValue::Number(3.0)));
    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.t1, s.t3]));
    Ok(())
}

#[tokio::test]
async fn favorite_is_true() -> Result<(), AppError> {
    let s = seed().await?;
    let c = one(RuleField::Favorite, RuleOp::IsTrue, None);
    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.t1, s.t3]));
    Ok(())
}

#[tokio::test]
async fn last_played_never() -> Result<(), AppError> {
    let s = seed().await?;
    let c = one(RuleField::LastPlayed, RuleOp::IsNotSet, None);
    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.t3, s.t4]));
    Ok(())
}

#[tokio::test]
async fn last_played_in_last_30_days() -> Result<(), AppError> {
    let s = seed().await?;
    let c = one(RuleField::LastPlayed, RuleOp::InLast, Some(RuleValue::Days(30)));
    // Only t1 (played 1 day ago); t2 is 400 days ago, t3/t4 never.
    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.t1]));
    Ok(())
}

#[tokio::test]
async fn last_played_not_in_last_365_days() -> Result<(), AppError> {
    let s = seed().await?;
    let c = one(RuleField::LastPlayed, RuleOp::NotInLast, Some(RuleValue::Days(365)));
    // Old (t2) plus the never-played (t3, t4); t1 is recent → excluded.
    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.t2, s.t3, s.t4]));
    Ok(())
}

#[tokio::test]
async fn match_all_is_intersection() -> Result<(), AppError> {
    let s = seed().await?;
    let c = SmartCriteria {
        match_mode: MatchMode::All,
        rules: vec![
            Rule {
                field: RuleField::Genre,
                op: RuleOp::Contains,
                value: Some(RuleValue::Text("Rock".to_owned())),
            },
            Rule { field: RuleField::Rating, op: RuleOp::Gte, value: Some(RuleValue::Number(4.0)) },
        ],
        ..SmartCriteria::default()
    };
    // Rock AND rating>=4 → only t1 (t2 is Rock but rating 2).
    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.t1]));
    Ok(())
}

#[tokio::test]
async fn match_any_is_union() -> Result<(), AppError> {
    let s = seed().await?;
    let c = SmartCriteria {
        match_mode: MatchMode::Any,
        rules: vec![
            Rule {
                field: RuleField::Genre,
                op: RuleOp::Is,
                value: Some(RuleValue::Text("Pop".to_owned())),
            },
            Rule { field: RuleField::Rating, op: RuleOp::Gte, value: Some(RuleValue::Number(4.0)) },
        ],
        ..SmartCriteria::default()
    };
    // Pop (t3) OR rating>=4 (t1, t3) → {t1, t3}.
    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.t1, s.t3]));
    Ok(())
}

#[tokio::test]
async fn limit_caps_and_orders() -> Result<(), AppError> {
    let s = seed().await?;
    let c = SmartCriteria {
        rules: vec![Rule {
            field: RuleField::PlayCount,
            op: RuleOp::Gt,
            value: Some(RuleValue::Number(0.0)),
        }],
        limit: Some(SmartLimit { count: 2, order: LimitOrder::PlayCountDesc }),
        ..SmartCriteria::default()
    };
    let rows = resolve(&s.db, &c).await?;
    // plays>0 → t1(10), t3(5), t2(1); top-2 by play_count desc → [t1, t3].
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, s.t1);
    assert_eq!(rows[1].id, s.t3);
    Ok(())
}

#[tokio::test]
async fn empty_rules_match_whole_library() -> Result<(), AppError> {
    let s = seed().await?;
    let rows = resolve(&s.db, &SmartCriteria::default()).await?;
    assert_eq!(rows.len(), 4);
    Ok(())
}

#[tokio::test]
async fn count_matches_membership() -> Result<(), AppError> {
    let s = seed().await?;
    let c = one(RuleField::Rating, RuleOp::Gte, Some(RuleValue::Number(4.0)));
    let (count, duration) = queries::smart_playlist::count_smart_playlist(&s.db, &c).await?;
    // t1 + t3, each 180_000 ms (helper default duration).
    assert_eq!(count, 2);
    assert_eq!(duration, 360_000);
    Ok(())
}

#[tokio::test]
async fn count_respects_limit() -> Result<(), AppError> {
    let s = seed().await?;
    let c = SmartCriteria {
        rules: vec![Rule {
            field: RuleField::PlayCount,
            op: RuleOp::Gt,
            value: Some(RuleValue::Number(0.0)),
        }],
        limit: Some(SmartLimit { count: 2, order: LimitOrder::PlayCountDesc }),
        ..SmartCriteria::default()
    };
    let (count, _) = queries::smart_playlist::count_smart_playlist(&s.db, &c).await?;
    // 3 tracks match plays>0 but the limit caps the reported size at 2.
    assert_eq!(count, 2);
    Ok(())
}

#[tokio::test]
async fn count_random_limit_caps_without_ordering() -> Result<(), AppError> {
    let s = seed().await?;
    // A `Random` limit skips the pointless `ORDER BY RANDOM()` in the count
    // path. COUNT is still min(limit, matches); and since every seeded track
    // shares the helper's 180_000 ms duration, the SUM of any 2 rows is
    // deterministic regardless of which 2 the LIMIT keeps.
    let c = SmartCriteria {
        rules: Vec::new(),
        limit: Some(SmartLimit { count: 2, order: LimitOrder::Random }),
        ..SmartCriteria::default()
    };
    let (count, duration) = queries::smart_playlist::count_smart_playlist(&s.db, &c).await?;
    assert_eq!(count, 2);
    assert_eq!(duration, 360_000);
    Ok(())
}

#[tokio::test]
async fn duration_rule_value_is_seconds() -> Result<(), AppError> {
    let s = seed().await?;
    // Give each track a distinct duration in ms. The editor presents Duration
    // in whole seconds ("Duration (sec)"), so a `Duration > 150` rule must
    // compare against 150_000 ms — not the raw 150 ms (which would match all).
    for (id, ms) in [(s.t1, 60_000_i64), (s.t2, 120_000), (s.t3, 240_000), (s.t4, 360_000)] {
        sqlx::query("UPDATE tracks SET duration_ms = ? WHERE id = ?")
            .bind(ms)
            .bind(id)
            .execute(s.db.write())
            .await?;
    }
    let c = one(RuleField::DurationMs, RuleOp::Gt, Some(RuleValue::Number(150.0)));
    // 150 s == 150_000 ms → only t3 (240 s) and t4 (360 s) exceed it.
    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.t3, s.t4]));
    Ok(())
}

/// Stage a title, for the cases whose subject is a `LIKE` metacharacter in one.
async fn set_title(db: &DbPool, id: i64, title: &str) -> Result<(), AppError> {
    sqlx::query("UPDATE tracks SET title = ? WHERE id = ?")
        .bind(title)
        .bind(id)
        .execute(db.write())
        .await?;
    Ok(())
}

/// Stage an album artist, which the scan helper always leaves null.
async fn set_album_artist(db: &DbPool, id: i64, album_artist: &str) -> Result<(), AppError> {
    sqlx::query("UPDATE tracks SET album_artist = ? WHERE id = ?")
        .bind(album_artist)
        .bind(id)
        .execute(db.write())
        .await?;
    Ok(())
}

/// Leave a row with no album at all, which the seed has no other way to produce.
async fn clear_album(db: &DbPool, id: i64) -> Result<(), AppError> {
    sqlx::query("UPDATE tracks SET album = NULL WHERE id = ?").bind(id).execute(db.write()).await?;
    Ok(())
}

// ---- what `push_where`'s parentheses are for ----

/// **Four predicates emit a bare `OR`, and only the wrapping parens keep them one rule.**
/// `NotContains`, `IsNot`, text `IsNotSet` and `NotInLast` all render `col IS NULL OR …`, so
/// under `MatchMode::All` an unwrapped one binds as `IS NULL OR (rest AND next-rule)` — `AND`
/// binding tighter — and every row with a null column joins the playlist whatever the other
/// rules say.
///
/// Every existing test of those forms uses a *single* rule, which is exactly the shape the
/// parens cannot matter in. Needs a null column to see, hence the staging.
///
/// **Over a column-backed field on purpose.** The set-valued ones render `NOT EXISTS`, which
/// carries no bare `OR` and so cannot reach this at all; staged over one of those the test goes
/// green while pinning nothing.
#[tokio::test]
async fn a_null_tolerant_rule_does_not_widen_the_rules_beside_it() -> Result<(), AppError> {
    let s = seed().await?;
    clear_album(&s.db, s.t4).await?;

    let c = SmartCriteria {
        rules: vec![
            Rule {
                field: RuleField::Album,
                op: RuleOp::NotContains,
                value: Some(RuleValue::Text("Album A".to_owned())),
            },
            Rule { field: RuleField::Rating, op: RuleOp::Gte, value: Some(RuleValue::Number(4.0)) },
        ],
        ..SmartCriteria::default()
    };

    // t3 is the only row that is both off Album A and rated 4+. t4 has a null album and rating 0,
    // so it satisfies the first rule and fails the second, then joins the set anyway the moment
    // the parens go.
    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.t3]));
    Ok(())
}

// ---- the LIKE metacharacters, which a user types without meaning them ----

/// An underscore is `LIKE`'s single-character wildcard, so an unescaped one in a rule value
/// quietly matches every neighbouring title too — a filter that looks like it works.
#[tokio::test]
async fn an_underscore_in_a_rule_value_matches_only_an_underscore() -> Result<(), AppError> {
    let s = seed().await?;
    set_title(&s.db, s.t1, "Track_04").await?;
    set_title(&s.db, s.t2, "TrackX04").await?;

    let c = one(RuleField::Title, RuleOp::Contains, Some(RuleValue::Text("Track_0".to_owned())));

    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.t1]));
    Ok(())
}

/// The other metacharacter, and the worse one: an unescaped `%` matches any run of characters,
/// so a rule for a title containing one takes most of the library.
#[tokio::test]
async fn a_percent_in_a_rule_value_matches_only_a_percent() -> Result<(), AppError> {
    let s = seed().await?;
    set_title(&s.db, s.t1, "100% Pure").await?;
    set_title(&s.db, s.t2, "100 Proof").await?;

    let c = one(RuleField::Title, RuleOp::Contains, Some(RuleValue::Text("100%".to_owned())));

    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.t1]));
    Ok(())
}

// ---- the column each field filters on ----

/// **`AlbumArtist` and `Artist` are the swap `field_source` exists to get right**, and the one a
/// reader cannot catch: both compile, both return rows, and the playlist is merely a different
/// set than the user asked for. Ten of the sixteen arms had no case at all; these two are the
/// pair that can be told apart by a query rather than by reading the match.
#[tokio::test]
async fn the_album_artist_rule_reads_its_own_column() -> Result<(), AppError> {
    let s = seed().await?;
    // The scan helper leaves `album_artist` null, so one row carrying a *different* album artist
    // from its own artist is what separates the two columns.
    set_album_artist(&s.db, s.t2, "Artist A").await?;

    let by_artist =
        one(RuleField::Artist, RuleOp::Is, Some(RuleValue::Text("Artist A".to_owned())));
    let by_album_artist =
        one(RuleField::AlbumArtist, RuleOp::Is, Some(RuleValue::Text("Artist A".to_owned())));

    assert_eq!(ids(&resolve(&s.db, &by_artist).await?), HashSet::from([s.t1]));
    assert_eq!(ids(&resolve(&s.db, &by_album_artist).await?), HashSet::from([s.t2]));
    Ok(())
}

// ---- what a null column counts as ----

/// "Is not X" has to include the rows that are nothing at all, or a rule over a column most
/// tracks leave empty answers with almost none of the library. `album_artist` is exactly such a
/// column: the scan writes it only where a file carries one.
#[tokio::test]
async fn a_row_with_no_value_is_not_the_value() -> Result<(), AppError> {
    let s = seed().await?;
    set_album_artist(&s.db, s.t2, "Artist A").await?;

    let c =
        one(RuleField::AlbumArtist, RuleOp::IsNot, Some(RuleValue::Text("Artist A".to_owned())));

    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.t1, s.t3, s.t4]));
    Ok(())
}

/// A text column counts an empty string as unset, which is what a tag editor leaves behind when
/// a field is cleared. Both directions, since the two arms spell the empty-string half
/// separately and losing either one strands those rows outside both answers.
#[tokio::test]
async fn a_blank_text_column_counts_as_unset_in_both_directions() -> Result<(), AppError> {
    let s = seed().await?;
    set_album_artist(&s.db, s.t1, "Artist A").await?;
    set_album_artist(&s.db, s.t2, "").await?;

    let set = one(RuleField::AlbumArtist, RuleOp::IsSet, None);
    let unset = one(RuleField::AlbumArtist, RuleOp::IsNotSet, None);

    assert_eq!(ids(&resolve(&s.db, &set).await?), HashSet::from([s.t1]));
    assert_eq!(ids(&resolve(&s.db, &unset).await?), HashSet::from([s.t2, s.t3, s.t4]));
    Ok(())
}

/// The remaining boolean arm, so the whole match is pinned rather than half of it.
#[tokio::test]
async fn a_favorite_rule_can_ask_for_the_ones_that_are_not() -> Result<(), AppError> {
    let s = seed().await?;
    let c = one(RuleField::Favorite, RuleOp::IsFalse, None);

    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.t2, s.t4]));
    Ok(())
}

// ---- the two edges of a relative-date rule ----

/// The clamp is what stops `TimeDelta::try_days` overflowing, and its failure mode is the
/// opposite of a crash: the `unwrap_or` behind it falls back to a zero delta, so the cutoff
/// becomes *now* and a rule asking for everything ever played answers with nothing.
#[tokio::test]
async fn an_absurd_day_count_still_asks_about_the_whole_past() -> Result<(), AppError> {
    let s = seed().await?;
    let c = one(RuleField::LastPlayed, RuleOp::InLast, Some(RuleValue::Days(i64::MAX)));

    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.t1, s.t2]));
    Ok(())
}

// ---- a rule the editor should not have been able to build ----

/// An operator the field's type does not offer is dropped, **not rendered as always-false**.
/// Under `MatchMode::All` a `0` term would empty the playlist instead, so a criteria file
/// written by an older build — or hand-edited — would silently resolve to nothing rather than
/// to the rules it still understands.
#[tokio::test]
async fn an_incoherent_rule_is_dropped_rather_than_matching_nothing() -> Result<(), AppError> {
    let s = seed().await?;
    let c = SmartCriteria {
        rules: vec![
            Rule {
                field: RuleField::Rating,
                op: RuleOp::Contains,
                value: Some(RuleValue::Text("4".to_owned())),
            },
            Rule {
                field: RuleField::Genre,
                op: RuleOp::Contains,
                value: Some(RuleValue::Text("Rock".to_owned())),
            },
        ],
        ..SmartCriteria::default()
    };

    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.t1, s.t2]));
    Ok(())
}

/// One rule's predicate with no `WHERE` around it. [`super::push_false`] pushes exactly
/// `"0"`, so a degraded rule is an equality rather than a search through real SQL.
fn rendered_predicate(rule: &Rule) -> String {
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new("");
    super::push_rule(&mut qb, rule);
    qb.sql().as_str().to_owned()
}

/// The evaluator's half of the invariant `netodia-core`'s `smart_criteria_tests` holds
/// from the editor's end: a rule the rule-builder can produce has to survive
/// [`super::rule_is_renderable`] and render a real predicate.
///
/// Both halves are needed because the failure is silent in a way that looks like the
/// opposite of a bug. `push_where` drops the rules that fail the gate, and a criteria
/// whose rules are *all* dropped emits no `WHERE` at all, so the smart playlist the user
/// built to hold one artist comes back holding the entire library.
#[test]
fn every_rule_the_editor_can_build_renders_a_real_predicate() {
    for &field in FIELDS {
        let value_type = field.value_type();
        for &op in ops_for(value_type) {
            let rule = Rule { field, op, value: RuleValue::from_input(value_type, op, "5") };
            assert!(
                super::rule_is_renderable(&rule),
                "{field:?} {op:?} is dropped by the gate, and a criteria of only dropped \
                 rules matches the whole library"
            );
            assert_ne!(
                rendered_predicate(&rule),
                "0",
                "{field:?} {op:?} passed the gate and rendered the always-false term"
            );
        }
    }
}

/// The degradation [`super::push_false`] exists for, which is reachable only by handing
/// `push_rule` a rule the gate would have refused. It matches nothing; matching
/// everything would turn an internal inconsistency into a playlist holding the library.
#[test]
fn a_rule_whose_value_shape_is_wrong_renders_as_matching_nothing() {
    let text_field_holding_a_number =
        Rule { field: RuleField::Genre, op: RuleOp::Is, value: Some(RuleValue::Number(5.0)) };
    assert!(
        !super::rule_is_renderable(&text_field_holding_a_number),
        "the gate is what keeps the fallback below unreachable"
    );
    assert_eq!(rendered_predicate(&text_field_holding_a_number), "0");
}

// === The set-valued fields ===

/// A seed the flat helper cannot spell: genres and role credits are rows, and every rule below is
/// about a track carrying more than one of something.
struct MultiSeed {
    db: DbPool,
    /// `Rock` + `Metal`, composer Alice, producer Bob.
    both: i64,
    /// `Rock` alone, composer Carol.
    rock: i64,
    /// `Pop` alone, no role credit at all.
    pop: i64,
}

async fn multi_seed() -> Result<MultiSeed, AppError> {
    let db = DbPool::test_pool().await?;
    queries::folder::insert_folder(&db, "/music", true).await?;

    let mut first = make_test_metadata("One");
    first.genres = GenreList::new(vec!["Rock".to_owned(), "Metal".to_owned()]);
    first.credits = RoleCredits::new(vec![
        role(CreditRole::Composer, "Alice"),
        role(CreditRole::Producer, "Bob"),
    ]);
    let both = insert_tagged_track(&db, "/music/1.mp3", &first).await?;

    let mut second = make_test_metadata("Two");
    second.genres = GenreList::from_name("Rock");
    second.credits = RoleCredits::new(vec![role(CreditRole::Composer, "Carol")]);
    let rock = insert_tagged_track(&db, "/music/2.mp3", &second).await?;

    let mut third = make_test_metadata("Three");
    third.genres = GenreList::from_name("Pop");
    let pop = insert_tagged_track(&db, "/music/3.mp3", &third).await?;

    Ok(MultiSeed { db, both, rock, pop })
}

fn role(role: CreditRole, name: &str) -> RoleCredit {
    RoleCredit { role, name: name.to_owned(), detail: String::new() }
}

/// **The correlation is the whole rule.** These fields render as `EXISTS (SELECT 1 FROM …)`, and a
/// fragment that leaves out the `WHERE track_id = tracks.id` half asks whether the join table
/// holds any row at all — true for every track, so the playlist quietly becomes the library.
#[tokio::test]
async fn a_genre_rule_admits_only_the_tracks_that_carry_it() -> Result<(), AppError> {
    let s = multi_seed().await?;
    let c = one(RuleField::Genre, RuleOp::Is, Some(RuleValue::Text("Pop".to_owned())));

    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.pop]));
    Ok(())
}

#[tokio::test]
async fn a_credits_rule_admits_only_the_tracks_that_carry_it() -> Result<(), AppError> {
    let s = multi_seed().await?;
    let c = one(RuleField::Credits, RuleOp::Is, Some(RuleValue::Text("Carol".to_owned())));

    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.rock]));
    Ok(())
}

/// Against the rendered line `is` is unsatisfiable for a track carrying two of anything: the
/// column reads `Rock, Metal` and no exact match on either name can reach it.
#[tokio::test]
async fn an_exact_rule_reaches_a_track_that_carries_two() -> Result<(), AppError> {
    let s = multi_seed().await?;
    let rock = one(RuleField::Genre, RuleOp::Is, Some(RuleValue::Text("Rock".to_owned())));
    let metal = one(RuleField::Genre, RuleOp::Is, Some(RuleValue::Text("Metal".to_owned())));

    assert_eq!(ids(&resolve(&s.db, &rock).await?), HashSet::from([s.both, s.rock]));
    // The second genre is the one the rendered line buries mid-string.
    assert_eq!(ids(&resolve(&s.db, &metal).await?), HashSet::from([s.both]));
    Ok(())
}

/// `starts_with` against the line only ever reads the first name, so the second is unreachable by
/// prefix however it is spelled.
#[tokio::test]
async fn a_prefix_rule_reaches_a_name_that_is_not_first() -> Result<(), AppError> {
    let s = multi_seed().await?;
    let c = one(RuleField::Genre, RuleOp::StartsWith, Some(RuleValue::Text("Met".to_owned())));

    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.both]));
    Ok(())
}

/// The negated arm renders `NOT EXISTS`, which is true for a track with no rows of that kind at
/// all — the honest reading, since a track credits nobody rather than crediting the empty string.
#[tokio::test]
async fn a_negated_rule_excludes_only_the_carriers_and_keeps_the_untagged() -> Result<(), AppError>
{
    let s = multi_seed().await?;
    let c = one(RuleField::Credits, RuleOp::NotContains, Some(RuleValue::Text("Alice".to_owned())));

    assert_eq!(ids(&resolve(&s.db, &c).await?), HashSet::from([s.rock, s.pop]));
    Ok(())
}

/// A set-valued fragment and a column-backed one have to compose, and `Any` is where an
/// uncorrelated `EXISTS` would be loudest — one arm true for everything widens the whole rule.
#[tokio::test]
async fn a_set_valued_rule_composes_with_a_column_rule() -> Result<(), AppError> {
    let s = multi_seed().await?;

    let all = SmartCriteria {
        rules: vec![
            Rule {
                field: RuleField::Genre,
                op: RuleOp::Is,
                value: Some(RuleValue::Text("Rock".to_owned())),
            },
            Rule {
                field: RuleField::Title,
                op: RuleOp::Is,
                value: Some(RuleValue::Text("One".to_owned())),
            },
        ],
        match_mode: MatchMode::All,
        ..SmartCriteria::default()
    };
    assert_eq!(ids(&resolve(&s.db, &all).await?), HashSet::from([s.both]));

    let any = SmartCriteria { match_mode: MatchMode::Any, ..all };
    assert_eq!(ids(&resolve(&s.db, &any).await?), HashSet::from([s.both, s.rock]));
    Ok(())
}

// --- get_smart_playlist_tracks_for_export ---

async fn exported_paths(db: &DbPool, c: &SmartCriteria) -> Result<Vec<String>, AppError> {
    Ok(queries::smart_playlist::get_smart_playlist_tracks_for_export(db, c)
        .await?
        .into_iter()
        .map(|row| row.file_path)
        .collect())
}

/// A smart playlist exports what its page shows, so the two queries have to agree on membership
/// and on order.
#[tokio::test]
async fn the_export_holds_the_tracks_the_list_shows_in_the_same_order() -> Result<(), AppError> {
    let s = seed().await?;
    let c = one(RuleField::Rating, RuleOp::Gte, Some(RuleValue::Number(2.0)));
    let listed: Vec<String> =
        resolve(&s.db, &c).await?.into_iter().map(|row| row.file_path).collect();
    assert_eq!(listed.len(), 3, "test setup: the rule matches three of the four tracks");

    assert_eq!(exported_paths(&s.db, &c).await?, listed);
    Ok(())
}

#[tokio::test]
async fn the_export_holds_to_a_limit_and_its_order() -> Result<(), AppError> {
    let s = seed().await?;
    let c = SmartCriteria {
        limit: Some(SmartLimit { count: 2, order: LimitOrder::PlayCountDesc }),
        ..SmartCriteria::default()
    };

    // t1 has 10 plays, t3 has 5.
    assert_eq!(exported_paths(&s.db, &c).await?, ["/music/1.mp3", "/music/3.mp3"]);
    Ok(())
}

/// The export names its columns against `tracks` so the correlated `EXISTS` a set-valued rule
/// renders can't make one ambiguous.
#[tokio::test]
async fn the_export_resolves_a_set_valued_rule() -> Result<(), AppError> {
    let s = multi_seed().await?;
    let c = one(RuleField::Genre, RuleOp::Is, Some(RuleValue::Text("Rock".to_owned())));

    assert_eq!(exported_paths(&s.db, &c).await?, ["/music/1.mp3", "/music/2.mp3"]);
    Ok(())
}

#[tokio::test]
async fn an_export_with_no_rules_is_the_whole_library_in_sort_order() -> Result<(), AppError> {
    let s = seed().await?;

    assert_eq!(
        exported_paths(&s.db, &SmartCriteria::default()).await?,
        ["/music/4.mp3", "/music/1.mp3", "/music/3.mp3", "/music/2.mp3"],
        "Four, One, Three, Two"
    );
    Ok(())
}

#[tokio::test]
async fn an_exported_row_carries_what_its_entry_is_written_from() -> Result<(), AppError> {
    let s = seed().await?;
    let c = one(RuleField::Title, RuleOp::Is, Some(RuleValue::Text("One".to_owned())));

    let rows = queries::smart_playlist::get_smart_playlist_tracks_for_export(&s.db, &c).await?;

    let written: Vec<_> = rows
        .into_iter()
        .map(|row| (row.title, row.artist, row.duration_ms, row.file_hash))
        .collect();
    let hash = blake3::hash(b"One").to_hex().to_string();
    assert_eq!(written, [("One".to_owned(), Some("Artist A".to_owned()), 180_000, Some(hash))]);
    Ok(())
}

/// The card menu's batch actions resolve a smart playlist through the id projection rather than
/// the row one, so the two have to answer identically. Order and cap are the halves a narrowed
/// projection can lose in silence, and a different order means the same playlist queues one way
/// from its page and another from the card beside it.
#[tokio::test]
async fn the_id_projection_answers_in_the_row_projection_s_order() -> Result<(), AppError> {
    let s = seed().await?;
    let c = SmartCriteria {
        rules: vec![Rule {
            field: RuleField::PlayCount,
            op: RuleOp::Gt,
            value: Some(RuleValue::Number(0.0)),
        }],
        limit: Some(SmartLimit { count: 2, order: LimitOrder::PlayCountDesc }),
        ..SmartCriteria::default()
    };

    let rows = resolve(&s.db, &c).await?;
    let ids = queries::smart_playlist::get_smart_playlist_track_ids(&s.db, &c).await?;

    assert_eq!(ids, rows.iter().map(|r| r.id).collect::<Vec<i64>>());
    assert_eq!(ids, [s.t1, s.t3], "the cap and the order are both the row query's");
    Ok(())
}

/// A rule set nothing matches is the empty answer rather than the whole library, which is what a
/// dropped `WHERE` would make it.
#[tokio::test]
async fn an_unmatched_rule_set_resolves_to_no_ids() -> Result<(), AppError> {
    let s = seed().await?;
    let c = one(RuleField::Title, RuleOp::Is, Some(RuleValue::Text("Nothing".to_owned())));

    assert!(queries::smart_playlist::get_smart_playlist_track_ids(&s.db, &c).await?.is_empty());
    Ok(())
}
