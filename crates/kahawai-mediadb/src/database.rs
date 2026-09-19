//! Mediadb owns this database file and its SQLx migration history. The hub's
//! users, credentials and watch state live in a different file and are never
//! migrated here. Store::create requires a new path; Store::open checks ownership
//! read-only before acquiring a writer. The initial migration's recorded checksum
//! identifies the database, avoiding another marker/version table. Pre-migration
//! development databases have no such history and must be recreated explicitly.
//!
//! SQLx applies and records each migration transactionally, checks immutable
//! checksums and rejects unknown applied versions. Only the serialized writer runs
//! migrations; no Store escapes until they finish. A failed upgrade closes the
//! database and leaves earlier migrations intact, so a later open can retry.
//! An initial creation failure leaves an uninitialized file, not a usable Store.
//! Add numbered migrations; never edit an applied file or its history. Schema
//! meaning belongs beside the enforcing Rust operations, not in migration comments.

use crate::Store;
use anyhow::{Context, Result, ensure};
use kahawai_sqlite::Database;
use sqlx::Connection;
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use std::path::Path;

const MIGRATOR: Migrator = sqlx::migrate!("./migrations");

impl Store {
    /// Open an existing mediadb and apply pending migrations before returning it.
    /// A foreign or pre-migration database is rejected without opening a writer.
    pub async fn open(path: &Path) -> Result<Self> {
        open(path, MIGRATOR).await
    }

    /// An isolated ephemeral catalogue, useful for embedding and tests.
    pub async fn in_memory() -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(format!("file:mediadb-{}", crate::id()))
            .in_memory(true)
            .shared_cache(true)
            .foreign_keys(true);
        let db = Database::connect_with(options.clone(), options, 1).await?;
        db.write("mediadb migrations", |c| {
            Box::pin(async move {
                MIGRATOR.run_direct(None, c, false).await?;
                Ok(())
            })
        })
        .await?;
        Ok(Self { db })
    }

    /// Create a new mediadb and run its migrations. Never overwrite an existing file.
    pub async fn create(path: &Path) -> Result<Self> {
        let file = kahawai_core::private::create(path)?;
        drop(file);
        connect(path, MIGRATOR).await
    }
}

async fn open(path: &Path, migrator: Migrator) -> Result<Store> {
    let mut reader = sqlx::SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(path).read_only(true),
    )
    .await?;
    let initial = migrator
        .iter()
        .next()
        .context("missing initial mediadb migration")?;
    let checksum: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT checksum FROM _sqlx_migrations WHERE version=? AND success=true",
    )
    .bind(initial.version)
    .fetch_optional(&mut reader)
    .await
    .context("not a migrated mediadb database; recreate disposable pre-migration databases")?;
    ensure!(
        checksum.as_deref() == Some(initial.checksum.as_ref()),
        "not a compatible mediadb database: initial migration is missing or its checksum differs"
    );
    reader.close().await?;
    for suffix in ["", "-wal", "-shm"] {
        let mut owned_path = path.as_os_str().to_os_string();
        owned_path.push(suffix);
        kahawai_core::private::narrow(Path::new(&owned_path))?;
    }
    connect(path, migrator).await
}

async fn connect(path: &Path, migrator: Migrator) -> Result<Store> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal);
    let db = Database::connect_with(options.clone(), options.read_only(true), 3).await?;
    let result = db
        .write("mediadb migrations", move |connection| {
            Box::pin(async move {
                migrator
                    .run_direct(None, connection, false)
                    .await
                    .context("running mediadb migrations")
            })
        })
        .await;
    if let Err(error) = result {
        db.close().await;
        return Err(error);
    }
    Ok(Store { db })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MediaType;
    use sqlx::SqlSafeStr;
    use sqlx::migrate::{MigrateError, Migration, MigrationType};

    fn upgrade_version() -> i64 {
        MIGRATOR.iter().last().unwrap().version + 1
    }

    fn upgrade(sql: &'static str) -> Migrator {
        let mut migrations: Vec<_> = MIGRATOR.iter().cloned().collect();
        migrations.push(Migration::new(
            upgrade_version(),
            "test upgrade".into(),
            MigrationType::Simple,
            sql.into_sql_str(),
            false,
        ));
        Migrator::with_migrations(migrations)
    }

    async fn reader(path: &Path) -> sqlx::SqliteConnection {
        sqlx::SqliteConnection::connect_with(
            &SqliteConnectOptions::new().filename(path).read_only(true),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn episode_table_rename_preserves_coverage_and_cascade() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mediadb.db");
        std::fs::File::create(&path).unwrap();
        let old =
            Migrator::with_migrations(MIGRATOR.iter().filter(|m| m.version < 4).cloned().collect());
        let s = connect(&path, old).await.unwrap();
        let mut tx = s.db.begin().await.unwrap();
        sqlx::raw_sql("INSERT INTO mediahosts VALUES('host','Host');
            INSERT INTO collections(id,mediahost_id,remote_id,media_type,epoch) VALUES('col','host','shows','series','epoch');
            INSERT INTO collection_roots(id,collection_id,token,path) VALUES('root','col','root','/shows');
            INSERT INTO library_items VALUES('show','series','Show','show',2000,'');
            INSERT INTO collection_items(id,collection_id,root_id,occurrence,title,library_item_id,description_json)
                VALUES('copy','col','root','Show','Show','show','{}');
            INSERT INTO media_entries(id,collection_id,item_id,occurrence,kind,title)
                VALUES('entry','col','copy','combined','episode','Combined');
            INSERT INTO entry_episodes VALUES('entry',1,'season',1,3,4),('entry',2,'absolute',NULL,123,NULL);")
            .execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        s.close().await;

        for _ in 0..2 {
            let s = Store::open(&path).await.unwrap();
            let spans: Vec<(String, Option<i64>, i64, Option<i64>)> = sqlx::query_as(
                "SELECT numbering,season,episode,episode_end FROM media_entry_episodes ORDER BY ordinal",
            ).fetch_all(s.db.read_pool()).await.unwrap();
            assert_eq!(
                spans,
                vec![
                    ("season".into(), Some(1), 3, Some(4)),
                    ("absolute".into(), None, 123, None)
                ]
            );
            s.close().await;
        }
        let s = Store::open(&path).await.unwrap();
        let mut tx = s.db.begin().await.unwrap();
        sqlx::query("DELETE FROM media_entries WHERE id='entry'")
            .execute(&mut *tx)
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM media_entry_episodes")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(count, 0);
        tx.commit().await.unwrap();
        s.close().await;
    }

    #[tokio::test]
    async fn pending_migrations_apply_once_before_store_is_exposed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mediadb.db");
        let s = Store::create(&path).await.unwrap();
        s.put_mediahost("host", "Keep this host").await.unwrap();
        let library = s
            .create_library("Keep this library", MediaType::Movies, &[])
            .await
            .unwrap();
        s.close().await;
        let sql = "ALTER TABLE mediahosts ADD COLUMN migration_note TEXT NOT NULL DEFAULT 'preserved';
            CREATE TABLE migration_probe(value TEXT); INSERT INTO migration_probe VALUES('applied once');";
        for _ in 0..2 {
            let s = open(&path, upgrade(sql)).await.unwrap();
            assert_eq!(s.libraries().await.unwrap()[0].id, library);
            let host: (String, String) =
                sqlx::query_as("SELECT name,migration_note FROM mediahosts WHERE id='host'")
                    .fetch_one(s.db.read_pool())
                    .await
                    .unwrap();
            assert_eq!(host, ("Keep this host".into(), "preserved".into()));
            s.close().await;
        }
        let mut db = reader(&path).await;
        let history: Vec<(i64, bool)> =
            sqlx::query_as("SELECT version,success FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&mut db)
                .await
                .unwrap();
        let expected: Vec<_> = MIGRATOR
            .iter()
            .map(|m| (m.version, true))
            .chain([(upgrade_version(), true)])
            .collect();
        assert_eq!(history, expected);
        let rows: Vec<String> = sqlx::query_scalar("SELECT value FROM migration_probe")
            .fetch_all(&mut db)
            .await
            .unwrap();
        assert_eq!(rows, ["applied once"]);
    }

    #[tokio::test]
    async fn failed_migration_rolls_back_schema_and_history_and_can_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mediadb.db");
        let s = Store::create(&path).await.unwrap();
        s.put_mediahost("host", "Keep").await.unwrap();
        s.close().await;
        let sql = "CREATE TABLE migration_probe(value TEXT); INSERT INTO migration_probe SELECT value FROM upgrade_input;";
        let error = open(&path, upgrade(sql)).await.err().unwrap();
        assert!(matches!(
            error.downcast_ref::<MigrateError>(),
            Some(MigrateError::ExecuteMigration(_, version)) if *version == upgrade_version()
        ));
        let mut db = reader(&path).await;
        assert!(
            sqlx::query("SELECT value FROM migration_probe")
                .fetch_all(&mut db)
                .await
                .is_err()
        );
        let versions: Vec<i64> = sqlx::query_scalar("SELECT version FROM _sqlx_migrations")
            .fetch_all(&mut db)
            .await
            .unwrap();
        assert_eq!(
            versions,
            MIGRATOR.iter().map(|m| m.version).collect::<Vec<_>>()
        );
        db.close().await.unwrap();
        // Supply the missing test input; retry exactly the same migration SQL.
        let mut writer =
            sqlx::SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
                .await
                .unwrap();
        sqlx::raw_sql(
            "CREATE TABLE upgrade_input(value TEXT); INSERT INTO upgrade_input VALUES('ready')",
        )
        .execute(&mut writer)
        .await
        .unwrap();
        writer.close().await.unwrap();
        let s = open(&path, upgrade(sql)).await.unwrap();
        let value: String = sqlx::query_scalar("SELECT value FROM migration_probe")
            .fetch_one(s.db.read_pool())
            .await
            .unwrap();
        assert_eq!(value, "ready");
        let host: String = sqlx::query_scalar("SELECT name FROM mediahosts WHERE id='host'")
            .fetch_one(s.db.read_pool())
            .await
            .unwrap();
        assert_eq!(host, "Keep");
        s.close().await;
    }

    #[tokio::test]
    async fn newer_and_modified_histories_are_rejected_without_rewriting_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mediadb.db");
        Store::create(&path).await.unwrap().close().await;
        let sql = "CREATE TABLE migration_probe(value TEXT);";
        open(&path, upgrade(sql)).await.unwrap().close().await;
        let before = std::fs::read(&path).unwrap();
        let error = Store::open(&path).await.err().unwrap();
        assert!(matches!(
            error.downcast_ref::<MigrateError>(),
            Some(MigrateError::VersionMissing(version)) if *version == upgrade_version()
        ));
        assert_eq!(before, std::fs::read(&path).unwrap());
        let error = open(&path, upgrade("CREATE TABLE changed_probe(value TEXT);"))
            .await
            .err()
            .unwrap();
        assert!(matches!(
            error.downcast_ref::<MigrateError>(),
            Some(MigrateError::VersionMismatch(version)) if *version == upgrade_version()
        ));
        assert_eq!(before, std::fs::read(&path).unwrap());
        open(&path, upgrade(sql)).await.unwrap().close().await;
    }

    #[tokio::test]
    async fn a_hub_migration_history_and_legacy_or_missing_files_are_not_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let hub = dir.path().join("hub.db");
        let mut db = sqlx::SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&hub)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        let other = Migrator::with_migrations(vec![Migration::new(
            1,
            "hub users".into(),
            MigrationType::Simple,
            "CREATE TABLE users(name TEXT); INSERT INTO users VALUES('keep');".into_sql_str(),
            false,
        )]);
        other.run_direct(None, &mut db, false).await.unwrap();
        db.close().await.unwrap();
        let before = std::fs::read(&hub).unwrap();
        assert!(Store::open(&hub).await.is_err());
        assert!(Store::create(&hub).await.is_err());
        assert_eq!(before, std::fs::read(&hub).unwrap());
        let legacy = dir.path().join("legacy.db");
        let mut db = sqlx::SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&legacy)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        sqlx::raw_sql(
            "CREATE TABLE schema_version(version INTEGER); INSERT INTO schema_version VALUES(1)",
        )
        .execute(&mut db)
        .await
        .unwrap();
        db.close().await.unwrap();
        let before = std::fs::read(&legacy).unwrap();
        assert!(Store::open(&legacy).await.is_err());
        assert_eq!(before, std::fs::read(&legacy).unwrap());
        let missing = dir.path().join("missing.db");
        assert!(Store::open(&missing).await.is_err());
        assert!(!missing.exists());
    }
}
