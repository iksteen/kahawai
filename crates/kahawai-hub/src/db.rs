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
    install_views(&pool).await?;
    Ok(pool)
}

/// In-memory DB for tests.
pub async fn open_in_memory() -> Result<SqlitePool> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    install_views(&pool).await?;
    Ok(pool)
}

/// Views are installed on open, not by a migration: they derive rather
/// than store, so their definition is free to change, and a migration is
/// an immutable log of changes to what IS stored.
async fn install_views(pool: &SqlitePool) -> Result<()> {
    sqlx::raw_sql(&crate::providers::resolved_metadata_sql())
        .execute(pool)
        .await
        .context("installing resolved_metadata")?;
    Ok(())
}
