use crate::database::queries;
use crate::database::queries::fixtures::insert_test_track;
use crate::database::{DbPool, MAX_BINDS_PER_STATEMENT, chunked_in_query};
use netodia_core::error::AppError;

// Private to `database/mod.rs`; reachable from this child test module via `super::`.
use super::{FTS_OPTIMIZE, strip_windows_verbatim_paths};

/// Exercise the real file-backed pools: the in-memory fixture intentionally has
/// different options and cannot catch an option being applied to only one pool.
#[tokio::test]
async fn file_database_uses_bounded_caches_on_both_pools() -> Result<(), AppError> {
    let dir = tempfile::tempdir()?;
    let paths = netodia_core::config::Paths::rooted_at(dir.path().to_path_buf());
    paths.create_dirs()?;
    let db = super::init_database(&paths).await?;

    let read_cache: (i64,) = sqlx::query_as("PRAGMA cache_size").fetch_one(db.read()).await?;
    let write_cache: (i64,) = sqlx::query_as("PRAGMA cache_size").fetch_one(db.write()).await?;
    let read_temp: (i64,) = sqlx::query_as("PRAGMA temp_store").fetch_one(db.read()).await?;
    let write_temp: (i64,) = sqlx::query_as("PRAGMA temp_store").fetch_one(db.write()).await?;
    let read_mmap: (i64,) = sqlx::query_as("PRAGMA mmap_size").fetch_one(db.read()).await?;

    assert_eq!(read_cache.0, -1024);
    assert_eq!(write_cache.0, -2048);
    assert_eq!(read_temp.0, 1, "large read sorts must be able to spill to disk");
    assert_eq!(write_temp.0, 1, "large write sorts must be able to spill to disk");
    assert_eq!(read_mmap.0, 8_388_608);
    db.close().await;
    Ok(())
}

#[test]
fn max_binds_per_statement_is_999() {
    assert_eq!(MAX_BINDS_PER_STATEMENT, 999);
}

#[tokio::test]
async fn chunked_in_query_empty_input() -> Result<(), AppError> {
    let db = DbPool::test_pool().await?;
    let empty: Vec<i64> = Vec::new();
    let result: Vec<(i64,)> = chunked_in_query(db.read(), &empty, |ph| {
        format!("SELECT id FROM tracks WHERE id IN ({ph})")
    })
    .await?;
    assert!(result.is_empty());
    Ok(())
}

#[tokio::test]
async fn chunked_in_query_small_set() -> Result<(), AppError> {
    let db = DbPool::test_pool().await?;

    // Insert a folder + track so we have data to query
    queries::folder::insert_folder(&db, "/test", true).await?;

    let track_id =
        insert_test_track(&db, "/test/song.mp3", "Test Song", "Test Artist", "Test Album", "Rock")
            .await?;

    let ids = vec![track_id, 99999]; // one real, one missing
    let result: Vec<(i64,)> =
        chunked_in_query(db.read(), &ids, |ph| format!("SELECT id FROM tracks WHERE id IN ({ph})"))
            .await?;

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, track_id);
    Ok(())
}

#[tokio::test]
async fn chunked_in_query_single_item() -> Result<(), AppError> {
    let db = DbPool::test_pool().await?;
    queries::folder::insert_folder(&db, "/test", true).await?;
    let track_id = insert_test_track(&db, "/test/a.mp3", "A", "Art", "Alb", "Pop").await?;

    let ids = vec![track_id];
    let result: Vec<(i64,)> =
        chunked_in_query(db.read(), &ids, |ph| format!("SELECT id FROM tracks WHERE id IN ({ph})"))
            .await?;

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, track_id);
    Ok(())
}

#[tokio::test]
async fn chunked_in_query_preserves_all_results() -> Result<(), AppError> {
    let db = DbPool::test_pool().await?;
    queries::folder::insert_folder(&db, "/test", true).await?;

    let mut track_ids = Vec::new();
    for i in 0..5 {
        let id = insert_test_track(
            &db,
            &format!("/test/track_{i}.mp3"),
            &format!("Track {i}"),
            "Artist",
            "Album",
            "Rock",
        )
        .await?;
        track_ids.push(id);
    }

    let result: Vec<(i64,)> = chunked_in_query(db.read(), &track_ids, |ph| {
        format!("SELECT id FROM tracks WHERE id IN ({ph})")
    })
    .await?;

    assert_eq!(result.len(), 5);
    let result_ids: Vec<i64> = result.iter().map(|r| r.0).collect();
    for id in &track_ids {
        assert!(result_ids.contains(id), "missing track id {id}");
    }
    Ok(())
}

#[tokio::test]
async fn chunked_in_query_large_set_splits_correctly() -> Result<(), AppError> {
    let db = DbPool::test_pool().await?;
    queries::folder::insert_folder(&db, "/test", true).await?;

    // Insert more tracks than MAX_BINDS_PER_STATEMENT to test chunking
    // Use a smaller number that still validates the chunking logic
    let count = 50;
    let mut track_ids = Vec::with_capacity(count);
    for i in 0..count {
        let id = insert_test_track(
            &db,
            &format!("/test/track_{i}.mp3"),
            &format!("Track {i}"),
            "Artist",
            "Album",
            "Rock",
        )
        .await?;
        track_ids.push(id);
    }

    let result: Vec<(i64,)> = chunked_in_query(db.read(), &track_ids, |ph| {
        format!("SELECT id FROM tracks WHERE id IN ({ph})")
    })
    .await?;

    assert_eq!(result.len(), count);
    Ok(())
}

#[tokio::test]
async fn chunked_in_query_with_string_binds() -> Result<(), AppError> {
    let db = DbPool::test_pool().await?;
    queries::folder::insert_folder(&db, "/test", true).await?;
    insert_test_track(&db, "/test/song.mp3", "Song", "Art", "Alb", "Pop").await?;

    let paths = vec!["/test/song.mp3".to_owned(), "/test/missing.mp3".to_owned()];
    let result: Vec<(i64,)> = chunked_in_query(db.read(), &paths, |ph| {
        format!("SELECT id FROM tracks WHERE file_path IN ({ph})")
    })
    .await?;

    assert_eq!(result.len(), 1);
    Ok(())
}

#[tokio::test]
async fn db_pool_close_succeeds() -> Result<(), AppError> {
    let db = DbPool::test_pool().await?;
    // Ensure queries work before close
    let _: Vec<(i64,)> = sqlx::query_as("SELECT 1").fetch_all(db.read()).await?;
    db.close().await;
    // No panic means success
    Ok(())
}

/// `close` can only log this one, so nothing it does is visible to the test
/// above — and fts5 rejects an unrecognised command at *step* time, not at
/// prepare, so a typo would be a shutdown that quietly stopped compacting the
/// index. Running the same const here is what turns that into a failure.
#[tokio::test]
async fn the_shutdown_fts_optimize_is_a_command_fts5_accepts() -> Result<(), AppError> {
    let db = DbPool::test_pool().await?;
    sqlx::query(FTS_OPTIMIZE).execute(db.write()).await?;
    Ok(())
}

#[tokio::test]
async fn db_pool_read_write_accessors() -> Result<(), AppError> {
    let db = DbPool::test_pool().await?;
    // Verify both pools are functional
    let _: (i64,) = sqlx::query_as("SELECT 42").fetch_one(db.read()).await?;
    let _: (i64,) = sqlx::query_as("SELECT 42").fetch_one(db.write()).await?;
    Ok(())
}

#[tokio::test]
async fn chunked_in_query_all_missing() -> Result<(), AppError> {
    let db = DbPool::test_pool().await?;
    let ids = vec![9999_i64, 8888, 7777];
    let result: Vec<(i64,)> =
        chunked_in_query(db.read(), &ids, |ph| format!("SELECT id FROM tracks WHERE id IN ({ph})"))
            .await?;
    assert!(result.is_empty());
    Ok(())
}

// ---- strip_windows_verbatim_paths -----------------------------------------

async fn insert_folder_raw(db: &DbPool, path: &str) -> Result<(), AppError> {
    sqlx::query("INSERT INTO folders (path, is_enabled, added_at) VALUES (?, 1, '2026-01-01T00:00:00+00:00')")
        .bind(path)
        .execute(db.write())
        .await?;
    Ok(())
}

async fn folder_paths(db: &DbPool) -> Result<Vec<String>, AppError> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT path FROM folders ORDER BY id").fetch_all(db.read()).await?;
    Ok(rows.into_iter().map(|(p,)| p).collect())
}

#[tokio::test]
async fn strip_verbatim_drive_form() -> Result<(), AppError> {
    let db = DbPool::test_pool().await?;
    insert_folder_raw(&db, r"\\?\C:\Music").await?;
    insert_folder_raw(&db, r"\\?\D:\Library\Albums").await?;
    strip_windows_verbatim_paths(db.write()).await?;
    assert_eq!(folder_paths(&db).await?, vec![r"C:\Music", r"D:\Library\Albums"]);
    Ok(())
}

#[tokio::test]
async fn strip_verbatim_unc_form() -> Result<(), AppError> {
    let db = DbPool::test_pool().await?;
    insert_folder_raw(&db, r"\\?\UNC\nas\share\music").await?;
    insert_folder_raw(&db, r"\\?\UNC\server\public\Albums\Rock").await?;
    strip_windows_verbatim_paths(db.write()).await?;
    assert_eq!(
        folder_paths(&db).await?,
        vec![r"\\nas\share\music", r"\\server\public\Albums\Rock"],
    );
    Ok(())
}

#[tokio::test]
async fn strip_verbatim_leaves_clean_paths_untouched() -> Result<(), AppError> {
    let db = DbPool::test_pool().await?;
    insert_folder_raw(&db, r"C:\Already\Clean").await?;
    insert_folder_raw(&db, r"\\server\share\plain").await?;
    insert_folder_raw(&db, "/posix/style/path").await?;
    let before = folder_paths(&db).await?;
    strip_windows_verbatim_paths(db.write()).await?;
    assert_eq!(folder_paths(&db).await?, before);
    Ok(())
}

#[tokio::test]
async fn strip_verbatim_keeps_prefix_when_result_would_exceed_max_path() -> Result<(), AppError> {
    // Stripping `\\?\` would leave a 260+ char path that can't be opened
    // without the verbatim prefix. `dunce::canonicalize` also keeps the
    // prefix in this case, so the strip must too. `tail` is sized so the
    // post-strip length (`C:\` + tail) is exactly 260.
    let db = DbPool::test_pool().await?;
    let tail = "a".repeat(257);
    let long_drive = format!(r"\\?\C:\{tail}"); // LENGTH = 4 + 3 + 257 = 264
    let long_unc = format!(r"\\?\UNC\srv\share\{tail}"); // LENGTH = 8 + 11 + 257 = 276 (post-strip 270)
    insert_folder_raw(&db, &long_drive).await?;
    insert_folder_raw(&db, &long_unc).await?;
    let before = folder_paths(&db).await?;
    strip_windows_verbatim_paths(db.write()).await?;
    assert_eq!(folder_paths(&db).await?, before);
    Ok(())
}

#[tokio::test]
async fn strip_verbatim_is_idempotent() -> Result<(), AppError> {
    let db = DbPool::test_pool().await?;
    insert_folder_raw(&db, r"\\?\C:\Music").await?;
    insert_folder_raw(&db, r"\\?\UNC\nas\share\music").await?;
    strip_windows_verbatim_paths(db.write()).await?;
    let after_first = folder_paths(&db).await?;
    strip_windows_verbatim_paths(db.write()).await?;
    assert_eq!(folder_paths(&db).await?, after_first);
    Ok(())
}

#[tokio::test]
async fn strip_verbatim_normalizes_track_file_paths() -> Result<(), AppError> {
    // Tracks share the same SQL shape as folders; one mixed-form spot-check
    // guards against a future regression where only one table is updated.
    let db = DbPool::test_pool().await?;
    queries::folder::insert_folder(&db, "/host", true).await?;
    insert_test_track(&db, r"\\?\C:\Music\drive.mp3", "Drive", "A", "Alb", "Pop").await?;
    insert_test_track(&db, r"\\?\UNC\nas\share\unc.mp3", "UNC", "A", "Alb", "Pop").await?;
    insert_test_track(&db, r"D:\Already\Clean.mp3", "Clean", "A", "Alb", "Pop").await?;

    strip_windows_verbatim_paths(db.write()).await?;

    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT file_path FROM tracks ORDER BY id").fetch_all(db.read()).await?;
    let paths: Vec<String> = rows.into_iter().map(|(p,)| p).collect();
    assert_eq!(
        paths,
        vec![
            r"C:\Music\drive.mp3".to_owned(),
            r"\\nas\share\unc.mp3".to_owned(),
            r"D:\Already\Clean.mp3".to_owned(),
        ],
    );
    Ok(())
}
