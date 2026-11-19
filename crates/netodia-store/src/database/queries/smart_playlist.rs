//! Smart-playlist evaluation engine — turns a [`SmartCriteria`] rule set into a
//! parameterized query over the `tracks` table.
//!
//! **Safety contract:** only enum-derived `&'static str` fragments (the column
//! name or correlated rows from [`field_source`], the SQL operator token, the
//! ORDER BY clause) are ever pushed as raw SQL. Every user-supplied value goes through
//! [`sqlx::QueryBuilder::push_bind`] — the same "structure interpolated, data
//! bound" guarantee the rest of the query layer gets from `AssertSqlSafe(sql) +
//! .bind()`. No user string can reach the SQL text.

use jiff::{SignedDuration, Timestamp, tz::Offset};
use sqlx::{QueryBuilder, Sqlite};

use crate::database::DbPool;
use crate::database::queries::track::columns;
use netodia_core::entities::smart_criteria::{
    LimitOrder, MatchMode, Rule, RuleField, RuleOp, RuleValue, SmartCriteria, SmartLimit,
    ValueType, ops_for,
};
use netodia_core::entities::track;
use netodia_core::error::AppError;
use netodia_core::utils::format_rfc3339;

/// Resolve a smart playlist's current membership as list-view rows, in the
/// criteria's order (and capped by its optional limit).
pub async fn get_smart_playlist_tracks(
    db: &DbPool,
    criteria: &SmartCriteria,
) -> Result<Vec<track::TrackListRow>, AppError> {
    let cols = columns::TRACK_LIST.with_album_artwork_fallback(None);
    let mut qb = membership_query(&cols, criteria);
    let rows =
        qb.build_query_as::<track::TrackListRow>().persistent(false).fetch_all(db.read()).await?;
    Ok(rows)
}

/// [`get_smart_playlist_tracks`] as ids alone, for a caller that only needs the membership.
///
/// **Backs the card menu's batch actions**, which resolve a selection to track ids and act on
/// those: going through the list projection there materialises every `TrackListRow` column only
/// to read `id` off it. Same criteria, same order, same cap, so the ids arrive in the order the
/// rows would have.
pub async fn get_smart_playlist_track_ids(
    db: &DbPool,
    criteria: &SmartCriteria,
) -> Result<Vec<i64>, AppError> {
    let mut qb = membership_query("id", criteria);
    let ids = qb.build_query_scalar::<i64>().persistent(false).fetch_all(db.read()).await?;
    Ok(ids)
}

/// [`get_smart_playlist_tracks`] as M3U export rows, in the same order and under the same cap.
pub async fn get_smart_playlist_tracks_for_export(
    db: &DbPool,
    criteria: &SmartCriteria,
) -> Result<Vec<track::PlaylistExportRow>, AppError> {
    let mut qb = membership_query(columns::PLAYLIST_EXPORT.prefixed("tracks"), criteria);
    let rows = qb
        .build_query_as::<track::PlaylistExportRow>()
        .persistent(false)
        .fetch_all(db.read())
        .await?;
    Ok(rows)
}

/// `SELECT columns FROM tracks` over the criteria's membership, ordered and capped.
fn membership_query(columns: &str, criteria: &SmartCriteria) -> QueryBuilder<Sqlite> {
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(format!("SELECT {columns} FROM tracks"));
    push_where(&mut qb, criteria);
    if let Some(limit) = &criteria.limit {
        push_order_and_limit(&mut qb, limit);
    } else {
        // Unlimited: fall back to the natural display order.
        qb.push(order_by_fragment(None));
    }
    qb
}

/// `(track_count, total_duration_ms)` for a smart playlist without projecting
/// any track rows — backs the grid card and the hero's chips, which can't
/// rely on the `playlist_items` triggers (a smart playlist has no junction
/// rows). Respects the limit so a "top 25" playlist reports 25, not the full
/// match count.
pub async fn count_smart_playlist(
    db: &DbPool,
    criteria: &SmartCriteria,
) -> Result<(i64, i64), AppError> {
    let mut qb: QueryBuilder<Sqlite> = if criteria.limit.is_some() {
        QueryBuilder::new(
            "SELECT COUNT(*), COALESCE(SUM(duration_ms), 0) FROM \
             (SELECT duration_ms FROM tracks",
        )
    } else {
        QueryBuilder::new("SELECT COUNT(*), COALESCE(SUM(duration_ms), 0) FROM tracks")
    };
    push_where(&mut qb, criteria);
    if let Some(limit) = &criteria.limit {
        // The count subquery only needs the LIMIT to bound COUNT/SUM to the
        // capped set. A deterministic order still selects *which* `count` rows
        // are summed, so keep it — but `RANDOM()` would force SQLite to
        // materialize + randomly sort the whole match set just to sum a
        // nondeterministic slice (the displayed duration for a random-limited
        // card is already nondeterministic), so skip that pointless full sort.
        if limit.order != LimitOrder::Random {
            qb.push(order_by_fragment(Some(limit)));
        }
        qb.push(" LIMIT ").push_bind(i64::from(limit.count));
        qb.push(")");
    }
    let row = qb.build_query_as::<(i64, i64)>().persistent(false).fetch_one(db.read()).await?;
    Ok(row)
}

/// Append the `WHERE (rule) <AND|OR> (rule) …` clause. Incoherent rules
/// (operator not valid for the field's type, or a missing/wrong-typed value)
/// are silently skipped; an all-skipped rule set yields no `WHERE` (matches the
/// whole library).
fn push_where(qb: &mut QueryBuilder<Sqlite>, criteria: &SmartCriteria) {
    let rules: Vec<&Rule> = criteria.rules.iter().filter(|r| rule_is_renderable(r)).collect();
    if rules.is_empty() {
        return;
    }
    qb.push(" WHERE ");
    let sep = match criteria.match_mode {
        MatchMode::All => " AND ",
        MatchMode::Any => " OR ",
    };
    for (i, rule) in rules.iter().enumerate() {
        if i > 0 {
            qb.push(sep);
        }
        qb.push("(");
        push_rule(qb, rule);
        qb.push(")");
    }
}

/// Whether a rule is coherent enough to render: its operator must be valid for
/// the field's value type, and any value-taking operator must carry a value of
/// the matching kind. Guarantees the value extraction in [`push_rule`] succeeds.
fn rule_is_renderable(rule: &Rule) -> bool {
    if !ops_for(rule.field.value_type()).contains(&rule.op) {
        return false;
    }
    match rule.op {
        RuleOp::IsSet | RuleOp::IsNotSet | RuleOp::IsTrue | RuleOp::IsFalse => true,
        RuleOp::InLast | RuleOp::NotInLast => matches!(rule.value, Some(RuleValue::Days(_))),
        RuleOp::Contains
        | RuleOp::NotContains
        | RuleOp::Is
        | RuleOp::IsNot
        | RuleOp::StartsWith
        | RuleOp::EndsWith => matches!(rule.value, Some(RuleValue::Text(_))),
        RuleOp::Eq | RuleOp::Ne | RuleOp::Gt | RuleOp::Gte | RuleOp::Lt | RuleOp::Lte => {
            matches!(rule.value, Some(RuleValue::Number(_)))
        }
    }
}

/// Push a single rule's predicate. The caller wraps it in `(...)`. Dispatches on
/// the field's value category to a focused per-category helper. Assumes the rule
/// passed [`rule_is_renderable`]; a value-shape mismatch degrades to a benign
/// always-false term via [`push_false`] rather than panicking.
fn push_rule(qb: &mut QueryBuilder<Sqlite>, rule: &Rule) {
    let value = rule.value.as_ref();
    let col = match field_source(rule.field) {
        FieldSource::Members(rows) => return push_member_predicate(qb, rows, rule.op, value),
        FieldSource::Column(col) => col,
    };
    match rule.field.value_type() {
        ValueType::Text => push_text_predicate(qb, col, rule.op, value),
        ValueType::Number => push_numeric_predicate(qb, col, rule.field, rule.op, value),
        ValueType::Bool => push_bool_predicate(qb, col, rule.op),
        ValueType::Date => push_date_predicate(qb, col, rule.op, value),
    }
}

/// Run a text predicate against a set-valued field's *rows*, as `(NOT) EXISTS` over the join table
/// correlated on `tracks.id`.
///
/// The negation is spelled once here rather than per operator, so the inner predicate is the same
/// [`push_text_predicate`] every column field runs and a set matches when *any* member does.
fn push_member_predicate(
    qb: &mut QueryBuilder<Sqlite>,
    rows: MemberRows,
    op: RuleOp,
    value: Option<&RuleValue>,
) {
    let (negated, op) = match op {
        RuleOp::NotContains => (true, RuleOp::Contains),
        RuleOp::IsNot => (true, RuleOp::Is),
        RuleOp::IsNotSet => (true, RuleOp::IsSet),
        op => (false, op),
    };
    // Ahead of the wrapper, not inside it: a value-shape mismatch owes an always-false term, and
    // one placed under `NOT EXISTS` is an always-true one.
    if op != RuleOp::IsSet && !matches!(value, Some(RuleValue::Text(_))) {
        return push_false(qb);
    }

    if negated {
        qb.push("NOT ");
    }
    qb.push("EXISTS (SELECT 1 FROM ");
    qb.push(rows.from);
    qb.push(" WHERE ");
    qb.push(rows.correlate);
    // `IsSet` is the row existing, so the correlation already is the whole predicate.
    if op != RuleOp::IsSet {
        // Parenthesized: two of `push_text_predicate`'s arms spell a bare `OR`, and one reaching
        // here would bind the correlation to its left half and leave the `EXISTS` uncorrelated.
        qb.push(" AND (");
        push_text_predicate(qb, rows.name, op, value);
        qb.push(")");
    }
    qb.push(")");
}

/// A benign always-false term (`WHERE (… 0 …)` matches no row). Reached only if
/// a rule that should have passed [`rule_is_renderable`] arrives with the wrong
/// value shape — an internal-consistency break between the renderability gate
/// and value extraction. Degrades to matching nothing rather than panicking on
/// a user's library.
fn push_false(qb: &mut QueryBuilder<Sqlite>) {
    qb.push("0");
}

/// `IS (NOT) NULL` presence test shared by every value category. Text columns
/// also treat an empty string as "not set".
fn push_presence(qb: &mut QueryBuilder<Sqlite>, col: &str, op: RuleOp, is_text: bool) {
    match op {
        RuleOp::IsSet => {
            qb.push(col).push(" IS NOT NULL");
            if is_text {
                qb.push(" AND ").push(col).push(" != ''");
            }
        }
        RuleOp::IsNotSet => {
            qb.push(col).push(" IS NULL");
            if is_text {
                qb.push(" OR ").push(col).push(" = ''");
            }
        }
        _ => push_false(qb),
    }
}

/// Text-column predicates: substring/prefix/suffix `LIKE`, case-insensitive
/// equality, and presence.
fn push_text_predicate(
    qb: &mut QueryBuilder<Sqlite>,
    col: &str,
    op: RuleOp,
    value: Option<&RuleValue>,
) {
    match op {
        RuleOp::IsSet | RuleOp::IsNotSet => push_presence(qb, col, op, true),
        RuleOp::Contains | RuleOp::NotContains | RuleOp::StartsWith | RuleOp::EndsWith => {
            let Some(RuleValue::Text(s)) = value else {
                return push_false(qb);
            };
            let esc = escape_like(s);
            let pattern = match op {
                RuleOp::StartsWith => format!("{esc}%"),
                RuleOp::EndsWith => format!("%{esc}"),
                _ => format!("%{esc}%"),
            };
            if op == RuleOp::NotContains {
                qb.push(col).push(" IS NULL OR ").push(col).push(" NOT LIKE ");
            } else {
                qb.push(col).push(" LIKE ");
            }
            qb.push_bind(pattern).push(" ESCAPE '\\'");
        }
        RuleOp::Is => {
            let Some(RuleValue::Text(s)) = value else {
                return push_false(qb);
            };
            qb.push(col).push(" = ").push_bind(s.clone()).push(" COLLATE NOCASE");
        }
        RuleOp::IsNot => {
            let Some(RuleValue::Text(s)) = value else {
                return push_false(qb);
            };
            qb.push(col)
                .push(" IS NULL OR ")
                .push(col)
                .push(" <> ")
                .push_bind(s.clone())
                .push(" COLLATE NOCASE");
        }
        _ => push_false(qb),
    }
}

/// Numeric-column predicates: the six comparisons and presence. Owns the
/// Duration seconds→milliseconds scaling.
fn push_numeric_predicate(
    qb: &mut QueryBuilder<Sqlite>,
    col: &str,
    field: RuleField,
    op: RuleOp,
    value: Option<&RuleValue>,
) {
    match op {
        RuleOp::IsSet | RuleOp::IsNotSet => push_presence(qb, col, op, false),
        RuleOp::Eq | RuleOp::Ne | RuleOp::Gt | RuleOp::Gte | RuleOp::Lt | RuleOp::Lte => {
            let Some(RuleValue::Number(n)) = value else {
                return push_false(qb);
            };
            let sql_op = match op {
                RuleOp::Eq => " = ",
                RuleOp::Ne => " <> ",
                RuleOp::Gt => " > ",
                RuleOp::Gte => " >= ",
                RuleOp::Lt => " < ",
                _ => " <= ",
            };
            // The editor presents Duration in whole seconds ("Duration (sec)"),
            // but the column stores milliseconds — scale the bound value so the
            // comparison is against the same unit the user typed.
            let bound = if field == RuleField::DurationMs { *n * 1000.0 } else { *n };
            qb.push(col).push(sql_op).push_bind(bound);
        }
        _ => push_false(qb),
    }
}

/// Boolean-column predicates (`is_favorite = 1/0`).
fn push_bool_predicate(qb: &mut QueryBuilder<Sqlite>, col: &str, op: RuleOp) {
    match op {
        RuleOp::IsTrue => {
            qb.push(col).push(" = 1");
        }
        RuleOp::IsFalse => {
            qb.push(col).push(" = 0");
        }
        _ => push_false(qb),
    }
}

/// Date-column predicates: the relative-date window tests and presence.
fn push_date_predicate(
    qb: &mut QueryBuilder<Sqlite>,
    col: &str,
    op: RuleOp,
    value: Option<&RuleValue>,
) {
    match op {
        RuleOp::IsSet | RuleOp::IsNotSet => push_presence(qb, col, op, false),
        RuleOp::InLast | RuleOp::NotInLast => {
            let Some(RuleValue::Days(days)) = value else {
                return push_false(qb);
            };
            let cutoff = cutoff_rfc3339(*days);
            if op == RuleOp::InLast {
                qb.push(col).push(" IS NOT NULL AND ").push(col).push(" >= ");
            } else {
                qb.push(col).push(" IS NULL OR ").push(col).push(" < ");
            }
            qb.push_bind(cutoff);
        }
        _ => push_false(qb),
    }
}

/// Where a [`RuleField`]'s values live.
///
/// One answer per field, so nothing can compare a genre against the rendered line in one place and
/// against its rows in another.
enum FieldSource {
    /// A column on `tracks` holding the whole value.
    Column(&'static str),
    /// Rows in a join table, reached through a correlated `EXISTS`.
    Members(MemberRows),
}

/// The three fragments a correlated `EXISTS` over a join table needs. Split rather than one string
/// so the correlation can't be left out of a fourth set of rows and go unnoticed — omitted, the
/// subquery matches the whole library instead of failing.
#[derive(Clone, Copy)]
struct MemberRows {
    /// The `FROM` text: the join table and whatever it reaches for a name.
    from: &'static str,
    /// The term tying one of its rows to the track under test.
    correlate: &'static str,
    /// The name column a predicate compares.
    name: &'static str,
}

/// The column or rows each [`RuleField`] filters on. Exhaustive `match` over an enum, and every
/// fragment it hands back is a `&'static str`, so no user string reaches SQL.
///
/// **Genres and role credits are rows.** The `tracks.genre` / `.credits` columns beside them are
/// the rendered line `tracks_fts` and the display surfaces read, and a rule run against that line
/// leaves `is` unsatisfiable for a track carrying two of anything, `starts_with` reading only the
/// one the line opens with, and `contains` matching across the seam between two of them.
///
/// **`Artist` is deliberately a column.** `tracks.artist` is the credit as the release printed it,
/// which is the string on screen and so the string a rule is written against; the genre line is a
/// join nothing prints.
fn field_source(field: RuleField) -> FieldSource {
    match field {
        RuleField::Genre => FieldSource::Members(MemberRows {
            from: "track_genres tg JOIN genres g ON g.id = tg.genre_id",
            correlate: "tg.track_id = tracks.id",
            name: "g.name",
        }),
        RuleField::Credits => FieldSource::Members(MemberRows {
            from: "track_credits tc JOIN artists a ON a.id = tc.artist_id",
            correlate: "tc.track_id = tracks.id",
            name: "a.name",
        }),
        RuleField::Title => FieldSource::Column("title"),
        RuleField::Artist => FieldSource::Column("artist"),
        RuleField::AlbumArtist => FieldSource::Column("album_artist"),
        RuleField::Album => FieldSource::Column("album"),
        RuleField::Mood => FieldSource::Column("mood"),
        RuleField::InitialKey => FieldSource::Column("initial_key"),
        RuleField::Isrc => FieldSource::Column("isrc"),
        RuleField::Year => FieldSource::Column("year"),
        RuleField::Bpm => FieldSource::Column("bpm"),
        RuleField::DurationMs => FieldSource::Column("duration_ms"),
        RuleField::PlayCount => FieldSource::Column("play_count"),
        RuleField::SkipCount => FieldSource::Column("skip_count"),
        RuleField::Rating => FieldSource::Column("rating"),
        RuleField::Bitrate => FieldSource::Column("bitrate"),
        RuleField::SampleRate => FieldSource::Column("sample_rate"),
        RuleField::FileSize => FieldSource::Column("file_size"),
        RuleField::Favorite => FieldSource::Column("is_favorite"),
        RuleField::LastPlayed => FieldSource::Column("last_played"),
        RuleField::DateAdded => FieldSource::Column("date_added"),
    }
}

/// Append `ORDER BY … LIMIT ?` for a capped smart playlist. Shared by the row
/// query and the count subquery (which then closes its own `)`); an *unlimited*
/// query handles ordering itself — the row query needs a display order, the
/// count query needs none.
fn push_order_and_limit(qb: &mut QueryBuilder<Sqlite>, limit: &SmartLimit) {
    qb.push(order_by_fragment(Some(limit)));
    qb.push(" LIMIT ").push_bind(i64::from(limit.count));
}

/// The `ORDER BY` clause (leading space included) for a resolved smart playlist.
/// A `&'static str` from an enum match — the selection order can't inject SQL.
/// Lexical `TEXT` order over RFC-3339 `last_played`/`date_added` == chronological
/// order (same invariant `get_recently_played` documents).
fn order_by_fragment(limit: Option<&SmartLimit>) -> &'static str {
    match limit.map(|l| l.order) {
        Some(LimitOrder::DateAddedDesc) => " ORDER BY date_added DESC",
        Some(LimitOrder::DateAddedAsc) => " ORDER BY date_added ASC",
        Some(LimitOrder::PlayCountDesc) => " ORDER BY play_count DESC",
        Some(LimitOrder::PlayCountAsc) => " ORDER BY play_count ASC",
        Some(LimitOrder::LastPlayedDesc) => " ORDER BY last_played DESC",
        Some(LimitOrder::LastPlayedAsc) => " ORDER BY last_played ASC",
        Some(LimitOrder::RatingDesc) => " ORDER BY rating DESC",
        Some(LimitOrder::Random) => " ORDER BY RANDOM()",
        None => " ORDER BY sort_key COLLATE NOCASE ASC",
    }
}

/// Escape a user substring for a `LIKE ? ESCAPE '\'` pattern — the wildcard
/// metacharacters `% _ \` are prefixed with a backslash.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Upper bound on a relative-date rule's day count (~9,863 years).
const MAX_RELATIVE_DAYS: i64 = 3_600_000;

/// The RFC-3339 cutoff `now - days` for the relative-date operators. `days` is
/// clamped to [`MAX_RELATIVE_DAYS`] before converting it to seconds.
fn cutoff_rfc3339(days: i64) -> String {
    let days = days.clamp(0, MAX_RELATIVE_DAYS);
    let delta = SignedDuration::from_secs(days * 86_400);
    let cutoff = Timestamp::now().checked_sub(delta).unwrap_or(Timestamp::MIN);
    format_rfc3339(cutoff, Offset::UTC)
}

#[cfg(test)]
#[path = "tests/smart_playlist_tests.rs"]
mod tests;
