//! The `TrackListRow` fetches every track list renders from.

use sqlx::AssertSqlSafe;

use super::columns;
use crate::database::DbPool;
use netodia_core::entities::track;
use netodia_core::error::AppError;

/// The `ORDER BY` body every track fetch with no order of its own shares: the natural-ordering key
/// built at scan time from title/artist/album.
///
/// **Fixed**, because every view that reads one re-sorts what it gets, over rows it retains
/// (`ui::track_list_cache`) or through `ui::track_sort` directly, so a per-column order here would
/// be a second spelling of sort semantics that nothing consults.
///
/// **Which makes the clause dead on a detail page, and copying it into a batch-action query the
/// trap.** `queries::track::entity_ids` decides the order a card's right-click Play queues in and
/// has no re-sort behind it, so it owes agreement to `ui::track_sort`'s default for that page and
/// not to this: Artist and Genre Detail default to `"album"`, and `sort_key` taken from here
/// queues a catalogue by title under a page that shows it by album.
///
/// What the clause is still for is **determinism**: the in-memory comparator appends `sort_key` as
/// its tie-breaker and `sort_by_cached_key` is stable, so rows tied on it fall back to the order
/// `SQLite` handed over. Dropping the clause would leave that to the query plan — and it is close
/// to free, `idx_tracks_sort_key` being the same expression under the same collation, so this
/// reads as an index scan rather than a sort.
pub(super) const TRACK_LIST_ORDER: &str = "sort_key COLLATE NOCASE ASC";

/// Lightweight version of `get_all_tracks` returning only list-view columns.
///
/// The display order is the caller's: this hands back [`TRACK_LIST_ORDER`] and
/// `ui::track_list_cache` permutes it.
pub async fn get_all_tracks_for_list(db: &DbPool) -> Result<Vec<track::TrackListRow>, AppError> {
    let cols = columns::TRACK_LIST.with_album_artwork_fallback(None);
    let sql = format!("SELECT {cols} FROM tracks ORDER BY {TRACK_LIST_ORDER}");
    let tracks =
        sqlx::query_as::<_, track::TrackListRow>(AssertSqlSafe(sql)).fetch_all(db.read()).await?;
    Ok(tracks)
}

/// Lightweight version of `get_tracks_by_album` for list views.
pub async fn get_tracks_by_album_for_list(
    db: &DbPool,
    album_id: i64,
) -> Result<Vec<track::TrackListRow>, AppError> {
    let cols = columns::TRACK_LIST.with_album_artwork_fallback(None);
    let tracks = sqlx::query_as::<_, track::TrackListRow>(AssertSqlSafe(format!(
        "SELECT {cols} FROM tracks WHERE album_id = ? ORDER BY disc_number ASC, track_number ASC"
    )))
    .bind(album_id)
    .fetch_all(db.read())
    .await?;
    Ok(tracks)
}

/// Lightweight version of `get_tracks_by_artist` for list views.
///
/// Every track this artist is *credited* on, which is the join table rather than the
/// `tracks.artist_id` FK — that one names only the first credit. `IN` rather than a join for two
/// reasons: `artist_id` is a column of both tables, and a credit that names one artist twice
/// ("X feat. X") would join the row in twice.
pub async fn get_tracks_by_artist_for_list(
    db: &DbPool,
    artist_id: i64,
) -> Result<Vec<track::TrackListRow>, AppError> {
    let cols = columns::TRACK_LIST.with_album_artwork_fallback(None);
    let tracks = sqlx::query_as::<_, track::TrackListRow>(AssertSqlSafe(format!(
        "SELECT {cols} FROM tracks \
         WHERE id IN (SELECT track_id FROM track_artists WHERE artist_id = ?) \
         ORDER BY {TRACK_LIST_ORDER}"
    )))
    .bind(artist_id)
    .fetch_all(db.read())
    .await?;
    Ok(tracks)
}

/// Lightweight version of `get_tracks_by_genre` for list views.
///
/// Every track tagged with this genre, through the join table for
/// [`get_tracks_by_artist_for_list`]'s reason. What makes it load-bearing here rather than merely
/// consistent: `genres.track_count` is maintained off `track_genres`, so reading `tracks.genre_id`
/// left the grid card stating a count over a page that listed a subset of it, and nothing at all
/// for a genre no track happens to carry first.
pub async fn get_tracks_by_genre_for_list(
    db: &DbPool,
    genre_id: i64,
) -> Result<Vec<track::TrackListRow>, AppError> {
    let cols = columns::TRACK_LIST.with_album_artwork_fallback(None);
    let tracks = sqlx::query_as::<_, track::TrackListRow>(AssertSqlSafe(format!(
        "SELECT {cols} FROM tracks \
         WHERE id IN (SELECT track_id FROM track_genres WHERE genre_id = ?) \
         ORDER BY {TRACK_LIST_ORDER}"
    )))
    .bind(genre_id)
    .fetch_all(db.read())
    .await?;
    Ok(tracks)
}

/// Fetch all favorite tracks (lightweight list-view columns).
///
/// Ordered like [`get_all_tracks_for_list`] and for the same reason — the Songs tab's own sort is
/// resolved over the retained rows, not here.
pub async fn get_favorite_tracks_for_list(
    db: &DbPool,
) -> Result<Vec<track::TrackListRow>, AppError> {
    let cols = columns::TRACK_LIST.with_album_artwork_fallback(None);
    let sql =
        format!("SELECT {cols} FROM tracks WHERE is_favorite = TRUE ORDER BY {TRACK_LIST_ORDER}");
    let tracks =
        sqlx::query_as::<_, track::TrackListRow>(AssertSqlSafe(sql)).fetch_all(db.read()).await?;
    Ok(tracks)
}

/// The native path separator as it must appear *inside* a LIKE pattern. `\` is the ESCAPE
/// character, so on Windows a literal backslash is doubled.
#[cfg(windows)]
pub(super) const LIKE_SEPARATOR: &str = "\\\\";
#[cfg(not(windows))]
pub(super) const LIKE_SEPARATOR: &str = "/";

/// `dir_path` escaped for `LIKE … ESCAPE '\'` and terminated with [`LIKE_SEPARATOR`]: the prefix
/// every "files under this directory" pattern is built from.
///
/// One home for the escaping, shared with [`super::entity_ids::track_ids_under_directory`]. A
/// folder is named by the user, and `_` is `LIKE`'s single-character wildcard.
pub(super) fn directory_like_prefix(dir_path: &str) -> String {
    let escaped = dir_path.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    format!("{escaped}{LIKE_SEPARATOR}")
}

/// Fetch tracks whose files live directly inside `dir_path`, not in subdirectories. Returns the
/// `TrackListRow` projection — Browse renders these through the shared `TrackList`, so it needs
/// the same column set the Tracks view uses rather than a narrower browse-only slice.
///
/// **Platform invariant**: `tracks.file_path` is stored verbatim from `Path::to_string_lossy()` at
/// scan time, so it carries the platform's native separator and the LIKE pattern below matches
/// that. Pass `dir_path` in native form too — callers route through
/// `netodia_core::utils::canonicalize_path`.
///
/// Query-plan note: `file_path` has a UNIQUE auto-index, but the `ESCAPE` clause disables
/// `SQLite`'s prefix-`LIKE`→range-scan optimisation entirely. At realistic library sizes a one-off
/// index scan per *navigation* isn't worth a generated `parent_dir` column and index, which would
/// slow every insert. Revisit only if a profiler flags it at a much larger library size.
pub async fn get_tracks_in_directory(
    db: &DbPool,
    dir_path: &str,
) -> Result<Vec<track::TrackListRow>, AppError> {
    let prefix = directory_like_prefix(dir_path);
    let pattern = format!("{prefix}%");
    let subdir_pattern = format!("{prefix}%{LIKE_SEPARATOR}%");

    let cols = columns::TRACK_LIST.with_album_artwork_fallback(None);
    let sql = format!(
        "SELECT {cols} FROM tracks WHERE file_path LIKE ? ESCAPE '\\' AND file_path NOT LIKE ? ESCAPE '\\'"
    );
    let tracks = sqlx::query_as::<_, track::TrackListRow>(AssertSqlSafe(sql))
        .bind(&pattern)
        .bind(&subdir_pattern)
        .fetch_all(db.read())
        .await?;

    Ok(tracks)
}

/// The N most-recently-played tracks (list-view projection), newest first.
///
/// Ordering rides on `last_played` (RFC-3339 UTC, written by the play-count flusher /
/// `update_play_count`), whose lexical order is chronological even though the RFC 3339 formatter
/// fractional seconds vary in width: the offset's `'+'` sorts below every digit, and `AutoSi` only
/// drops trailing-zero groups. Rows without a `last_played` (never played) are excluded —
/// index-backed by the partial `idx_tracks_last_played`.
///
/// Returns the `TrackListRow` projection so the Recently-Played view can render
/// through the shared `TrackList`; `last_played` itself is not selected (it is
/// only the sort key). Membership is the top `limit` by recency — the view
/// re-sorts/filters this set in memory, it never re-queries.
pub async fn get_recently_played(
    db: &DbPool,
    limit: i64,
) -> Result<Vec<track::TrackListRow>, AppError> {
    let cols = columns::TRACK_LIST.with_album_artwork_fallback(None);
    let sql = format!(
        "SELECT {cols} FROM tracks \
         WHERE last_played IS NOT NULL \
         ORDER BY last_played DESC \
         LIMIT ?"
    );
    let rows = sqlx::query_as::<_, track::TrackListRow>(AssertSqlSafe(sql))
        .bind(limit)
        .fetch_all(db.read())
        .await?;
    Ok(rows)
}
