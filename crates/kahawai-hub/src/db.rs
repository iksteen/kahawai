//! Embedded SQLite (HUB-13): WAL mode, migrations on open, no external
//! services.

use std::path::Path;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;

pub async fn open(data_dir: &Path) -> Result<SqlitePool> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    let path = data_dir.join("hub.db");
    let opts = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
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
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    // The DB holds password hashes and session state; SQLite gives -wal/-shm
    // the same mode as the main file, so 0600 here covers all three.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for suffix in ["", "-wal", "-shm"] {
            let p = data_dir.join(format!("hub.db{suffix}"));
            if p.exists() {
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
            }
        }
    }
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("running migrations")?;
    install_derived(&pool).await?;
    backfill_norm_artist(&pool).await?;
    backfill_revision(&pool).await?;
    repair_release_tag_titles(&pool).await?;
    Ok(pool)
}

/// Fill `items.norm_artist` where it is missing (0041). Folding is a
/// Rust function, so the migration could not do this; the write sites
/// keep it filled from here on, which makes this a no-op after its first
/// run — the guard query is one indexless scan of a column that is
/// almost always fully populated.
async fn backfill_norm_artist(pool: &SqlitePool) -> Result<()> {
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
async fn backfill_revision(pool: &SqlitePool) -> Result<()> {
    let missing: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT module_id, collection_id, path_rel FROM files WHERE revision IS NULL",
    )
    .fetch_all(pool)
    .await?;
    if missing.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for (m, c, path) in &missing {
        sqlx::query(
            "UPDATE files SET revision = ? WHERE module_id = ? AND collection_id = ? AND path_rel = ?",
        )
        .bind(kahawai_core::names::release_revision(path) as i64)
        .bind(m)
        .bind(c)
        .bind(path)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    tracing::info!(rows = missing.len(), "release revisions backfilled");
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
async fn repair_release_tag_titles(pool: &SqlitePool) -> Result<()> {
    let rows: Vec<(String, String, Option<i64>, String)> = sqlx::query_as(
        "SELECT id, title, year, kind FROM items
          WHERE kind IN ('show', 'movie') AND title LIKE '%)'",
    )
    .fetch_all(pool)
    .await?;
    let mut fixed = 0;
    for (id, title, year, kind) in rows {
        let stripped = kahawai_core::names::strip_release_tags(&title);
        if stripped == title || stripped.is_empty() {
            continue;
        }
        let norm = kahawai_core::names::normalize_title(&stripped);
        let taken: Option<String> = sqlx::query_scalar(
            "SELECT id FROM items WHERE kind = ?1 AND norm_title = ?2 AND year IS ?3 AND id <> ?4",
        )
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

/// In-memory DB for tests.
pub async fn open_in_memory() -> Result<SqlitePool> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    install_derived(&pool).await?;
    Ok(pool)
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
async fn install_derived(pool: &SqlitePool) -> Result<()> {
    // Safe by construction: the statement is generated from a fixed field
    // table in providers.rs, with no caller input anywhere in it.
    sqlx::raw_sql(sqlx::AssertSqlSafe(crate::providers::resolved_metadata_sql()))
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
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP TRIGGER IF EXISTS {name}")))
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
    crate::providers::reassign(&mut tx, None, None).await.context("rebuilding item_match")?;
    tx.commit().await?;
    tracing::info!(triggers = want.len(), "assignment triggers installed; item_match rebuilt");
    Ok(())
}
