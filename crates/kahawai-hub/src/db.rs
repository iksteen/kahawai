//! Embedded SQLite (HUB-13): WAL mode, migrations on open, no external
//! services.

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
    install_derived(&database).await?;
    backfill_norm_artist(&database).await?;
    backfill_revision(&database).await?;
    backfill_playable_source_families(&database).await?;
    repair_release_tag_titles(&database).await?;
    Ok(database)
}

/// Fill `items.norm_artist` where it is missing (0041). Folding is a
/// Rust function, so the migration could not do this; the write sites
/// keep it filled from here on, which makes this a no-op after its first
/// run — the guard query is one indexless scan of a column that is
/// almost always fully populated.
async fn backfill_norm_artist(pool: &Database) -> Result<()> {
    let missing: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, artist FROM items WHERE artist IS NOT NULL AND norm_artist IS NULL",
    )
    .fetch_all(pool)
    .await?;
    if missing.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for (id, artist) in &missing {
        sqlx::query("UPDATE items SET norm_artist = ? WHERE id = ?")
            .bind(crate::enrich::fold(artist))
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    tracing::info!(rows = missing.len(), "norm_artist backfilled");
    Ok(())
}

/// Fill `files.revision` where it is missing (0043) — same story as
/// `norm_artist`: the parse is Rust, so the migration could not do it,
/// and this is a no-op after its first run.
async fn backfill_revision(pool: &Database) -> Result<()> {
    let missing: Vec<(i64, String)> =
        sqlx::query_as("SELECT id,path_rel FROM files WHERE revision IS NULL")
            .fetch_all(pool)
            .await?;
    if missing.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for (id, path) in &missing {
        sqlx::query("UPDATE files SET revision=? WHERE id=?")
            .bind(kahawai_core::names::release_revision(path) as i64)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    tracing::info!(rows = missing.len(), "release revisions backfilled");
    Ok(())
}

/// Replace migration 57's lossless temporary multipart grouping keys with the
/// filename-derived rendition family. Existing ordinary rows are already
/// final (`file:<id>`). This is bounded to the handful of multipart files and
/// never changes item identity or scan generations.
async fn backfill_playable_source_families(pool: &Database) -> Result<()> {
    type Row = (i64, String, String, String, Option<i64>, i64, i64, String);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT ps.id,ps.module_id,ps.collection_id,ps.item_id,ps.root_id,
                p.file_id,p.ordinal,f.path_rel
           FROM playable_sources ps
           JOIN playable_source_parts p ON p.playable_source_id=ps.id
           JOIN files f ON f.id=p.file_id
          WHERE ps.family_key LIKE 'legacy-item:%'
          ORDER BY ps.id,p.ordinal,p.file_id",
    )
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    type Families = std::collections::BTreeMap<String, Vec<(i64, i64)>>;
    type Source = (String, String, String, Option<i64>, Families);
    let mut by_source: std::collections::BTreeMap<i64, Source> = Default::default();
    for (source_id, module, collection, item, root, file_id, ordinal, path) in rows {
        by_source
            .entry(source_id)
            .or_insert_with(|| (module, collection, item, root, Default::default()))
            .4
            .entry(kahawai_core::names::rendition_family_key(&path))
            .or_default()
            .push((file_id, ordinal));
    }
    let mut tx = pool.begin().await?;
    for (source_id, (module, collection, item, root, families)) in by_source {
        for (n, (family, parts)) in families.into_iter().enumerate() {
            let expected = parts.iter().map(|(_, ordinal)| *ordinal).max().unwrap_or(1);
            let target = if n == 0 {
                sqlx::query("UPDATE playable_sources SET family_key=?,expected_parts=? WHERE id=?")
                    .bind(&family)
                    .bind(expected)
                    .bind(source_id)
                    .execute(&mut *tx)
                    .await?;
                source_id
            } else {
                sqlx::query_scalar(
                    "INSERT INTO playable_sources
                       (module_id,collection_id,item_id,root_id,family_key,expected_parts)
                     VALUES(?,?,?,?,?,?) RETURNING id",
                )
                .bind(&module)
                .bind(&collection)
                .bind(&item)
                .bind(root)
                .bind(&family)
                .bind(expected)
                .fetch_one(&mut *tx)
                .await?
            };
            if target != source_id {
                for (file_id, _) in parts {
                    sqlx::query(
                        "UPDATE playable_source_parts SET playable_source_id=? WHERE file_id=?",
                    )
                    .bind(target)
                    .bind(file_id)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Retitle items whose names carry a trailing release tag —
/// "(Dual-Audio)", "(Eng.-Dub)" — that the parser now strips (see
/// names::strip_release_tags). Same one-shot shape as the other
/// backfills: the parser stops producing such titles, so after this
/// heals the existing rows it never matches again. A rename that would
/// collide with an existing item is left for a human or for hash
/// reconciliation — renaming into a collision would create twins the
/// dedup key can no longer tell apart.
async fn repair_release_tag_titles(pool: &Database) -> Result<()> {
    let rows: Vec<(String, String, Option<i64>, String, String, String)> = sqlx::query_as(
        "SELECT id,title,year,kind,module_id,collection_id FROM items
          WHERE kind IN ('show','movie') AND title LIKE '%)'",
    )
    .fetch_all(pool)
    .await?;
    let mut fixed = 0;
    for (id, title, year, kind, module_id, collection_id) in rows {
        let stripped = kahawai_core::names::strip_release_tags(&title);
        if stripped == title || stripped.is_empty() {
            continue;
        }
        let norm = kahawai_core::names::normalize_title(&stripped);
        let taken: Option<String> = sqlx::query_scalar(
            "SELECT id FROM items WHERE module_id=?1 AND collection_id=?2
               AND kind=?3 AND norm_title=?4 AND year IS ?5 AND id<>?6",
        )
        .bind(&module_id)
        .bind(&collection_id)
        .bind(&kind)
        .bind(&norm)
        .bind(year)
        .bind(&id)
        .fetch_optional(pool)
        .await?;
        if let Some(other) = taken {
            tracing::warn!(item = %id, title = %title, duplicate_of = %other,
                "release-tag title left in place: cleaned name already exists");
            continue;
        }
        sqlx::query("UPDATE items SET title = ?, norm_title = ? WHERE id = ?")
            .bind(&stripped)
            .bind(&norm)
            .bind(&id)
            .execute(pool)
            .await?;
        fixed += 1;
    }
    if fixed > 0 {
        tracing::info!(rows = fixed, "release-tag titles repaired");
    }
    Ok(())
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
    let (busy, _log, _checkpointed): (i64, i64, i64) =
        sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(db)
            .await
            .context("truncating the write-ahead log")?;
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
    install_derived(&database).await?;
    Ok(database)
}

/// Derivations are installed on open, not by a migration: they derive
/// rather than store, so their definition is free to change, and a
/// migration is an immutable log of changes to what IS stored.
///
/// Triggers that MAINTAIN a stored table are the same category with one
/// extra hazard. A stale view fails loudly — a column it names is gone.
/// A stale trigger keeps working and quietly maintains the wrong answer.
/// So a definition that differs from what this binary wants is not just
/// replaced: everything it was maintaining is rebuilt from scratch, which
/// makes a downgrade-then-upgrade self-healing rather than silently
/// wrong.
async fn install_derived(pool: &Database) -> Result<()> {
    // Safe by construction: the statement is generated from a fixed field
    // table in providers.rs, with no caller input anywhere in it.
    sqlx::raw_sql(sqlx::AssertSqlSafe(
        crate::providers::resolved_metadata_sql(),
    ))
    .execute(pool)
    .await
    .context("installing resolved_metadata")?;

    let want = crate::providers::repick_triggers();
    let have: Vec<(String, String)> = sqlx::query_as(
        "SELECT name, sql FROM sqlite_schema
          WHERE type = 'trigger' AND name LIKE 'repick\\_%' ESCAPE '\\'
          ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .context("reading installed triggers")?;
    let mut sorted = want.clone();
    sorted.sort();
    if sorted == have {
        return Ok(());
    }

    let mut tx = pool.begin().await?;
    for (name, _) in have.iter().chain(want.iter()) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP TRIGGER IF EXISTS {name}"
        )))
        .execute(&mut *tx)
        .await
        .with_context(|| format!("dropping trigger {name}"))?;
    }
    for (name, sql) in &want {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.clone()))
            .execute(&mut *tx)
            .await
            .with_context(|| format!("installing trigger {name}"))?;
    }
    // What the old definitions maintained is now of unknown provenance.
    crate::providers::reassign(&mut tx, None, None)
        .await
        .context("rebuilding item_match")?;
    tx.commit().await?;
    tracing::info!(
        triggers = want.len(),
        "assignment triggers installed; item_match rebuilt"
    );
    Ok(())
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
