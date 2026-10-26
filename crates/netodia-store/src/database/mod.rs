mod backup;
pub mod queries;

use sqlx::AssertSqlSafe;
use sqlx::migrate::Migrate;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;
use std::time::Duration;

use netodia_core::config::Paths;
use netodia_core::error::{AppError, describe};

/// Bind-variable budget for one statement, divided by a query's columns-per-row to size its chunks.
///
/// A bound we choose, not one the engine gives us: the bundled `SQLite` has allowed 32766 bind
/// variables since 3.32, and 999 is the older default this value kept. The arithmetic settles
/// nothing, because moving it trades in both directions: a larger chunk buys fewer prepares,
/// every bulk site running `.persistent(false)` and so re-preparing per chunk, and costs a
/// proportionally larger parameter array and bind set resident per statement. Move it against
/// a profile of a first scan on a large library.
pub const MAX_BINDS_PER_STATEMENT: usize = 999;

/// sqlx's default migrations-tracking table; the 0.9 `Migrate` trait methods
/// take it explicitly (it became configurable via `sqlx.toml`).
const MIGRATIONS_TABLE: &str = "_sqlx_migrations";

/// SQLite's page cache is per connection, not per database. The old 16,000 KiB
/// allowance could retain almost 80 MiB across the writer and four readers.
/// Keep the pool's combined page-cache target at 4 MiB; reads also share up to
/// 8 MiB of file-backed mappings. These are cache targets, not a total-memory cap:
/// SQLite can exceed them temporarily while a transaction pins dirty pages.
const WRITE_CACHE_KIB: &str = "-2048";
const READ_CACHE_KIB: &str = "-1024";
const READ_CONNECTIONS: u32 = 2;
const READ_MMAP_BYTES: &str = "8388608";
const STATEMENT_CACHE_CAPACITY: usize = 32;

/// The fts5 index-compaction command [`DbPool::close`] issues at shutdown.
/// Named so the test that proves it still runs binds to the same string —
/// fts5 rejects an unknown command at *step* time, and `close` can only
/// afford to log that.
///
/// The full collapse to a single segment, not the bounded `'merge'` fts5
/// documents as the incremental alternative. `automerge` already folds segments
/// as writes accumulate, so what reaches shutdown is the tail: collapsing it
/// leaves the next call a no-op, where a page budget only spreads the same work
/// across more shutdowns. An unfinished one costs nothing but the tidying, the
/// force-exit rolling it back, and the expensive case is the session that
/// scanned a library in, which is also the one with the most segments to fold.
const FTS_OPTIMIZE: &str = "INSERT INTO tracks_fts(tracks_fts) VALUES('optimize')";

/// Build a `?, ?, …` placeholder list for an `IN (...)` clause. Single-pass and
/// capacity-preallocated, where a `repeat_n(…).join(", ")` allocates an
/// intermediate `Vec<&str>` of size `n` first.
pub(crate) fn placeholders(n: usize) -> String {
    let mut s = String::with_capacity(n * 3);
    for i in 0..n {
        if i > 0 {
            s.push_str(", ");
        }
        s.push('?');
    }
    s
}

/// What one row spends in an `UPDATE … SET col = CASE id WHEN ? THEN ? … END WHERE id IN (…)`: the
/// `WHEN ? THEN ?` pair, then its id again in the `IN` list.
pub(crate) const CASE_BINDS_PER_ROW: usize = 3;

/// `CASE id WHEN ? THEN ? … END` over `rows` pairs, each bound id first.
pub(crate) fn case_by_id(rows: usize) -> String {
    const WHEN_THEN: &str = " WHEN ? THEN ?";
    let mut s = String::with_capacity("CASE id END".len() + rows * WHEN_THEN.len());
    s.push_str("CASE id");
    for _ in 0..rows {
        s.push_str(WHEN_THEN);
    }
    s.push_str(" END");
    s
}

/// Execute a chunked single-column IN-clause query inside [`MAX_BINDS_PER_STATEMENT`],
/// concatenating the results.
///
/// Each item binds exactly one placeholder. **Not** for a tuple-IN clause — the
/// chunk size assumes one bind per item and would bust the cap.
pub(crate) async fn chunked_in_query<T, B>(
    pool: &SqlitePool,
    items: &[B],
    build_sql: impl Fn(&str) -> String,
) -> Result<Vec<T>, AppError>
where
    T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> + Send + Unpin,
    for<'q> B: sqlx::Encode<'q, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite> + Send + Sync,
{
    if items.is_empty() {
        return Ok(Vec::new());
    }

    let mut all_results: Vec<T> = Vec::new();

    for chunk in items.chunks(MAX_BINDS_PER_STATEMENT) {
        let sql = build_sql(&placeholders(chunk.len()));

        let mut query = sqlx::query_as::<_, T>(AssertSqlSafe(sql));
        for item in chunk {
            query = query.bind(item);
        }

        let rows = query.persistent(false).fetch_all(pool).await?;
        all_results.extend(rows);
    }

    Ok(all_results)
}

#[derive(Clone)]
pub struct DbPool {
    read: SqlitePool,
    write: SqlitePool,
}

impl DbPool {
    pub fn read(&self) -> &SqlitePool {
        &self.read
    }

    pub fn write(&self) -> &SqlitePool {
        &self.write
    }

    /// Refresh planner statistics, compact the search index, and close both
    /// pools. Call on shutdown.
    ///
    /// The two optimizes are unrelated despite the shared name: `PRAGMA
    /// optimize` updates `sqlite_stat1`, while the fts5 command collapses the
    /// index's segments and drops the tombstone every track delete leaves.
    /// fts5's `automerge` already folds segments as writes accumulate, so this
    /// is the full collapse that never gets to — buying a smaller index for
    /// every pre-migration `VACUUM INTO` to copy, and skipped outright once the
    /// index is one segment.
    ///
    /// All four steps share the budget `flush_tasks_and_db` gives this call, and
    /// the merge is the one that can grow — a session that just scanned a
    /// library in leaves the most segments to fold. Nothing is lost if it
    /// doesn't finish: `SQLite` rolls an unfinished merge back on the next open
    /// and the index is correct, merely un-compacted.
    ///
    /// Both are best-effort, and both **log** rather than discard: an
    /// unrecognised fts5 command is a step-time error with no other symptom, so
    /// a silent discard would make a typo here indistinguishable from a shutdown
    /// that did the work.
    pub async fn close(&self) {
        if let Err(e) = sqlx::query("PRAGMA optimize").execute(&self.write).await {
            log::warn!("db close: PRAGMA optimize: {}", describe(&e));
        }
        if let Err(e) = sqlx::query(FTS_OPTIMIZE).execute(&self.write).await {
            log::warn!("db close: fts5 optimize: {}", describe(&e));
        }
        self.write.close().await;
        self.read.close().await;
    }
}

/// One-shot normalization of Windows verbatim path prefixes left by older
/// versions that called `std::fs::canonicalize` directly. Runs right after
/// migrations so the rest of init sees the canonical form, in one transaction,
/// and idempotently — a second boot matches zero rows.
///
/// Two forms: `\\?\<drive>:\…` drops its 4-char prefix, `\\?\UNC\server\share\…`
/// swaps its 8-char one for `\\`. Both gate on the *post-strip* length fitting
/// in `MAX_PATH`, since a path genuinely exceeding it keeps the prefix under
/// `dunce::canonicalize` too.
///
/// Not `#[cfg(windows)]`, so it stays compiled and unit-testable on every CI
/// runner; the *call site* is guarded by a runtime `cfg!` instead. That is what
/// keeps two full-table `tracks` LIKE scans — the patterns can't match
/// off-Windows, and the default collation can't use the `file_path` index — out
/// of every Linux and macOS boot.
async fn strip_windows_verbatim_paths(pool: &SqlitePool) -> Result<(), AppError> {
    // `_` matches the drive letter and the literal `:` after it is what stops
    // this pattern reaching a UNC path, which has `N` at that position. The
    // length bound is the `MAX_PATH` cap plus the prefix this strips.
    const FOLDERS_DRIVE: &str = "\
        UPDATE folders SET path = SUBSTR(path, 5) \
        WHERE path LIKE '\\\\?\\_:\\%' AND LENGTH(path) < 264";
    const TRACKS_DRIVE: &str = "\
        UPDATE tracks SET file_path = SUBSTR(file_path, 5) \
        WHERE file_path LIKE '\\\\?\\_:\\%' AND LENGTH(file_path) < 264";
    // The prefix is 8 chars and the replacement 2, so the bound is the cap plus
    // the net 6 this removes.
    const FOLDERS_UNC: &str = "\
        UPDATE folders SET path = '\\\\' || SUBSTR(path, 9) \
        WHERE path LIKE '\\\\?\\UNC\\%' AND LENGTH(path) < 266";
    const TRACKS_UNC: &str = "\
        UPDATE tracks SET file_path = '\\\\' || SUBSTR(file_path, 9) \
        WHERE file_path LIKE '\\\\?\\UNC\\%' AND LENGTH(file_path) < 266";

    let mut tx = pool.begin().await?;
    let folders_drive = sqlx::query(FOLDERS_DRIVE).execute(&mut *tx).await?.rows_affected();
    let folders_unc = sqlx::query(FOLDERS_UNC).execute(&mut *tx).await?.rows_affected();
    let tracks_drive = sqlx::query(TRACKS_DRIVE).execute(&mut *tx).await?.rows_affected();
    let tracks_unc = sqlx::query(TRACKS_UNC).execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;

    let folders = folders_drive + folders_unc;
    let tracks = tracks_drive + tracks_unc;
    if folders > 0 || tracks > 0 {
        log::info!(
            "Normalized Windows verbatim paths: {folders} folder row(s) ({folders_drive} drive, {folders_unc} UNC), {tracks} track row(s) ({tracks_drive} drive, {tracks_unc} UNC)"
        );
    }
    Ok(())
}

pub async fn init_database(paths: &Paths) -> Result<DbPool, AppError> {
    let db_path = paths.db_path.clone();
    let db_url = format!("sqlite:{}", db_path.display());

    log::info!("Database path: {}", db_path.display());

    // Asked before the write pool opens, because `create_if_missing` below turns it true right
    // there. Inside the backup it could only ever read `true`, and a first launch would snapshot
    // an empty schema and keep the file forever.
    let db_existed = db_path.exists();

    // One connection, so writes serialize; `busy_timeout` lets a writer wait out
    // the WAL checkpointer instead of returning `SQLITE_BUSY` at once.
    //
    // `.claude/rules/sqlx.md` prefers `BEGIN IMMEDIATE` for write transactions,
    // to fail fast on contention. There is none to fail fast on here: one write
    // connection, WAL readers that don't block writers, and no second process
    // opening the file. DEFERRED stays — revisit if a sidecar tool or a
    // multi-window mode ever adds a second writer.
    let write_opts = SqliteConnectOptions::from_str(&db_url)?
        .journal_mode(SqliteJournalMode::Wal)
        .create_if_missing(true)
        .busy_timeout(Duration::from_secs(5))
        .pragma("foreign_keys", "ON")
        .pragma("synchronous", "NORMAL")
        .statement_cache_capacity(STATEMENT_CACHE_CAPACITY)
        .pragma("cache_size", WRITE_CACHE_KIB)
        // Let large sorts spill to a temporary file instead of growing an
        // unbounded in-memory temporary database alongside the page cache.
        .pragma("temp_store", "FILE");

    let write_pool = SqlitePoolOptions::new().max_connections(1).connect_with(write_opts).await?;

    let migrator = sqlx::migrate!("../../migrations");
    let (has_pending, applied_version) = {
        let mut conn = write_pool.acquire().await?;
        conn.ensure_migrations_table(MIGRATIONS_TABLE).await?;
        let applied: std::collections::HashSet<i64> = conn
            .list_applied_migrations(MIGRATIONS_TABLE)
            .await?
            .into_iter()
            .map(|m| m.version)
            .collect();

        let pending = migrator
            .iter()
            .filter(|m| !m.migration_type.is_down_migration())
            .any(|m| !applied.contains(&m.version));

        // The scope releases the single write connection.
        (pending, applied.iter().max().copied().unwrap_or(0))
    };

    // Fatal on failure: a migration that runs without a recovery point is worse
    // than a boot that stops and says why.
    let backup_path = if has_pending && db_existed {
        Some(backup::create(&write_pool, &paths.backups_dir, applied_version).await?)
    } else {
        None
    };
    // Unconditional, and after the backup: a launch with nothing pending is the
    // common one, and it is where loose files from older versions get adopted.
    backup::maintain(&paths.data_dir, &paths.backups_dir);

    if let Err(e) = migrator.run(&write_pool).await {
        if let Some(path) = &backup_path {
            log::error!(
                "Migration failed — the database as it stood before is at {}",
                path.display()
            );
        }
        return Err(e.into());
    }

    // New writes route through `netodia_core::utils::canonicalize_path`, which never
    // produces the prefix for a `MAX_PATH`-fitting path; this brings existing
    // rows in line, so Browse's path-keyed `HashMap` matches `read_dir` output.
    // `cfg!` const-folds, leaving the fn compiled and testable everywhere.
    if cfg!(target_os = "windows") {
        strip_windows_verbatim_paths(&write_pool).await?;
    }

    // A small fixed band rather than one connection per core: reads here are
    // tiny and effectively sequential, and scaling with core count only
    // multiplies per-connection page-cache and statement memory for concurrency
    // this workload never uses. (`.claude/rules/sqlx.md`'s num_cpus advice is
    // for a server profile.)
    let read_opts = SqliteConnectOptions::from_str(&db_url)?
        .journal_mode(SqliteJournalMode::Wal)
        .read_only(true)
        .busy_timeout(Duration::from_secs(5))
        .pragma("foreign_keys", "ON")
        .statement_cache_capacity(STATEMENT_CACHE_CAPACITY)
        .pragma("cache_size", READ_CACHE_KIB)
        .pragma("temp_store", "FILE")
        .pragma("mmap_size", READ_MMAP_BYTES);

    // Reap connections opened by the boot prefetch after fifteen idle seconds,
    // rather than sqlx's default ten minutes. `min_connections` stays at its
    // default — a cold reopen costs nothing at this scale.
    let read_pool = SqlitePoolOptions::new()
        .max_connections(READ_CONNECTIONS)
        .idle_timeout(Duration::from_secs(15))
        .connect_with(read_opts)
        .await?;

    log::info!("Database initialized successfully (read pool: {READ_CONNECTIONS} connections)");

    Ok(DbPool { read: read_pool, write: write_pool })
}

#[doc(hidden)]
impl DbPool {
    /// An in-memory `DbPool` for tests. Read and write share one connection,
    /// in-memory `SQLite` being per-connection.
    pub async fn test_pool() -> Result<Self, AppError> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?
            .pragma("foreign_keys", "ON")
            .pragma("synchronous", "NORMAL")
            .pragma("temp_store", "MEMORY");

        let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await?;

        sqlx::migrate!("../../migrations").run(&pool).await?;

        Ok(DbPool { read: pool.clone(), write: pool })
    }
}

#[cfg(test)]
#[path = "tests/mod_tests.rs"]
mod tests;
