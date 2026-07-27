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
        .foreign_keys(true);
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
    Ok(pool)
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
