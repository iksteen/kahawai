//! Hub-owned operational and user state: accounts, grants, credentials,
//! satellites, settings, pacing and watch state keyed by stable mediadb IDs.
//! Migration 0089 removes the retired catalogue and its historical watch rows.
//! Current catalogue/metadata live exclusively in the separately migrated
//! mediadb database opened by `open_catalogue`. `ed2k_aid` remains an input
//! cache for enrichment: its paid AniDB answers are imported into mediadb.

use std::path::Path;

use anyhow::{Context, Result};
use kahawai_sqlite::Database;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};

pub async fn open(data_dir: &Path) -> Result<Database> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    let path = data_dir.join("hub.db");
    match kahawai_core::private::create(&path) {
        Ok(file) => drop(file),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            anyhow::ensure!(
                std::fs::metadata(&path)?.is_file(),
                "{} is not a file",
                path.display()
            );
            kahawai_core::private::narrow(&path)
                .with_context(|| format!("restricting {}", path.display()))?;
        }
        Err(error) => {
            return Err(error).with_context(|| format!("creating {}", path.display()));
        }
    }
    let writer_options = SqliteConnectOptions::new()
        .filename(&path)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        // Overwrite deleted rows instead of unlinking them: a freed cell keeps
        // its bytes, so `strings hub.db` reads back deleted settings, the
        // operator's provider keys among them.
        .pragma("secure_delete", "on")
        // 8 MiB of page cache PER CONNECTION (negative = KiB, not pages).
        //
        // SQLite's default is 2 MB, which is smaller than the index a
        // browse page walks: a deep page over 50k items thrashed it and
        // the SAME query took 253 ms or 50 ms depending on which pooled
        // connection served it. Measured at 2/8/16/64 MiB, the two-mode
        // behaviour disappears at 8 and nothing improves above it.
        //
        // Cost, both axes: memory is a CEILING of 8 connections × 8 MiB =
        // 64 MiB, allocated lazily as pages are touched, against a hub
        // that already holds a 61 MB database and serves video. Latency
        // at point of use is the thing bought: browse is the one path a
        // user waits on synchronously.
        .pragma("cache_size", "-8192")
        // The enrichment pass, the repick triggers and a browse request
        // are three legitimate concurrent writers; sqlx's default 5 s
        // busy handout has been seen expiring under a long pass
        // ("database is locked" in the binder). Waiting longer IS the
        // correct behaviour — no writer here holds the lock unbounded.
        .busy_timeout(std::time::Duration::from_secs(30));
    let reader_options = SqliteConnectOptions::new()
        .filename(&path)
        .read_only(true)
        .foreign_keys(true)
        .pragma("cache_size", "-8192")
        .busy_timeout(std::time::Duration::from_secs(30));
    // Preserve the former eight-connection/64 MiB ceiling: seven readers at
    // 8 MiB each plus the actor's sole 8 MiB writer connection.
    let database = Database::connect_with(writer_options, reader_options, 7)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    // WAL and SHM are created by SQLite after the main file, inheriting its
    // mode. Narrow them as well for existing databases that arrived wider.
    for suffix in ["-wal", "-shm"] {
        kahawai_core::private::narrow(&data_dir.join(format!("hub.db{suffix}")))?;
    }
    database
        .write("hub migrations", |connection| {
            Box::pin(async move {
                sqlx::migrate!("./migrations")
                    .run_direct(None, connection, false)
                    .await
                    .context("running migrations")
            })
        })
        .await?;
    Ok(database)
}

/// Open the independent catalogue before exposing any hub listeners.
pub async fn open_catalogue(data_dir: &Path) -> Result<kahawai_mediadb::Store> {
    let path = data_dir.join("mediadb.db");
    if path.try_exists()? {
        kahawai_mediadb::Store::open(&path).await
    } else {
        kahawai_mediadb::Store::create(&path).await
    }
}

/// Reset the write-ahead log, and say so when it could not be.
///
/// `secure_delete` zeroes a deleted row in the page image it writes, but the
/// image from BEFORE the delete stays readable in the log until this runs — so
/// "the plaintext is gone" is only true once the log has been truncated.
///
/// The pragma reports failure in its result row rather than as an error:
/// `busy = 1` means another connection held the log open and nothing was
/// truncated. `execute()` discards that row, which made a checkpoint that did
/// nothing look exactly like one that worked.
pub async fn checkpoint_truncate(db: &Database) -> Result<()> {
    let (busy, _log, _checkpointed): (i64, i64, i64) = db
        .write("truncate checkpoint", |connection| {
            Box::pin(async move {
                sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
                    .fetch_one(connection)
                    .await
                    .context("truncating the write-ahead log")
            })
        })
        .await?;
    if busy != 0 {
        tracing::warn!(
            "the write-ahead log could not be truncated — a reader held it open, so what was \
             just deleted stays readable in hub.db-wal until a later checkpoint"
        );
    }
    Ok(())
}

/// In-memory DB for tests.
pub async fn open_in_memory() -> Result<Database> {
    let name = format!("file:kahawai-test-{}", ulid::Ulid::generate());
    let writer = SqliteConnectOptions::new()
        .filename(&name)
        .in_memory(true)
        .shared_cache(true)
        .foreign_keys(true);
    let reader = SqliteConnectOptions::new()
        .filename(&name)
        .in_memory(true)
        .shared_cache(true)
        .foreign_keys(true);
    let database = Database::connect_with(writer, reader, 1).await?;
    database
        .write("hub test migrations", |connection| {
            Box::pin(async move {
                sqlx::migrate!("./migrations")
                    .run_direct(None, connection, false)
                    .await?;
                Ok(())
            })
        })
        .await?;
    Ok(database)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    #[tokio::test]
    async fn the_database_is_private_before_and_after_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");

        let db = open(dir.path()).await.unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        db.close().await;

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let db = open(dir.path()).await.unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        db.close().await;
    }

    /// The provider keys live in `settings` in the clear. The second row is
    /// what keeps the page allocated, so the freeblock is not simply freed.
    #[tokio::test]
    async fn a_deleted_setting_is_not_still_in_the_file() {
        const CANARY: &str = "CANARY-provider-key-0123456789";

        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path()).await.unwrap();
        for (key, value) in [("tmdb_api_key", CANARY), ("stays", "keeps the page")] {
            sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?)")
                .bind(key)
                .bind(value)
                .execute(&db)
                .await
                .unwrap();
        }
        sqlx::query("DELETE FROM settings WHERE key = 'tmdb_api_key'")
            .execute(&db)
            .await
            .unwrap();
        // Closing the last connection checkpoints and removes the WAL, so the
        // main file is the whole story by the time it is read.
        db.close().await;
        assert!(!dir.path().join("hub.db-wal").exists());

        let bytes = std::fs::read(dir.path().join("hub.db")).unwrap();
        assert!(
            !bytes.windows(CANARY.len()).any(|w| w == CANARY.as_bytes()),
            "the deleted value is still readable in hub.db"
        );
    }
}
