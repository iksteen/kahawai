//! Durable source catalogue and protocol-4 replay log.
//!
//! The mediahost is authoritative for physical/source-derived facts. Each
//! collection owns an epoch and a monotonically increasing version. A write
//! changes the current entity row and its replay version in one SQLite
//! transaction; hubs persist only projections and acknowledge after commit.
//!
//! `catalog_records` is current-state-plus-tombstones, not an append-only log:
//! if a source changes repeatedly while a hub is offline, only the latest fact
//! needs to cross the wire. Protocol 4 initially retains tombstones
//! indefinitely; per-hub ACKs and the replay floor make later compaction safe.
//! A restored hub below that floor resets its projection and takes a live
//! snapshot, so compaction can never turn a deletion into a resurrected file.
//!
//! Discovery queue membership is materialized from missing current records,
//! which makes a crash retry work without a hub or a queue-repair pass.
//! `catalog_jobs` is the lease/error journal for work that later needs partial
//! progress. Cheap attachment/chapter, keyframe and geometry retries use the
//! same exact-revision claims as expensive work; retryable worker failures
//! release that running claim locally and never settle a fact by themselves.
//! Each claim stores the exact catalogue source version, so a worker result
//! cannot attach to replacement bytes that happen to retain size and mtime.
//! `catalog_meta` stores the analyzer generation that gives a derived record
//! meaning. Opening under a new generation invalidates that kind locally, so
//! an application upgrade schedules fresh results instead of treating an old
//! analyzer's answer as permanently complete.
//! `completed_generation` distinguishes a fresh/aborted first scan from a
//! catalogue with a complete prior manifest. Only the former delays offers;
//! ordinary process restarts can reconnect from SQLite while rescanning.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use kahawai_proto::v1::{
    CatalogCollection, CatalogRecord, CollectionRoot, DiscoveryStatus, FileError, FileRecord,
    HostToHub, SourcePath, host_to_hub,
};
use kahawai_sqlite::Database;
use prost::Message as _;
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};

#[derive(Debug)]
struct StaleFact(&'static str);

impl std::fmt::Display for StaleFact {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for StaleFact {}

pub(crate) fn is_stale_fact(error: &anyhow::Error) -> bool {
    error.downcast_ref::<StaleFact>().is_some()
}

use crate::scan::CollectionConfig;

pub(crate) type VersionState = HashMap<String, (String, u64, bool)>;

#[derive(Clone)]
pub struct Catalog {
    db: Database,
    versions: tokio::sync::watch::Sender<VersionState>,
}

#[derive(Debug, Clone)]
pub struct KnownFile {
    pub size: u64,
    pub mtime_unix: i64,
    pub streams_json: String,
}

#[derive(sqlx::FromRow)]
struct StoredFileRevision {
    size: i64,
    mtime_unix: i64,
    head_xxh3: i64,
    tail_xxh3: i64,
    oshash: i64,
    streams_json: String,
    error: String,
    version: i64,
}

#[derive(Debug, Clone)]
pub struct Delta {
    pub epoch: String,
    pub current_version: u64,
    pub oldest_replayable_version: u64,
    pub records: Vec<CatalogRecord>,
    pub done: bool,
}

impl Catalog {
    pub async fn open(state_dir: &Path, collections: &[CollectionConfig]) -> Result<Self> {
        Self::open_with_segment_detection(state_dir, collections, true).await
    }

    pub async fn open_with_segment_detection(
        state_dir: &Path,
        collections: &[CollectionConfig],
        detect_segments: bool,
    ) -> Result<Self> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("creating {}", state_dir.display()))?;
        let path = state_dir.join("catalog.db");
        let writer_options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(30));
        let reader_options = SqliteConnectOptions::new()
            .filename(&path)
            .read_only(true)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(30));
        // The former four-connection budget becomes three concurrent WAL
        // readers plus the one actor-owned writer connection.
        let db = Database::connect_with(writer_options, reader_options, 3)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        db.write("mediahost catalog migrations", |connection| {
            Box::pin(async move {
                sqlx::migrate!("./migrations")
                    .run_direct(None, connection, false)
                    .await
                    .context("migrating mediahost catalogue")
            })
        })
        .await?;
        sqlx::query("UPDATE catalog_jobs SET state='pending' WHERE state='running'")
            .execute(&db)
            .await?;
        Self::ensure_discovery_generation(
            &db,
            "file_loudness",
            &kahawai_media::loudness::ANALYZER.to_string(),
        )
        .await?;
        if detect_segments {
            Self::ensure_discovery_generation(
                &db,
                "file_segments",
                &kahawai_core::segments::DETECTOR_GENERATION.to_string(),
            )
            .await?;
        }

        let configured: HashSet<&str> = collections.iter().map(|c| c.name.as_str()).collect();
        let stored: Vec<String> =
            sqlx::query_scalar("SELECT id FROM catalog_collections WHERE retired=0")
                .fetch_all(&db)
                .await?;
        for id in stored
            .into_iter()
            .filter(|id| !configured.contains(id.as_str()))
        {
            sqlx::query("UPDATE catalog_collections SET retired=1 WHERE id=?")
                .bind(id)
                .execute(&db)
                .await?;
        }
        for collection in collections {
            let existing: Option<(String, String, i64)> = sqlx::query_as(
                "SELECT media_type,epoch,retired FROM catalog_collections WHERE id=?",
            )
            .bind(&collection.name)
            .fetch_optional(&db)
            .await?;
            match existing {
                Some((media_type, _, 0)) if media_type == collection.media_type => {}
                Some(_) => {
                    let mut tx = db.begin().await?;
                    sqlx::query("DELETE FROM catalog_collections WHERE id=?")
                        .bind(&collection.name)
                        .execute(&mut *tx)
                        .await?;
                    Self::insert_collection(&mut tx, collection).await?;
                    tx.commit().await?;
                }
                None => {
                    let mut tx = db.begin().await?;
                    Self::insert_collection(&mut tx, collection).await?;
                    tx.commit().await?;
                }
            }
        }
        // Every runtime opening this catalogue immediately starts one local
        // scan per configured collection. Publish that intent before any hub
        // supervisor can observe a seemingly complete version-zero catalogue.
        sqlx::query("UPDATE catalog_collections SET scanning=1 WHERE retired=0")
            .execute(&db)
            .await?;
        let initial_versions = Self::read_version_states(&db).await?;
        let (versions, _) = tokio::sync::watch::channel(initial_versions);
        Ok(Self { db, versions })
    }

    async fn ensure_discovery_generation(
        db: &Database,
        kind: &str,
        generation: &str,
    ) -> Result<()> {
        let stored: Option<String> =
            sqlx::query_scalar("SELECT value FROM catalog_meta WHERE key=?")
                .bind(kind)
                .fetch_optional(db)
                .await?;
        if stored.as_deref() == Some(generation) {
            return Ok(());
        }
        let mut tx = db.begin().await?;
        let records = sqlx::query(
            "SELECT collection_id,record_key FROM catalog_records
              WHERE kind=? AND deleted=0",
        )
        .bind(kind)
        .fetch_all(&mut *tx)
        .await?;
        for record in records {
            let collection: String = record.get("collection_id");
            let key: Vec<u8> = record.get("record_key");
            let version = Self::next_version(&mut tx, &collection).await?;
            Self::put_record(&mut tx, &collection, kind, &key, version, Vec::new(), true).await?;
        }
        sqlx::query("DELETE FROM catalog_jobs WHERE kind=?")
            .bind(kind)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO catalog_meta(key,value) VALUES(?,?)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        )
        .bind(kind)
        .bind(generation)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        if let Some(stored) = stored {
            tracing::info!(
                kind,
                from = stored,
                to = generation,
                "local discovery generation changed; source facts scheduled again"
            );
        }
        Ok(())
    }

    async fn insert_collection(
        tx: &mut sqlx::SqliteConnection,
        collection: &CollectionConfig,
    ) -> Result<()> {
        sqlx::query("INSERT INTO catalog_collections(id,media_type,epoch) VALUES(?,?,?)")
            .bind(&collection.name)
            .bind(&collection.media_type)
            .bind(ulid::Ulid::generate().to_string())
            .execute(&mut *tx)
            .await?;
        Ok(())
    }

    pub fn read_pool(&self) -> &sqlx::SqlitePool {
        self.db.read_pool()
    }

    pub async fn offers(&self, collections: &[CollectionConfig]) -> Result<Vec<CatalogCollection>> {
        let mut offers = Vec::with_capacity(collections.len());
        for collection in collections {
            let row = sqlx::query(
                "SELECT epoch,current_version,oldest_replayable_version,scanning
                   FROM catalog_collections WHERE id=? AND retired=0",
            )
            .bind(&collection.name)
            .fetch_one(&self.db)
            .await?;
            offers.push(CatalogCollection {
                id: collection.name.clone(),
                media_type: collection.media_type.clone(),
                roots: collection
                    .resolved_roots()
                    .map(|root| CollectionRoot {
                        root_token: root.token,
                        normalized_path: root.path.to_string_lossy().into_owned(),
                    })
                    .collect(),
                epoch: row.get("epoch"),
                current_version: row.get::<i64, _>("current_version") as u64,
                oldest_replayable_version: row.get::<i64, _>("oldest_replayable_version") as u64,
                scanning: row.get::<i64, _>("scanning") != 0,
            });
        }
        Ok(offers)
    }

    async fn read_version_states(db: &Database) -> Result<VersionState> {
        let rows = sqlx::query(
            "SELECT id,epoch,current_version,completed_generation
               FROM catalog_collections WHERE retired=0",
        )
        .fetch_all(db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    row.get("id"),
                    (
                        row.get("epoch"),
                        row.get::<i64, _>("current_version") as u64,
                        row.get::<i64, _>("completed_generation") > 0,
                    ),
                )
            })
            .collect())
    }

    pub async fn version_states(&self) -> Result<VersionState> {
        Self::read_version_states(&self.db).await
    }

    pub(crate) fn subscribe_versions(&self) -> tokio::sync::watch::Receiver<VersionState> {
        self.versions.subscribe()
    }

    /// Publish only committed catalogue state. Versions merge monotonically so
    /// tasks resuming out of commit order cannot make a connected hub overlook
    /// a newer transaction. The epoch cannot change while this catalogue is
    /// open; collection recreation happens during `open` before publication.
    fn publish_version(&self, collection: &str, version: u64, complete: bool) {
        self.versions.send_if_modified(|versions| {
            let Some((_, current, completed)) = versions.get_mut(collection) else {
                tracing::error!(
                    collection,
                    version,
                    "committed unknown catalogue collection"
                );
                return false;
            };
            let next = (*current).max(version);
            let next_completed = *completed || complete;
            let changed = next != *current || next_completed != *completed;
            *current = next;
            *completed = next_completed;
            changed
        });
    }

    pub async fn segment_scan_state(&self, collection: &str) -> Result<(i64, bool)> {
        let (generation, scanning): (i64, i64) = sqlx::query_as(
            "SELECT completed_generation,scanning
               FROM catalog_collections WHERE id=? AND retired=0",
        )
        .bind(collection)
        .fetch_one(&self.db)
        .await?;
        Ok((generation, scanning != 0))
    }

    pub async fn known_files(
        &self,
        collection: &str,
    ) -> Result<HashMap<(String, String), KnownFile>> {
        let rows = sqlx::query(
            "SELECT root_token,path_rel,size,mtime_unix,streams_json
               FROM catalog_files WHERE collection_id=? AND error=''",
        )
        .bind(collection)
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    (row.get("root_token"), row.get("path_rel")),
                    KnownFile {
                        size: row.get::<i64, _>("size") as u64,
                        mtime_unix: row.get("mtime_unix"),
                        streams_json: row.get("streams_json"),
                    },
                )
            })
            .collect())
    }

    pub async fn begin_scan(&self, collection: &str) -> Result<i64> {
        let generation: i64 = sqlx::query_scalar(
            "UPDATE catalog_collections
                SET scan_generation=scan_generation+1,scanning=1,scanned=0,failed=0,skipped=0
              WHERE id=? RETURNING scan_generation",
        )
        .bind(collection)
        .fetch_one(&self.db)
        .await?;
        Ok(generation)
    }

    pub async fn scan_progress(
        &self,
        collection: &str,
        scanned: u32,
        failed: u32,
        skipped: u32,
    ) -> Result<()> {
        sqlx::query("UPDATE catalog_collections SET scanned=?,failed=?,skipped=? WHERE id=?")
            .bind(scanned as i64)
            .bind(failed as i64)
            .bind(skipped as i64)
            .bind(collection)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// A live, local view of work ownership. Counts are source counts rather
    /// than hub queue rows: the same missing answer is computed once even when
    /// several hubs subscribe to the collection.
    pub async fn discovery_status(&self, collection: &str) -> Result<DiscoveryStatus> {
        let row = sqlx::query(
            "SELECT media_type,scanning,scanned,failed,skipped
               FROM catalog_collections WHERE id=?",
        )
        .bind(collection)
        .fetch_one(&self.db)
        .await?;
        let media_type: String = row.get("media_type");
        let pending = |kind: &'static str| async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM catalog_files f
                   WHERE f.collection_id=? AND f.error=''
                     AND (? != 'file_loudness'
                          OR COALESCE(json_array_length(f.streams_json,'$.audio'),0)>0)
                     AND NOT EXISTS (
                       SELECT 1 FROM catalog_records r
                        WHERE r.collection_id=f.collection_id AND r.kind=?
                          AND r.record_key=CAST(f.root_token || char(0) || f.path_rel AS BLOB)
                          AND r.deleted=0)",
            )
            .bind(collection)
            .bind(kind)
            .bind(kind)
            .fetch_one(&self.db)
            .await
        };
        let hashes = if media_type == "anime" {
            pending("file_hashes").await?
        } else {
            0
        };
        let segments = if matches!(media_type.as_str(), "series" | "anime") {
            pending("file_segments").await?
        } else {
            0
        };
        let loudness = if media_type != "music" {
            pending("file_loudness").await?
        } else {
            0
        };
        let cheap: i64 = sqlx::query_scalar(
            "SELECT
               COALESCE(SUM(CASE WHEN
                    json_extract(f.streams_json,'$.container') IN ('matroska','webm')
                    AND c.media_type IN ('movies','series','anime')
                    AND (json_extract(f.streams_json,'$.attachments') IS NULL
                         OR json_extract(f.streams_json,'$.chapters') IS NULL)
                    AND NOT EXISTS (
                      SELECT 1 FROM catalog_records r
                       WHERE r.collection_id=f.collection_id AND r.kind='file_attachments'
                         AND r.record_key=CAST(f.root_token || char(0) || f.path_rel AS BLOB)
                         AND r.deleted=0)
                   THEN 1 ELSE 0 END),0)
               + COALESCE(SUM(CASE WHEN
                    json_extract(f.streams_json,'$.video[0].codec') IS NOT NULL
                    AND json_extract(f.streams_json,'$.video[0].max_keyframe_interval_ms') IS NULL
                    AND NOT EXISTS (
                      SELECT 1 FROM catalog_records r
                       WHERE r.collection_id=f.collection_id AND r.kind='file_keyframe'
                         AND r.record_key=CAST(f.root_token || char(0) || f.path_rel AS BLOB)
                         AND r.deleted=0)
                   THEN 1 ELSE 0 END),0)
               + COALESCE(SUM(CASE WHEN
                    json_extract(f.streams_json,'$.video[0].codec') IS NOT NULL
                    AND COALESCE(json_extract(f.streams_json,'$.video_geometry_probed'),0)=0
                    AND NOT EXISTS (
                      SELECT 1 FROM catalog_records r
                       WHERE r.collection_id=f.collection_id AND r.kind='file_geometry'
                         AND r.record_key=CAST(f.root_token || char(0) || f.path_rel AS BLOB)
                         AND r.deleted=0)
                   THEN 1 ELSE 0 END),0)
               FROM catalog_files f
               JOIN catalog_collections c ON c.id=f.collection_id
              WHERE f.collection_id=? AND f.error=''",
        )
        .bind(collection)
        .fetch_one(&self.db)
        .await?;
        Ok(DiscoveryStatus {
            collection_id: collection.to_string(),
            scanning: row.get::<i64, _>("scanning") != 0,
            scanned: row.get::<i64, _>("scanned") as u32,
            failed: row.get::<i64, _>("failed") as u32,
            skipped: row.get::<i64, _>("skipped") as u32,
            pending_cheap: cheap as u64,
            pending_hashes: hashes as u64,
            pending_segments: segments as u64,
            pending_loudness: loudness as u64,
        })
    }

    pub async fn mark_seen(
        &self,
        collection: &str,
        root_token: &str,
        path_rel: &str,
        generation: i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE catalog_files SET seen_generation=?
              WHERE collection_id=? AND root_token=? AND path_rel=?",
        )
        .bind(generation)
        .bind(collection)
        .bind(root_token)
        .bind(path_rel)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn mark_seen_batch(
        &self,
        collection: &str,
        sources: &[(String, String)],
        generation: i64,
    ) -> Result<()> {
        if sources.is_empty() {
            return Ok(());
        }
        let mut tx = self.db.begin().await?;
        for (root_token, path_rel) in sources {
            sqlx::query(
                "UPDATE catalog_files SET seen_generation=?
                  WHERE collection_id=? AND root_token=? AND path_rel=?",
            )
            .bind(generation)
            .bind(collection)
            .bind(root_token)
            .bind(path_rel)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn upsert_file(
        &self,
        collection: &str,
        file: &FileRecord,
        generation: i64,
    ) -> Result<Option<u64>> {
        let source = file
            .source
            .as_ref()
            .context("catalogue file missing source")?;
        let previous: Option<StoredFileRevision> = sqlx::query_as(
            "SELECT size,mtime_unix,head_xxh3,tail_xxh3,oshash,streams_json,error,version
               FROM catalog_files
              WHERE collection_id=? AND root_token=? AND path_rel=?",
        )
        .bind(collection)
        .bind(&source.root_token)
        .bind(&source.path_rel)
        .fetch_optional(&self.db)
        .await?;
        let unchanged = previous.as_ref().is_some_and(|stored| {
            stored.error.is_empty()
                && stored.size == file.size as i64
                && stored.mtime_unix == file.mtime_unix
                && stored.head_xxh3 == file.head_xxh3 as i64
                && stored.tail_xxh3 == file.tail_xxh3 as i64
                && stored.oshash == file.oshash as i64
                && stored.streams_json == file.streams_json
        });
        if unchanged {
            self.mark_seen(collection, &source.root_token, &source.path_rel, generation)
                .await?;
            return Ok(None);
        }
        let revision_changed = previous.as_ref().is_none_or(|stored| {
            !stored.error.is_empty()
                || stored.size != file.size as i64
                || stored.mtime_unix != file.mtime_unix
                || stored.head_xxh3 != file.head_xxh3 as i64
                || stored.tail_xxh3 != file.tail_xxh3 as i64
                || stored.oshash != file.oshash as i64
        });
        let mut tx = self.db.begin().await?;
        let version = Self::next_version(&mut tx, collection).await?;
        sqlx::query(
            "INSERT INTO catalog_files
               (collection_id,root_token,path_rel,size,mtime_unix,head_xxh3,tail_xxh3,
                oshash,streams_json,seen_generation,version,error)
             VALUES(?,?,?,?,?,?,?,?,?,?,?,'')
             ON CONFLICT(collection_id,root_token,path_rel) DO UPDATE SET
               size=excluded.size,mtime_unix=excluded.mtime_unix,
               head_xxh3=excluded.head_xxh3,tail_xxh3=excluded.tail_xxh3,
               oshash=excluded.oshash,streams_json=excluded.streams_json,
               seen_generation=excluded.seen_generation,version=excluded.version,error=''",
        )
        .bind(collection)
        .bind(&source.root_token)
        .bind(&source.path_rel)
        .bind(file.size as i64)
        .bind(file.mtime_unix)
        .bind(file.head_xxh3 as i64)
        .bind(file.tail_xxh3 as i64)
        .bind(file.oshash as i64)
        .bind(&file.streams_json)
        .bind(generation)
        .bind(version as i64)
        .execute(&mut *tx)
        .await?;
        let payload = kahawai_proto::v1::FileUpsert {
            collection_id: collection.to_string(),
            files: vec![file.clone()],
        }
        .encode_to_vec();
        Self::put_record(
            &mut tx,
            collection,
            "file",
            &source_key(&source.root_token, &source.path_rel),
            version,
            payload,
            false,
        )
        .await?;
        let derived_version = if revision_changed {
            // Expensive derived answers belong to the media bytes, not to
            // sidecar/catalogue presentation. Preserve them when only NFO,
            // artwork or external-subtitle metadata changes; replace them
            // when the guarded content identity changes.
            let derived_version = Self::tombstone_derived_records(
                &mut tx,
                collection,
                &source_key(&source.root_token, &source.path_rel),
            )
            .await?;
            sqlx::query(
                "DELETE FROM catalog_jobs
                  WHERE collection_id=? AND root_token=? AND path_rel=? AND state!='running'",
            )
            .bind(collection)
            .bind(&source.root_token)
            .bind(&source.path_rel)
            .execute(&mut *tx)
            .await?;
            derived_version
        } else {
            if let Some(stored) = previous.as_ref() {
                sqlx::query(
                    "UPDATE catalog_jobs SET source_version=?,updated_at=unixepoch()
                      WHERE collection_id=? AND root_token=? AND path_rel=?
                        AND state='running' AND source_version=?",
                )
                .bind(version as i64)
                .bind(collection)
                .bind(&source.root_token)
                .bind(&source.path_rel)
                .bind(stored.version)
                .execute(&mut *tx)
                .await?;
            }
            // A sidecar/NFO-only FileUpsert replaces the hub's complete
            // streams_json. Re-version every retained source fact after that
            // file row so attachment/keyframe/geometry backfills are replayed
            // in order instead of being silently overwritten there. Expensive
            // facts are small and remain bound to the unchanged bytes.
            Self::reversion_derived_records(
                &mut tx,
                collection,
                &source_key(&source.root_token, &source.path_rel),
            )
            .await?
        };
        let published_version = derived_version.unwrap_or(version);
        tx.commit().await?;
        self.publish_version(collection, published_version, false);
        Ok(Some(version))
    }

    pub async fn record_error(
        &self,
        collection: &str,
        error: &FileError,
        generation: i64,
    ) -> Result<u64> {
        let source = error
            .source
            .as_ref()
            .context("catalogue error missing source")?;
        let previous: Option<(String, i64)> = sqlx::query_as(
            "SELECT error,version FROM catalog_files
              WHERE collection_id=? AND root_token=? AND path_rel=?",
        )
        .bind(collection)
        .bind(&source.root_token)
        .bind(&source.path_rel)
        .fetch_optional(&self.db)
        .await?;
        if let Some((previous_error, version)) = previous.as_ref()
            && previous_error == &error.error
        {
            self.mark_seen(collection, &source.root_token, &source.path_rel, generation)
                .await?;
            return Ok(*version as u64);
        }
        let mut tx = self.db.begin().await?;
        let key = source_key(&source.root_token, &source.path_rel);
        // Protocol 3 omitted a failed inspection from FilesSeen, so the hub
        // reconciled a formerly valid source away after logging FileError.
        // Preserve that availability behavior now that the mediahost owns the
        // manifest: the diagnostic is live catalogue state, but stale bytes
        // must not remain playable.
        let had_live_file = previous
            .as_ref()
            .is_some_and(|(previous_error, _)| previous_error.is_empty());
        if had_live_file {
            let removed_version = Self::next_version(&mut tx, collection).await?;
            Self::put_record(
                &mut tx,
                collection,
                "file",
                &key,
                removed_version,
                Vec::new(),
                true,
            )
            .await?;
            Self::tombstone_derived_records(&mut tx, collection, &key).await?;
            sqlx::query(
                "DELETE FROM catalog_jobs
                  WHERE collection_id=? AND root_token=? AND path_rel=? AND state!='running'",
            )
            .bind(collection)
            .bind(&source.root_token)
            .bind(&source.path_rel)
            .execute(&mut *tx)
            .await?;
        }
        let version = Self::next_version(&mut tx, collection).await?;
        sqlx::query(
            "INSERT INTO catalog_files
               (collection_id,root_token,path_rel,size,mtime_unix,head_xxh3,tail_xxh3,
                oshash,streams_json,seen_generation,version,error)
             VALUES(?,?,?,0,0,0,0,0,'{}',?,?,?)
             ON CONFLICT(collection_id,root_token,path_rel) DO UPDATE SET
               seen_generation=excluded.seen_generation,version=excluded.version,
               error=excluded.error",
        )
        .bind(collection)
        .bind(&source.root_token)
        .bind(&source.path_rel)
        .bind(generation)
        .bind(version as i64)
        .bind(&error.error)
        .execute(&mut *tx)
        .await?;
        Self::put_record(
            &mut tx,
            collection,
            "file_error",
            &key,
            version,
            error.encode_to_vec(),
            false,
        )
        .await?;
        tx.commit().await?;
        self.publish_version(collection, version, false);
        Ok(version)
    }

    pub async fn finish_scan(
        &self,
        collection: &str,
        generation: i64,
        unavailable_roots: &HashSet<String>,
    ) -> Result<u64> {
        let stale = sqlx::query(
            "SELECT root_token,path_rel FROM catalog_files
              WHERE collection_id=? AND seen_generation!=?",
        )
        .bind(collection)
        .bind(generation)
        .fetch_all(&self.db)
        .await?;
        let mut tx = self.db.begin().await?;
        for row in stale {
            let root: String = row.get("root_token");
            if unavailable_roots.contains(&root) {
                continue;
            }
            let path: String = row.get("path_rel");
            let key = source_key(&root, &path);
            Self::tombstone_derived_records(&mut tx, collection, &key).await?;
            let version = Self::next_version(&mut tx, collection).await?;
            sqlx::query(
                "DELETE FROM catalog_files WHERE collection_id=? AND root_token=? AND path_rel=?",
            )
            .bind(collection)
            .bind(&root)
            .bind(&path)
            .execute(&mut *tx)
            .await?;
            Self::put_record(&mut tx, collection, "file", &key, version, Vec::new(), true).await?;
        }
        sqlx::query(
            "UPDATE catalog_collections
                SET scanning=0,completed_generation=? WHERE id=?",
        )
        .bind(generation)
        .bind(collection)
        .execute(&mut *tx)
        .await?;
        let current: i64 =
            sqlx::query_scalar("SELECT current_version FROM catalog_collections WHERE id=?")
                .bind(collection)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        self.publish_version(collection, current as u64, true);
        Ok(current as u64)
    }

    async fn next_version(tx: &mut sqlx::SqliteConnection, collection: &str) -> Result<u64> {
        let version: i64 = sqlx::query_scalar(
            "UPDATE catalog_collections SET current_version=current_version+1
              WHERE id=? RETURNING current_version",
        )
        .bind(collection)
        .fetch_one(&mut *tx)
        .await?;
        Ok(version as u64)
    }

    async fn put_record(
        tx: &mut sqlx::SqliteConnection,
        collection: &str,
        kind: &str,
        key: &[u8],
        version: u64,
        payload: Vec<u8>,
        deleted: bool,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO catalog_records(collection_id,kind,record_key,version,payload,deleted)
             VALUES(?,?,?,?,?,?)
             ON CONFLICT(collection_id,kind,record_key) DO UPDATE SET
               version=excluded.version,payload=excluded.payload,deleted=excluded.deleted",
        )
        .bind(collection)
        .bind(kind)
        .bind(key)
        .bind(version as i64)
        .bind(payload)
        .bind(deleted)
        .execute(&mut *tx)
        .await?;
        Ok(())
    }

    async fn tombstone_derived_records(
        tx: &mut sqlx::SqliteConnection,
        collection: &str,
        key: &[u8],
    ) -> Result<Option<u64>> {
        let kinds: Vec<String> = sqlx::query_scalar(
            "SELECT kind FROM catalog_records
              WHERE collection_id=? AND record_key=? AND kind!='file' AND deleted=0",
        )
        .bind(collection)
        .bind(key)
        .fetch_all(&mut *tx)
        .await?;
        let mut last = None;
        for kind in kinds {
            let version = Self::next_version(tx, collection).await?;
            Self::put_record(tx, collection, &kind, key, version, Vec::new(), true).await?;
            last = Some(version);
        }
        Ok(last)
    }

    async fn reversion_derived_records(
        tx: &mut sqlx::SqliteConnection,
        collection: &str,
        key: &[u8],
    ) -> Result<Option<u64>> {
        let records = sqlx::query(
            "SELECT kind,payload FROM catalog_records
              WHERE collection_id=? AND record_key=? AND kind!='file' AND deleted=0",
        )
        .bind(collection)
        .bind(key)
        .fetch_all(&mut *tx)
        .await?;
        let mut last = None;
        for record in records {
            let kind: String = record.get("kind");
            let payload: Vec<u8> = record.get("payload");
            let version = Self::next_version(tx, collection).await?;
            Self::put_record(tx, collection, &kind, key, version, payload, false).await?;
            last = Some(version);
        }
        Ok(last)
    }

    pub async fn delta(&self, collection: &str, cursor: u64, snapshot: bool) -> Result<Delta> {
        let mut pages = self.delta_pages(collection, cursor, snapshot).await?;
        let mut combined = None;
        while let Some(page) = pages.recv().await {
            let page = page?;
            let delta = combined.get_or_insert_with(|| Delta {
                epoch: page.epoch.clone(),
                current_version: page.current_version,
                oldest_replayable_version: page.oldest_replayable_version,
                records: Vec::new(),
                done: false,
            });
            delta.records.extend(page.records);
            delta.done = page.done;
        }
        combined.context("catalogue delta producer stopped before its first page")
    }

    /// Produce a consistent delta in bounded pages. The read transaction is
    /// held across pages: scans keep writing through WAL, while a 100k-file
    /// snapshot never materializes every protobuf payload in process memory.
    pub async fn delta_pages(
        &self,
        collection: &str,
        cursor: u64,
        snapshot: bool,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<Delta>>> {
        const RECORDS_PER_PAGE: usize = 256;
        let mut transaction = self.db.begin().await?;
        let row = sqlx::query(
            "SELECT epoch,current_version,oldest_replayable_version
               FROM catalog_collections WHERE id=? AND retired=0",
        )
        .bind(collection)
        .fetch_one(&mut *transaction)
        .await?;
        let epoch: String = row.get("epoch");
        let current_version = row.get::<i64, _>("current_version") as u64;
        let oldest_replayable_version = row.get::<i64, _>("oldest_replayable_version") as u64;
        anyhow::ensure!(
            snapshot || (cursor >= oldest_replayable_version && cursor <= current_version),
            "catalogue cursor {cursor} is outside replayable range {oldest_replayable_version}..={current_version}"
        );
        let collection = collection.to_string();
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            let produce = async {
                let mut after = if snapshot { 0 } else { cursor };
                let mut snapshot_files = snapshot;
                loop {
                    let was_snapshot_files = snapshot_files;
                    let mut rows = if snapshot {
                        if snapshot_files {
                            sqlx::query(
                                "SELECT kind,record_key,version,payload,deleted FROM catalog_records
                                  WHERE collection_id=? AND deleted=0 AND kind='file' AND version>?
                                  ORDER BY version,record_key LIMIT ?",
                            )
                            .bind(&collection)
                            .bind(after as i64)
                            .bind((RECORDS_PER_PAGE + 1) as i64)
                            .fetch_all(&mut *transaction)
                            .await?
                        } else {
                            sqlx::query(
                                "SELECT kind,record_key,version,payload,deleted FROM catalog_records
                                  WHERE collection_id=? AND deleted=0 AND kind!='file' AND version>?
                                  ORDER BY version,kind,record_key LIMIT ?",
                            )
                            .bind(&collection)
                            .bind(after as i64)
                            .bind((RECORDS_PER_PAGE + 1) as i64)
                            .fetch_all(&mut *transaction)
                            .await?
                        }
                    } else {
                        sqlx::query(
                            "SELECT kind,record_key,version,payload,deleted FROM catalog_records
                              WHERE collection_id=? AND version>?
                              ORDER BY version,kind,record_key LIMIT ?",
                        )
                        .bind(&collection)
                        .bind(after as i64)
                        .bind((RECORDS_PER_PAGE + 1) as i64)
                        .fetch_all(&mut *transaction)
                        .await?
                    };
                    if snapshot && snapshot_files && rows.is_empty() {
                        snapshot_files = false;
                        after = 0;
                        continue;
                    }
                    let more = rows.len() > RECORDS_PER_PAGE;
                    rows.truncate(RECORDS_PER_PAGE);
                    let records: Vec<CatalogRecord> = rows
                        .into_iter()
                        .map(|row| CatalogRecord {
                            version: row.get::<i64, _>("version") as u64,
                            kind: row.get("kind"),
                            key: row.get("record_key"),
                            payload: row.get("payload"),
                            deleted: row.get::<i64, _>("deleted") != 0,
                        })
                        .collect();
                    if more {
                        after = records.last().map_or(after, |record| record.version);
                    } else if snapshot && snapshot_files {
                        snapshot_files = false;
                        after = 0;
                    }
                    let done = !more && (!snapshot || !was_snapshot_files);
                    if sender
                        .send(Ok(Delta {
                            epoch: epoch.clone(),
                            current_version,
                            oldest_replayable_version,
                            records,
                            done,
                        }))
                        .await
                        .is_err()
                    {
                        return Ok::<_, anyhow::Error>(());
                    }
                    if done {
                        transaction.commit().await?;
                        return Ok(());
                    }
                }
            }
            .await;
            if let Err(error) = produce {
                let _ = sender.send(Err(error)).await;
            }
        });
        Ok(receiver)
    }

    pub async fn acknowledge(
        &self,
        hub_id: &str,
        collection: &str,
        epoch: &str,
        version: u64,
    ) -> Result<()> {
        let current: (String, i64) =
            sqlx::query_as("SELECT epoch,current_version FROM catalog_collections WHERE id=?")
                .bind(collection)
                .fetch_one(&self.db)
                .await?;
        anyhow::ensure!(current.0 == epoch, "catalogue ACK has stale epoch");
        anyhow::ensure!(
            version <= current.1 as u64,
            "catalogue ACK is ahead of source"
        );
        sqlx::query(
            "INSERT INTO catalog_hub_acks(hub_id,collection_id,epoch,version) VALUES(?,?,?,?)
             ON CONFLICT(hub_id,collection_id) DO UPDATE SET
               version=CASE WHEN catalog_hub_acks.epoch=excluded.epoch
                            THEN MAX(catalog_hub_acks.version,excluded.version)
                            ELSE excluded.version END,
               epoch=excluded.epoch",
        )
        .bind(hub_id)
        .bind(collection)
        .bind(epoch)
        .bind(version as i64)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// Sources still missing one locally owned derived fact. A record carrying
    /// a terminal analyzer error counts as settled for that exact revision;
    /// replacing the file removes it in `upsert_file`.
    pub async fn pending_sources(
        &self,
        collection: &str,
        kind: &str,
        limit: usize,
    ) -> Result<Vec<SourcePath>> {
        let rows = sqlx::query(
            "SELECT f.root_token,f.path_rel FROM catalog_files f
              WHERE f.collection_id=? AND f.error=''
                AND (? != 'file_loudness'
                     OR COALESCE(json_array_length(f.streams_json,'$.audio'),0)>0)
                AND NOT EXISTS (
                    SELECT 1 FROM catalog_records r
                     WHERE r.collection_id=f.collection_id AND r.kind=?
                       AND r.record_key=CAST(f.root_token || char(0) || f.path_rel AS BLOB)
                       AND r.deleted=0)
              ORDER BY f.mtime_unix DESC,f.root_token,f.path_rel LIMIT ?",
        )
        .bind(collection)
        .bind(kind)
        .bind(kind)
        .bind(limit as i64)
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| SourcePath {
                root_token: row.get("root_token"),
                path_rel: row.get("path_rel"),
            })
            .collect())
    }

    /// Lease missing single-source work to one local runner. A lease is
    /// durable so a 40-minute decode cannot be re-enqueued every scheduler
    /// tick; process restart turns `running` back into claimable `pending`.
    pub async fn claim_sources(
        &self,
        collection: &str,
        kind: &str,
        limit: usize,
    ) -> Result<Vec<SourcePath>> {
        let mut tx = self.db.begin().await?;
        let running: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM catalog_jobs
              WHERE collection_id=? AND kind=? AND state='running'",
        )
        .bind(collection)
        .bind(kind)
        .fetch_one(&mut *tx)
        .await?;
        let available = limit.saturating_sub(running as usize);
        if available == 0 {
            tx.commit().await?;
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT f.root_token,f.path_rel,f.size,f.mtime_unix,f.version
               FROM catalog_files f
              WHERE f.collection_id=? AND f.error=''
                AND (? != 'file_loudness'
                     OR COALESCE(json_array_length(f.streams_json,'$.audio'),0)>0)
                AND NOT EXISTS (
                    SELECT 1 FROM catalog_records r
                     WHERE r.collection_id=f.collection_id AND r.kind=?
                       AND r.record_key=CAST(f.root_token || char(0) || f.path_rel AS BLOB)
                       AND r.deleted=0)
                AND NOT EXISTS (
                    SELECT 1 FROM catalog_jobs j
                     WHERE j.collection_id=f.collection_id AND j.kind=?
                       AND j.root_token=f.root_token AND j.path_rel=f.path_rel
                       AND j.state='running')
              ORDER BY f.mtime_unix DESC,f.root_token,f.path_rel LIMIT ?",
        )
        .bind(collection)
        .bind(kind)
        .bind(kind)
        .bind(kind)
        .bind(available as i64)
        .fetch_all(&mut *tx)
        .await?;
        let mut sources = Vec::with_capacity(rows.len());
        for row in rows {
            let root: String = row.get("root_token");
            let path: String = row.get("path_rel");
            let key = source_key(&root, &path);
            sqlx::query(
                "INSERT INTO catalog_jobs
                   (collection_id,kind,job_key,root_token,path_rel,size,mtime_unix,
                    source_version,state,updated_at)
                 VALUES(?,?,?,?,?,?,?,?,'running',unixepoch())
                 ON CONFLICT(collection_id,kind,job_key) DO UPDATE SET
                   root_token=excluded.root_token,path_rel=excluded.path_rel,
                   size=excluded.size,mtime_unix=excluded.mtime_unix,
                   source_version=excluded.source_version,
                   state='running',error='',updated_at=unixepoch()",
            )
            .bind(collection)
            .bind(kind)
            .bind(key)
            .bind(&root)
            .bind(&path)
            .bind(row.get::<i64, _>("size"))
            .bind(row.get::<i64, _>("mtime_unix"))
            .bind(row.get::<i64, _>("version"))
            .execute(&mut *tx)
            .await?;
            sources.push(SourcePath {
                root_token: root,
                path_rel: path,
            });
        }
        tx.commit().await?;
        Ok(sources)
    }

    /// Claim bounded source-local retries for facts that are normally filled
    /// during discovery. These predicates distinguish "not yet measured" from
    /// a live dedicated result (including measured-unknown keyframe state).
    pub async fn claim_cheap_sources(
        &self,
        collection: &str,
        kind: &str,
        limit: usize,
    ) -> Result<Vec<SourcePath>> {
        anyhow::ensure!(
            matches!(kind, "file_attachments" | "file_keyframe" | "file_geometry"),
            "unknown cheap catalogue fact kind {kind:?}"
        );
        let mut tx = self.db.begin().await?;
        // A worker clears its claim by storing a result or explicitly reports
        // a retryable local failure. Never time out and overwrite a running
        // claim: the old result could otherwise attach to the replacement
        // claim for new bytes that retained the same path and stat stamp.
        let running: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM catalog_jobs
              WHERE collection_id=? AND kind=? AND state='running'",
        )
        .bind(collection)
        .bind(kind)
        .fetch_one(&mut *tx)
        .await?;
        let available = limit.saturating_sub(running as usize);
        if available == 0 {
            tx.commit().await?;
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT f.root_token,f.path_rel,f.size,f.mtime_unix,f.version
               FROM catalog_files f JOIN catalog_collections c ON c.id=f.collection_id
              WHERE f.collection_id=? AND f.error=''
                AND ((?='file_attachments'
                      AND json_extract(f.streams_json,'$.container') IN ('matroska','webm')
                      AND (json_extract(f.streams_json,'$.attachments') IS NULL
                           OR json_extract(f.streams_json,'$.chapters') IS NULL))
                  OR (?='file_keyframe'
                      AND json_extract(f.streams_json,'$.video[0].codec') IS NOT NULL
                      AND json_extract(f.streams_json,
                                       '$.video[0].max_keyframe_interval_ms') IS NULL)
                  OR (?='file_geometry'
                      AND json_extract(f.streams_json,'$.video[0].codec') IS NOT NULL
                      AND COALESCE(json_extract(f.streams_json,
                                                '$.video_geometry_probed'),0)=0))
                AND (? != 'file_attachments'
                     OR c.media_type IN ('movies','series','anime'))
                AND NOT EXISTS (
                    SELECT 1 FROM catalog_records r
                     WHERE r.collection_id=f.collection_id AND r.kind=?
                       AND r.record_key=CAST(f.root_token || char(0) || f.path_rel AS BLOB)
                       AND r.deleted=0)
                AND NOT EXISTS (
                    SELECT 1 FROM catalog_jobs j
                     WHERE j.collection_id=f.collection_id AND j.kind=?
                       AND j.root_token=f.root_token AND j.path_rel=f.path_rel
                       AND j.state='running')
              ORDER BY f.mtime_unix DESC,f.root_token,f.path_rel LIMIT ?",
        )
        .bind(collection)
        .bind(kind)
        .bind(kind)
        .bind(kind)
        .bind(kind)
        .bind(kind)
        .bind(kind)
        .bind(available as i64)
        .fetch_all(&mut *tx)
        .await?;
        let mut sources = Vec::with_capacity(rows.len());
        for row in rows {
            let root: String = row.get("root_token");
            let path: String = row.get("path_rel");
            sqlx::query(
                "INSERT INTO catalog_jobs
                   (collection_id,kind,job_key,root_token,path_rel,size,mtime_unix,
                    source_version,state,updated_at)
                 VALUES(?,?,?,?,?,?,?,?,'running',unixepoch())
                 ON CONFLICT(collection_id,kind,job_key) DO UPDATE SET
                   root_token=excluded.root_token,path_rel=excluded.path_rel,
                   size=excluded.size,mtime_unix=excluded.mtime_unix,
                   source_version=excluded.source_version,
                   state='running',error='',updated_at=unixepoch()",
            )
            .bind(collection)
            .bind(kind)
            .bind(source_key(&root, &path))
            .bind(&root)
            .bind(&path)
            .bind(row.get::<i64, _>("size"))
            .bind(row.get::<i64, _>("mtime_unix"))
            .bind(row.get::<i64, _>("version"))
            .execute(&mut *tx)
            .await?;
            sources.push(SourcePath {
                root_token: root,
                path_rel: path,
            });
        }
        tx.commit().await?;
        Ok(sources)
    }

    pub async fn release_claims(
        &self,
        collection: &str,
        kind: &str,
        sources: &[SourcePath],
    ) -> Result<()> {
        let mut tx = self.db.begin().await?;
        for source in sources {
            sqlx::query(
                "DELETE FROM catalog_jobs
                  WHERE collection_id=? AND kind=? AND job_key=? AND state='running'",
            )
            .bind(collection)
            .bind(kind)
            .bind(source_key(&source.root_token, &source.path_rel))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Choose one locally derived season containing pending source facts. The
    /// path parser is source truth; hubs may map the returned exact sources to
    /// different provider-enriched item identities afterwards.
    pub async fn next_segment_job(
        &self,
        collection: &str,
        media_type: &str,
    ) -> Result<Option<kahawai_proto::v1::DetectSegments>> {
        #[derive(Clone)]
        struct Candidate {
            root: String,
            path: String,
            size: u64,
            mtime: i64,
            duration: u64,
            group: String,
            episode: u32,
            pending: bool,
            source_version: i64,
        }
        let mut tx = self.db.begin().await?;
        let rows = sqlx::query(
            "SELECT f.root_token,f.path_rel,f.size,f.mtime_unix,f.streams_json,f.version,
                    s.payload AS segment_payload, s.payload IS NULL AS pending
               FROM catalog_files f
               LEFT JOIN catalog_records s
                 ON s.collection_id=f.collection_id AND s.kind='file_segments'
                AND s.record_key=CAST(f.root_token || char(0) || f.path_rel AS BLOB)
                AND s.deleted=0
              WHERE f.collection_id=? AND f.error=''
               ORDER BY f.mtime_unix DESC,f.path_rel",
        )
        .bind(collection)
        .fetch_all(&mut *tx)
        .await?;
        let mut candidates = Vec::new();
        for row in rows {
            if let Some(payload) = row.get::<Option<Vec<u8>>, _>("segment_payload") {
                let result = kahawai_proto::v1::SegmentDetectionResult::decode(payload.as_slice())
                    .context("decoding local segment fact")?;
                let episode = result
                    .episodes
                    .first()
                    .context("local segment fact has no episode")?;
                if !episode.retryable && (episode.unreadable || !episode.error.is_empty()) {
                    continue;
                }
            }
            let path: String = row.get("path_rel");
            let guess = if media_type == "anime" {
                kahawai_core::names::parse_anime(&path)
            } else {
                kahawai_core::names::parse_episode(&path)
            };
            let Some(guess) = guess else { continue };
            let group = match guess.season {
                Some(season) => format!(
                    "{}\u{1f}{}\u{1f}{season}",
                    kahawai_core::names::normalize_title(&guess.show_title),
                    guess
                        .show_year
                        .map_or_else(String::new, |year| year.to_string())
                ),
                None => format!(
                    "{}\u{1f}dir:{}",
                    kahawai_core::names::normalize_title(&guess.show_title),
                    std::path::Path::new(&path)
                        .parent()
                        .unwrap_or_else(|| std::path::Path::new(""))
                        .to_string_lossy()
                ),
            };
            let info: kahawai_core::media::MediaInfo =
                serde_json::from_str(row.get::<&str, _>("streams_json"))?;
            candidates.push(Candidate {
                root: row.get("root_token"),
                path,
                size: row.get::<i64, _>("size") as u64,
                mtime: row.get("mtime_unix"),
                duration: info.duration_ms.unwrap_or(0),
                group,
                episode: guess.episode,
                pending: row.get::<i64, _>("pending") != 0,
                source_version: row.get("version"),
            });
        }
        // One exact, newest source per episode. A pending singleton is not a
        // season job, but it also must not starve every older viable season.
        // Candidate order is newest-first, preserving the scheduler's prior
        // preference while considering every pending group.
        let mut pending_groups = Vec::new();
        let mut pending_seen = std::collections::HashSet::new();
        let mut groups =
            std::collections::HashMap::<String, std::collections::BTreeMap<u32, Candidate>>::new();
        for candidate in candidates {
            if candidate.pending && pending_seen.insert(candidate.group.clone()) {
                pending_groups.push(candidate.group.clone());
            }
            let episodes = groups.entry(candidate.group.clone()).or_default();
            let replace = episodes
                .get(&candidate.episode)
                .is_none_or(|old| candidate.mtime > old.mtime);
            if replace {
                episodes.insert(candidate.episode, candidate);
            }
        }
        let selected = pending_groups
            .into_iter()
            .find_map(|group| groups.remove(&group).filter(|episodes| episodes.len() >= 2));
        let Some(episodes) = selected else {
            tx.commit().await?;
            return Ok(None);
        };
        for candidate in episodes.values() {
            sqlx::query(
                "INSERT INTO catalog_jobs
                   (collection_id,kind,job_key,root_token,path_rel,size,mtime_unix,
                    source_version,state,updated_at)
                 VALUES(?,'file_segments',?,?,?,?,?,?,'running',unixepoch())
                 ON CONFLICT(collection_id,kind,job_key) DO UPDATE SET
                   root_token=excluded.root_token,path_rel=excluded.path_rel,
                   size=excluded.size,mtime_unix=excluded.mtime_unix,
                   source_version=excluded.source_version,
                   state='running',error='',updated_at=unixepoch()",
            )
            .bind(collection)
            .bind(source_key(&candidate.root, &candidate.path))
            .bind(&candidate.root)
            .bind(&candidate.path)
            .bind(candidate.size as i64)
            .bind(candidate.mtime)
            .bind(candidate.source_version)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(Some(kahawai_proto::v1::DetectSegments {
            request_id: ulid::Ulid::generate().to_string(),
            detector: kahawai_core::segments::DETECTOR_GENERATION,
            collection_id: collection.to_string(),
            anime: media_type == "anime",
            episodes: episodes
                .into_values()
                .map(|candidate| kahawai_proto::v1::SegmentEpisode {
                    item_id: format!("{}\0{}", candidate.root, candidate.path),
                    source: Some(SourcePath {
                        root_token: candidate.root,
                        path_rel: candidate.path,
                    }),
                    expected_size: candidate.size,
                    expected_mtime_unix: candidate.mtime,
                    duration_ms: candidate.duration,
                })
                .collect(),
        }))
    }

    /// Persist a worker result as a versioned source fact. Subtitle extraction
    /// deliberately does not enter here: it belongs only to the hub that
    /// requested it.
    pub async fn store_fact(&self, message: HostToHub) -> Result<()> {
        let Some(message) = message.msg else {
            return Ok(());
        };
        match message {
            host_to_hub::Msg::FileHashes(hashes) => {
                let mut failure = None;
                for hash in hashes.hashes {
                    let source = hash.source.as_ref().context("hash result missing source")?;
                    let payload = kahawai_proto::v1::FileHashes {
                        collection_id: hashes.collection_id.clone(),
                        hashes: vec![hash.clone()],
                    }
                    .encode_to_vec();
                    if let Err(error) = self
                        .store_source_fact(
                            &hashes.collection_id,
                            "file_hashes",
                            source,
                            payload,
                            hash.error.is_empty().then_some(hash.size),
                            None,
                        )
                        .await
                    {
                        if is_stale_fact(&error) {
                            tracing::warn!(
                                root = %source.root_token,
                                path = %source.path_rel,
                                error = format!("{error:#}"),
                                "discarding stale hash result while retaining its siblings"
                            );
                            if failure.is_none() {
                                failure = Some(error);
                            }
                        } else {
                            failure = Some(error);
                        }
                    }
                }
                if let Some(error) = failure {
                    return Err(error);
                }
            }
            host_to_hub::Msg::FileLoudness(loudness) => {
                let source = loudness
                    .source
                    .as_ref()
                    .context("loudness result missing source")?;
                self.store_source_fact(
                    &loudness.collection_id,
                    "file_loudness",
                    source,
                    loudness.encode_to_vec(),
                    Some(loudness.size),
                    Some(loudness.mtime_unix),
                )
                .await?;
            }
            host_to_hub::Msg::FileAttachments(value) => {
                let source = value
                    .source
                    .as_ref()
                    .context("attachment result missing source")?;
                self.store_source_fact(
                    &value.collection_id,
                    "file_attachments",
                    source,
                    value.encode_to_vec(),
                    Some(value.size),
                    None,
                )
                .await?;
            }
            host_to_hub::Msg::FileKeyframeInterval(value) => {
                let source = value
                    .source
                    .as_ref()
                    .context("keyframe result missing source")?;
                self.store_source_fact(
                    &value.collection_id,
                    "file_keyframe",
                    source,
                    value.encode_to_vec(),
                    Some(value.size),
                    None,
                )
                .await?;
            }
            host_to_hub::Msg::FileVideoGeometry(value) => {
                let source = value
                    .source
                    .as_ref()
                    .context("geometry result missing source")?;
                self.store_source_fact(
                    &value.collection_id,
                    "file_geometry",
                    source,
                    value.encode_to_vec(),
                    Some(value.size),
                    None,
                )
                .await?;
            }
            host_to_hub::Msg::SegmentDetectionResult(value) => {
                anyhow::ensure!(
                    !value.collection_id.is_empty(),
                    "local segment result has no collection"
                );
                let collection = value.collection_id.as_str();
                for episode in &value.episodes {
                    if episode.retryable {
                        if let Some(source) = &episode.source {
                            self.release_claims(
                                collection,
                                "file_segments",
                                std::slice::from_ref(source),
                            )
                            .await?;
                        }
                        continue;
                    }
                    let source = episode
                        .source
                        .as_ref()
                        .context("segment result missing source")?;
                    let one = kahawai_proto::v1::SegmentDetectionResult {
                        request_id: value.request_id.clone(),
                        detector: value.detector,
                        elapsed_ms: value.elapsed_ms,
                        episodes: vec![episode.clone()],
                        error: value.error.clone(),
                        collection_id: value.collection_id.clone(),
                    };
                    if let Err(error) = self
                        .store_source_fact(
                            collection,
                            "file_segments",
                            source,
                            one.encode_to_vec(),
                            Some(episode.observed_size),
                            Some(episode.observed_mtime_unix),
                        )
                        .await
                    {
                        if is_stale_fact(&error) {
                            tracing::warn!(
                                root = %source.root_token,
                                path = %source.path_rel,
                                error = format!("{error:#}"),
                                "discarding stale episode result while retaining its siblings"
                            );
                        } else {
                            return Err(error);
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn store_source_fact(
        &self,
        collection: &str,
        kind: &str,
        source: &SourcePath,
        payload: Vec<u8>,
        expected_size: Option<u64>,
        expected_mtime: Option<i64>,
    ) -> Result<u64> {
        let mut tx = self.db.begin().await?;
        let claim: Option<(i64, i64, i64)> = sqlx::query_as(
            "SELECT size,mtime_unix,source_version FROM catalog_jobs
              WHERE collection_id=? AND kind=? AND job_key=? AND state='running'",
        )
        .bind(collection)
        .bind(kind)
        .bind(source_key(&source.root_token, &source.path_rel))
        .fetch_optional(&mut *tx)
        .await?;
        let current: Option<(i64, i64, i64)> = sqlx::query_as(
            "SELECT size,mtime_unix,version FROM catalog_files
              WHERE collection_id=? AND root_token=? AND path_rel=? AND error=''",
        )
        .bind(collection)
        .bind(&source.root_token)
        .bind(&source.path_rel)
        .fetch_optional(&mut *tx)
        .await?;
        let key = source_key(&source.root_token, &source.path_rel);
        let guarded = matches!(
            kind,
            "file_hashes"
                | "file_loudness"
                | "file_segments"
                | "file_attachments"
                | "file_keyframe"
                | "file_geometry"
        );
        let valid_claim = !guarded || (claim.is_some() && claim == current);
        let Some((size, mtime, _)) = current else {
            sqlx::query("DELETE FROM catalog_jobs WHERE collection_id=? AND kind=? AND job_key=?")
                .bind(collection)
                .bind(kind)
                .bind(&key)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Err(StaleFact("derived result names a stale local source").into());
        };
        if !valid_claim {
            sqlx::query("DELETE FROM catalog_jobs WHERE collection_id=? AND kind=? AND job_key=?")
                .bind(collection)
                .bind(kind)
                .bind(&key)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            if claim.is_none() {
                return Err(
                    StaleFact("derived result has no active local source-revision claim").into(),
                );
            }
            return Err(StaleFact("derived result belongs to an old source revision").into());
        }
        if !expected_size.is_none_or(|expected| expected == size as u64)
            || !expected_mtime.is_none_or(|expected| expected == mtime)
        {
            sqlx::query("DELETE FROM catalog_jobs WHERE collection_id=? AND kind=? AND job_key=?")
                .bind(collection)
                .bind(kind)
                .bind(&key)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Err(StaleFact("derived result belongs to an old source revision").into());
        }
        let version = Self::next_version(&mut tx, collection).await?;
        Self::put_record(&mut tx, collection, kind, &key, version, payload, false).await?;
        sqlx::query("DELETE FROM catalog_jobs WHERE collection_id=? AND kind=? AND job_key=?")
            .bind(collection)
            .bind(kind)
            .bind(&key)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.publish_version(collection, version, false);
        Ok(version)
    }
}

pub fn source_key(root_token: &str, path_rel: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(root_token.len() + path_rel.len() + 1);
    key.extend_from_slice(root_token.as_bytes());
    key.push(0);
    key.extend_from_slice(path_rel.as_bytes());
    key
}

pub fn split_source_key(key: &[u8]) -> Result<(&str, &str)> {
    let at = key
        .iter()
        .position(|byte| *byte == 0)
        .context("source key has no separator")?;
    Ok((
        std::str::from_utf8(&key[..at])?,
        std::str::from_utf8(&key[at + 1..])?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collection(root: &Path) -> CollectionConfig {
        CollectionConfig {
            name: "movies".into(),
            media_type: "movies".into(),
            roots: vec![root.to_path_buf()],
        }
    }

    fn media_info(with_audio: bool) -> kahawai_core::media::MediaInfo {
        kahawai_core::media::MediaInfo {
            duration_ms: Some(60_000),
            audio: with_audio
                .then(|| kahawai_core::media::AudioStream {
                    codec: "aac".into(),
                    channels: 2,
                    sample_rate: 48_000,
                    ..Default::default()
                })
                .into_iter()
                .collect(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn current_rows_and_tombstones_replay_by_collection_version() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let generation = catalog.begin_scan("movies").await.unwrap();
        let source = SourcePath {
            root_token: kahawai_core::media::root_token(root.path()),
            path_rel: "Film.mkv".into(),
        };
        catalog
            .upsert_file(
                "movies",
                &FileRecord {
                    source: Some(source.clone()),
                    size: 123,
                    mtime_unix: 7,
                    head_xxh3: 1,
                    tail_xxh3: 2,
                    oshash: 3,
                    streams_json: serde_json::to_string(&media_info(true)).unwrap(),
                },
                generation,
            )
            .await
            .unwrap();
        let key = source_key(&source.root_token, &source.path_rel);
        let mut tx = catalog.db.begin().await.unwrap();
        let segment_version = Catalog::next_version(&mut tx, "movies").await.unwrap();
        Catalog::put_record(
            &mut tx,
            "movies",
            "file_segments",
            &key,
            segment_version,
            vec![1],
            false,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        catalog
            .finish_scan("movies", generation, &Default::default())
            .await
            .unwrap();
        let snapshot = catalog.delta("movies", 0, true).await.unwrap();
        assert_eq!(snapshot.current_version, 2);
        assert_eq!(snapshot.records.len(), 2);
        assert!(snapshot.records.iter().all(|record| !record.deleted));

        let generation = catalog.begin_scan("movies").await.unwrap();
        catalog
            .finish_scan("movies", generation, &Default::default())
            .await
            .unwrap();
        let delta = catalog.delta("movies", 2, false).await.unwrap();
        assert_eq!(delta.current_version, 4);
        assert_eq!(delta.records.len(), 2);
        assert!(delta.records.iter().all(|record| record.deleted));
        assert_eq!(
            delta
                .records
                .iter()
                .map(|record| record.kind.as_str())
                .collect::<Vec<_>>(),
            ["file_segments", "file"]
        );
        assert!(delta.records.iter().all(|record| {
            split_source_key(&record.key).unwrap()
                == (source.root_token.as_str(), source.path_rel.as_str())
        }));
    }

    #[tokio::test]
    async fn large_snapshots_are_streamed_in_bounded_consistent_pages() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let generation = catalog.begin_scan("movies").await.unwrap();
        let root_token = kahawai_core::media::root_token(root.path());
        for index in 0..257 {
            catalog
                .upsert_file(
                    "movies",
                    &FileRecord {
                        source: Some(SourcePath {
                            root_token: root_token.clone(),
                            path_rel: format!("Film {index}.mkv"),
                        }),
                        size: index + 1,
                        mtime_unix: index as i64,
                        streams_json: serde_json::to_string(&media_info(true)).unwrap(),
                        ..Default::default()
                    },
                    generation,
                )
                .await
                .unwrap();
        }
        let mut pages = catalog.delta_pages("movies", 0, true).await.unwrap();
        let first = pages.recv().await.unwrap().unwrap();
        assert_eq!(first.records.len(), 256);
        assert!(!first.done);
        let second = pages.recv().await.unwrap().unwrap();
        assert_eq!(second.records.len(), 1);
        assert!(!second.done);
        assert_eq!(second.current_version, 257);
        let final_page = pages.recv().await.unwrap().unwrap();
        assert!(final_page.records.is_empty());
        assert!(final_page.done);
        assert!(pages.recv().await.is_none());
    }

    #[tokio::test]
    async fn epoch_and_version_survive_reopen() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let first = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let offer = first
            .offers(std::slice::from_ref(&config))
            .await
            .unwrap()
            .remove(0);
        drop(first);
        let second = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let reopened = second.offers(&[config]).await.unwrap().remove(0);
        assert_eq!(reopened.epoch, offer.epoch);
        assert_eq!(reopened.current_version, offer.current_version);
    }

    #[tokio::test]
    async fn committed_versions_wake_each_subscriber() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let mut first = catalog.subscribe_versions();
        let mut second = catalog.subscribe_versions();
        let generation = catalog.begin_scan("movies").await.unwrap();
        let file = FileRecord {
            source: Some(SourcePath {
                root_token: kahawai_core::media::root_token(root.path()),
                path_rel: "Film.mkv".into(),
            }),
            size: 123,
            mtime_unix: 7,
            streams_json: serde_json::to_string(&media_info(true)).unwrap(),
            ..Default::default()
        };

        catalog
            .upsert_file("movies", &file, generation)
            .await
            .unwrap();
        assert!(first.has_changed().unwrap());
        assert!(second.has_changed().unwrap());
        assert_eq!(first.borrow_and_update()["movies"].1, 1);
        assert_eq!(second.borrow_and_update()["movies"].1, 1);

        assert!(
            catalog
                .upsert_file("movies", &file, generation)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!first.has_changed().unwrap());
        assert!(!second.has_changed().unwrap());

        assert!(
            catalog
                .upsert_file("missing", &file, generation)
                .await
                .is_err()
        );
        assert!(!first.has_changed().unwrap());
        assert!(!second.has_changed().unwrap());
    }

    #[tokio::test]
    async fn empty_first_scan_completion_is_published() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let mut versions = catalog.subscribe_versions();
        let generation = catalog.begin_scan("movies").await.unwrap();

        catalog
            .finish_scan("movies", generation, &Default::default())
            .await
            .unwrap();

        assert!(versions.has_changed().unwrap());
        assert_eq!(versions.borrow_and_update()["movies"].1, 0);
        assert!(versions.borrow()["movies"].2);
    }

    #[tokio::test]
    async fn offers_wait_only_until_the_first_scan_completes() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let first = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        assert!(!first.version_states().await.unwrap()["movies"].2);

        let generation = first.begin_scan("movies").await.unwrap();
        first
            .finish_scan("movies", generation, &Default::default())
            .await
            .unwrap();
        assert!(first.version_states().await.unwrap()["movies"].2);

        drop(first);
        let reopened = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        assert!(reopened.version_states().await.unwrap()["movies"].2);
        assert!(reopened.offers(&[config]).await.unwrap()[0].scanning);
    }

    #[tokio::test]
    async fn hub_acknowledgements_are_independent_and_monotonic() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let generation = catalog.begin_scan("movies").await.unwrap();
        for version in 1..=3 {
            catalog
                .upsert_file(
                    "movies",
                    &FileRecord {
                        source: Some(SourcePath {
                            root_token: kahawai_core::media::root_token(root.path()),
                            path_rel: format!("Film {version}.mkv"),
                        }),
                        size: version,
                        mtime_unix: version as i64,
                        streams_json: serde_json::to_string(&media_info(true)).unwrap(),
                        ..Default::default()
                    },
                    generation,
                )
                .await
                .unwrap();
        }
        let epoch = catalog
            .offers(std::slice::from_ref(&config))
            .await
            .unwrap()
            .remove(0)
            .epoch;
        catalog
            .acknowledge("home", "movies", &epoch, 3)
            .await
            .unwrap();
        catalog
            .acknowledge("family", "movies", &epoch, 1)
            .await
            .unwrap();
        catalog
            .acknowledge("home", "movies", &epoch, 1)
            .await
            .unwrap();
        let stored: Vec<(String, i64)> = sqlx::query_as(
            "SELECT hub_id,version FROM catalog_hub_acks
              WHERE collection_id='movies' ORDER BY hub_id",
        )
        .fetch_all(&catalog.db)
        .await
        .unwrap();
        assert_eq!(stored, [("family".into(), 1), ("home".into(), 3)]);
    }

    #[tokio::test]
    async fn analyzer_generation_changes_reschedule_local_facts() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let source = SourcePath {
            root_token: kahawai_core::media::root_token(root.path()),
            path_rel: "Film.mkv".into(),
        };
        let generation = catalog.begin_scan("movies").await.unwrap();
        catalog
            .upsert_file(
                "movies",
                &FileRecord {
                    source: Some(source.clone()),
                    size: 200,
                    mtime_unix: 8,
                    streams_json: serde_json::to_string(&media_info(true)).unwrap(),
                    ..Default::default()
                },
                generation,
            )
            .await
            .unwrap();
        catalog
            .claim_sources("movies", "file_loudness", 1)
            .await
            .unwrap();
        catalog
            .store_fact(HostToHub {
                msg: Some(host_to_hub::Msg::FileLoudness(
                    kahawai_proto::v1::FileLoudness {
                        collection_id: "movies".into(),
                        source: Some(source.clone()),
                        analyzer: kahawai_media::loudness::ANALYZER,
                        size: 200,
                        mtime_unix: 8,
                        ..Default::default()
                    },
                )),
            })
            .await
            .unwrap();
        catalog
            .claim_sources("movies", "file_segments", 1)
            .await
            .unwrap();
        catalog
            .store_fact(HostToHub {
                msg: Some(host_to_hub::Msg::SegmentDetectionResult(
                    kahawai_proto::v1::SegmentDetectionResult {
                        request_id: "movies:test".into(),
                        collection_id: "movies".into(),
                        detector: kahawai_core::segments::DETECTOR_GENERATION,
                        episodes: vec![kahawai_proto::v1::SegmentEpisodeResult {
                            source: Some(source),
                            observed_size: 200,
                            observed_mtime_unix: 8,
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                )),
            })
            .await
            .unwrap();
        sqlx::query("UPDATE catalog_meta SET value='old'")
            .execute(&catalog.db)
            .await
            .unwrap();
        drop(catalog);

        let reopened = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        assert_eq!(
            reopened
                .pending_sources("movies", "file_loudness", 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            reopened
                .pending_sources("movies", "file_segments", 10)
                .await
                .unwrap()
                .len(),
            1
        );
        let invalidations = reopened.delta("movies", 3, false).await.unwrap();
        assert_eq!(invalidations.records.len(), 2);
        assert!(invalidations.records.iter().all(|record| record.deleted));
    }

    #[tokio::test]
    async fn disabled_segment_policy_preserves_existing_results() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let source = SourcePath {
            root_token: kahawai_core::media::root_token(root.path()),
            path_rel: "Film.mkv".into(),
        };
        let generation = catalog.begin_scan("movies").await.unwrap();
        catalog
            .upsert_file(
                "movies",
                &FileRecord {
                    source: Some(source.clone()),
                    size: 200,
                    mtime_unix: 8,
                    streams_json: serde_json::to_string(&media_info(true)).unwrap(),
                    ..Default::default()
                },
                generation,
            )
            .await
            .unwrap();
        catalog
            .claim_sources("movies", "file_segments", 1)
            .await
            .unwrap();
        catalog
            .store_fact(HostToHub {
                msg: Some(host_to_hub::Msg::SegmentDetectionResult(
                    kahawai_proto::v1::SegmentDetectionResult {
                        request_id: "movies:test".into(),
                        collection_id: "movies".into(),
                        detector: kahawai_core::segments::DETECTOR_GENERATION,
                        episodes: vec![kahawai_proto::v1::SegmentEpisodeResult {
                            source: Some(source),
                            observed_size: 200,
                            observed_mtime_unix: 8,
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                )),
            })
            .await
            .unwrap();
        sqlx::query("UPDATE catalog_meta SET value='old' WHERE key='file_segments'")
            .execute(&catalog.db)
            .await
            .unwrap();
        drop(catalog);

        let reopened = Catalog::open_with_segment_detection(state.path(), &[config], false)
            .await
            .unwrap();
        assert!(
            reopened
                .pending_sources("movies", "file_segments", 1)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            reopened
                .delta("movies", 0, true)
                .await
                .unwrap()
                .records
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn durable_claim_prevents_duplicate_work_until_result_arrives() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let source = SourcePath {
            root_token: kahawai_core::media::root_token(root.path()),
            path_rel: "Film.mkv".into(),
        };
        let generation = catalog.begin_scan("movies").await.unwrap();
        catalog
            .upsert_file(
                "movies",
                &FileRecord {
                    source: Some(source.clone()),
                    size: 200,
                    mtime_unix: 8,
                    streams_json: serde_json::to_string(&media_info(true)).unwrap(),
                    ..Default::default()
                },
                generation,
            )
            .await
            .unwrap();

        assert_eq!(
            catalog
                .claim_sources("movies", "file_loudness", 10)
                .await
                .unwrap(),
            vec![source.clone()]
        );
        assert!(
            catalog
                .claim_sources("movies", "file_loudness", 10)
                .await
                .unwrap()
                .is_empty()
        );
        catalog
            .store_fact(HostToHub {
                msg: Some(host_to_hub::Msg::FileLoudness(
                    kahawai_proto::v1::FileLoudness {
                        collection_id: "movies".into(),
                        source: Some(source),
                        size: 200,
                        mtime_unix: 8,
                        ..Default::default()
                    },
                )),
            })
            .await
            .unwrap();
        assert!(
            catalog
                .claim_sources("movies", "file_loudness", 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn same_stamp_replacement_rejects_the_inflight_old_result() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let source = SourcePath {
            root_token: kahawai_core::media::root_token(root.path()),
            path_rel: "Film.mkv".into(),
        };
        let generation = catalog.begin_scan("movies").await.unwrap();
        let mut file = FileRecord {
            source: Some(source.clone()),
            size: 200,
            mtime_unix: 8,
            head_xxh3: 1,
            tail_xxh3: 2,
            oshash: 3,
            streams_json: serde_json::to_string(&media_info(true)).unwrap(),
        };
        catalog
            .upsert_file("movies", &file, generation)
            .await
            .unwrap();
        catalog
            .claim_sources("movies", "file_loudness", 1)
            .await
            .unwrap();

        file.head_xxh3 = 99;
        catalog
            .upsert_file("movies", &file, generation)
            .await
            .unwrap();
        let old_result = HostToHub {
            msg: Some(host_to_hub::Msg::FileLoudness(
                kahawai_proto::v1::FileLoudness {
                    collection_id: "movies".into(),
                    source: Some(source.clone()),
                    analyzer: kahawai_media::loudness::ANALYZER,
                    size: 200,
                    mtime_unix: 8,
                    ..Default::default()
                },
            )),
        };
        assert!(catalog.store_fact(old_result.clone()).await.is_err());
        assert_eq!(
            catalog
                .claim_sources("movies", "file_loudness", 1)
                .await
                .unwrap(),
            vec![source]
        );
        catalog.store_fact(old_result).await.unwrap();
    }

    #[tokio::test]
    async fn first_scan_error_is_journaled_once_and_later_tombstoned() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let error = FileError {
            collection_id: "movies".into(),
            source: Some(SourcePath {
                root_token: kahawai_core::media::root_token(root.path()),
                path_rel: "Unreadable.mkv".into(),
            }),
            error: "permission denied".into(),
        };
        let generation = catalog.begin_scan("movies").await.unwrap();
        assert_eq!(
            catalog
                .record_error("movies", &error, generation)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            catalog
                .record_error("movies", &error, generation)
                .await
                .unwrap(),
            1
        );

        let generation = catalog.begin_scan("movies").await.unwrap();
        catalog
            .finish_scan("movies", generation, &Default::default())
            .await
            .unwrap();
        let delta = catalog.delta("movies", 1, false).await.unwrap();
        assert_eq!(delta.current_version, 3);
        assert_eq!(delta.records.len(), 2);
        assert!(delta.records.iter().all(|record| record.deleted));
        assert_eq!(
            delta
                .records
                .iter()
                .map(|record| record.kind.as_str())
                .collect::<Vec<_>>(),
            ["file_error", "file"]
        );
    }

    #[tokio::test]
    async fn a_failed_reinspection_removes_the_stale_playable_record() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let source = SourcePath {
            root_token: kahawai_core::media::root_token(root.path()),
            path_rel: "Broke.mkv".into(),
        };
        let generation = catalog.begin_scan("movies").await.unwrap();
        let original = FileRecord {
            source: Some(source.clone()),
            size: 200,
            mtime_unix: 8,
            streams_json: serde_json::to_string(&media_info(true)).unwrap(),
            ..Default::default()
        };
        catalog
            .upsert_file("movies", &original, generation)
            .await
            .unwrap();
        let error = FileError {
            collection_id: "movies".into(),
            source: Some(source),
            error: "discoverer failed".into(),
        };
        assert_eq!(
            catalog
                .record_error("movies", &error, generation)
                .await
                .unwrap(),
            3
        );

        let delta = catalog.delta("movies", 1, false).await.unwrap();
        assert_eq!(delta.current_version, 3);
        assert_eq!(delta.records.len(), 2);
        assert_eq!(delta.records[0].kind, "file");
        assert!(delta.records[0].deleted);
        assert_eq!(delta.records[1].kind, "file_error");
        assert!(!delta.records[1].deleted);
        let snapshot = catalog.delta("movies", 0, true).await.unwrap();
        assert_eq!(snapshot.records.len(), 1);
        assert_eq!(snapshot.records[0].kind, "file_error");

        assert!(
            catalog.known_files("movies").await.unwrap().is_empty(),
            "an error row let the next scan skip reinspection"
        );
        let recovery = catalog.begin_scan("movies").await.unwrap();
        catalog
            .upsert_file("movies", &original, recovery)
            .await
            .unwrap();
        let recovered = catalog.delta("movies", 0, true).await.unwrap();
        assert_eq!(recovered.records.len(), 1);
        assert_eq!(recovered.records[0].kind, "file");
        assert!(!recovered.records[0].deleted);
    }

    #[tokio::test]
    async fn loudness_claims_skip_sources_without_audio() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let generation = catalog.begin_scan("movies").await.unwrap();
        catalog
            .upsert_file(
                "movies",
                &FileRecord {
                    source: Some(SourcePath {
                        root_token: kahawai_core::media::root_token(root.path()),
                        path_rel: "Silent.mkv".into(),
                    }),
                    size: 200,
                    mtime_unix: 8,
                    streams_json: serde_json::to_string(&media_info(false)).unwrap(),
                    ..Default::default()
                },
                generation,
            )
            .await
            .unwrap();
        assert!(
            catalog
                .claim_sources("movies", "file_loudness", 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            catalog
                .discovery_status("movies")
                .await
                .unwrap()
                .pending_loudness,
            0
        );
    }

    #[tokio::test]
    async fn sidecar_only_metadata_changes_keep_expensive_source_facts() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let source = SourcePath {
            root_token: kahawai_core::media::root_token(root.path()),
            path_rel: "Film.mkv".into(),
        };
        let generation = catalog.begin_scan("movies").await.unwrap();
        let original = FileRecord {
            source: Some(source.clone()),
            size: 200,
            mtime_unix: 8,
            head_xxh3: 1,
            tail_xxh3: 2,
            oshash: 3,
            streams_json: serde_json::to_string(&media_info(true)).unwrap(),
        };
        catalog
            .upsert_file("movies", &original, generation)
            .await
            .unwrap();
        catalog
            .claim_sources("movies", "file_loudness", 1)
            .await
            .unwrap();
        catalog
            .store_fact(HostToHub {
                msg: Some(host_to_hub::Msg::FileLoudness(
                    kahawai_proto::v1::FileLoudness {
                        collection_id: "movies".into(),
                        source: Some(source),
                        analyzer: kahawai_media::loudness::ANALYZER,
                        size: 200,
                        mtime_unix: 8,
                        ..Default::default()
                    },
                )),
            })
            .await
            .unwrap();
        let cursor =
            catalog.offers(std::slice::from_ref(&config)).await.unwrap()[0].current_version;
        let mut published = catalog.subscribe_versions();
        published.borrow_and_update();
        let mut metadata_only = original;
        let mut info = media_info(true);
        info.tags.insert("title".into(), "From an NFO".into());
        metadata_only.streams_json = serde_json::to_string(&info).unwrap();
        catalog
            .upsert_file("movies", &metadata_only, generation)
            .await
            .unwrap();

        assert!(
            catalog
                .pending_sources("movies", "file_loudness", 1)
                .await
                .unwrap()
                .is_empty(),
            "presentation-only metadata invalidated a full-file measurement"
        );
        let snapshot = catalog.delta("movies", 0, true).await.unwrap();
        assert_eq!(
            snapshot
                .records
                .iter()
                .map(|record| record.kind.as_str())
                .collect::<Vec<_>>(),
            ["file", "file_loudness"],
            "snapshot projected a derived fact before its current file row"
        );
        let incremental = catalog.delta("movies", cursor, false).await.unwrap();
        assert_eq!(
            published.borrow_and_update()["movies"].1,
            incremental.current_version,
            "publisher stopped at the file row before re-versioned derived facts"
        );
        assert_eq!(
            incremental
                .records
                .iter()
                .map(|record| record.kind.as_str())
                .collect::<Vec<_>>(),
            ["file", "file_loudness"],
            "a metadata-only file update did not replay its retained fact afterwards"
        );
    }

    #[tokio::test]
    async fn cheap_discovery_is_local_bounded_and_exact_revision_guarded() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let source = SourcePath {
            root_token: kahawai_core::media::root_token(root.path()),
            path_rel: "Film.mkv".into(),
        };
        let mut info = media_info(false);
        info.container = Some("matroska".into());
        info.video.push(kahawai_core::media::VideoStream {
            codec: "h264".into(),
            width: 1920,
            height: 1080,
            ..Default::default()
        });
        let generation = catalog.begin_scan("movies").await.unwrap();
        let mut file = FileRecord {
            source: Some(source.clone()),
            size: 200,
            mtime_unix: 8,
            head_xxh3: 1,
            tail_xxh3: 2,
            oshash: 3,
            streams_json: serde_json::to_string(&info).unwrap(),
        };
        catalog
            .upsert_file("movies", &file, generation)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .discovery_status("movies")
                .await
                .unwrap()
                .pending_cheap,
            3
        );

        let claimed = catalog
            .claim_cheap_sources("movies", "file_attachments", 1)
            .await
            .unwrap();
        assert_eq!(claimed.as_slice(), std::slice::from_ref(&source));
        assert!(
            catalog
                .claim_cheap_sources("movies", "file_attachments", 1)
                .await
                .unwrap()
                .is_empty(),
            "one local cheap fact was leased twice"
        );
        sqlx::query(
            "UPDATE catalog_jobs SET updated_at=unixepoch()-3600
              WHERE collection_id='movies' AND kind='file_attachments'",
        )
        .execute(&catalog.db)
        .await
        .unwrap();
        assert!(
            catalog
                .claim_cheap_sources("movies", "file_attachments", 1)
                .await
                .unwrap()
                .is_empty(),
            "an old running claim was overwritten while its worker could still return"
        );
        catalog
            .store_fact(HostToHub {
                msg: Some(host_to_hub::Msg::FileAttachments(
                    kahawai_proto::v1::FileAttachments {
                        collection_id: "movies".into(),
                        source: Some(source.clone()),
                        size: 200,
                        attachments_json: "[]".into(),
                        chapters_json: Some("[]".into()),
                    },
                )),
            })
            .await
            .unwrap();
        assert_eq!(
            catalog
                .discovery_status("movies")
                .await
                .unwrap()
                .pending_cheap,
            2
        );

        catalog
            .claim_cheap_sources("movies", "file_geometry", 1)
            .await
            .unwrap();
        // Same size and mtime, different bytes: only the catalogue revision
        // distinguishes this replacement from the claimed source.
        file.head_xxh3 = 99;
        catalog
            .upsert_file("movies", &file, generation)
            .await
            .unwrap();
        assert!(
            catalog
                .store_fact(HostToHub {
                    msg: Some(host_to_hub::Msg::FileVideoGeometry(
                        kahawai_proto::v1::FileVideoGeometry {
                            collection_id: "movies".into(),
                            source: Some(source),
                            size: 200,
                            geometry_json: "[]".into(),
                            error: String::new(),
                        },
                    )),
                })
                .await
                .is_err(),
            "a cheap result attached to replacement bytes with the same stat stamp"
        );
    }

    #[tokio::test]
    async fn metadata_only_update_carries_running_claim_to_the_same_bytes() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let source = SourcePath {
            root_token: kahawai_core::media::root_token(root.path()),
            path_rel: "Film.mkv".into(),
        };
        let generation = catalog.begin_scan("movies").await.unwrap();
        let mut file = FileRecord {
            source: Some(source.clone()),
            size: 200,
            mtime_unix: 8,
            head_xxh3: 1,
            tail_xxh3: 2,
            oshash: 3,
            streams_json: serde_json::to_string(&media_info(false)).unwrap(),
        };
        catalog
            .upsert_file("movies", &file, generation)
            .await
            .unwrap();
        catalog
            .claim_sources("movies", "file_hashes", 1)
            .await
            .unwrap();

        let mut updated = media_info(false);
        updated
            .tags
            .insert("title".into(), "New sidecar title".into());
        file.streams_json = serde_json::to_string(&updated).unwrap();
        catalog
            .upsert_file("movies", &file, generation)
            .await
            .unwrap();
        catalog
            .store_fact(HostToHub {
                msg: Some(host_to_hub::Msg::FileHashes(
                    kahawai_proto::v1::FileHashes {
                        collection_id: "movies".into(),
                        hashes: vec![kahawai_proto::v1::FileHash {
                            source: Some(source),
                            size: 200,
                            ed2k_hex: "current".into(),
                            ..Default::default()
                        }],
                    },
                )),
            })
            .await
            .expect("byte-identical metadata update invalidated running work");
    }

    #[tokio::test]
    async fn a_newest_single_episode_does_not_starve_an_analyzable_season() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = CollectionConfig {
            name: "series".into(),
            media_type: "series".into(),
            roots: vec![root.path().to_path_buf()],
        };
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let root_token = kahawai_core::media::root_token(root.path());
        let generation = catalog.begin_scan("series").await.unwrap();
        for (path_rel, mtime) in [
            ("Newest Show/Season 01/Newest Show - S01E01.mkv", 100),
            ("Older Show/Season 01/Older Show - S01E01.mkv", 90),
            ("Older Show/Season 01/Older Show - S01E02.mkv", 80),
        ] {
            catalog
                .upsert_file(
                    "series",
                    &FileRecord {
                        source: Some(SourcePath {
                            root_token: root_token.clone(),
                            path_rel: path_rel.into(),
                        }),
                        size: 200,
                        mtime_unix: mtime,
                        streams_json: serde_json::to_string(&media_info(true)).unwrap(),
                        ..Default::default()
                    },
                    generation,
                )
                .await
                .unwrap();
        }

        let job = catalog
            .next_segment_job("series", "series")
            .await
            .unwrap()
            .expect("the older complete season should be selected");
        assert_eq!(job.episodes.len(), 2);
        assert!(job.episodes.iter().all(|episode| {
            episode
                .source
                .as_ref()
                .is_some_and(|source| source.path_rel.contains("Older Show"))
        }));
    }

    #[tokio::test]
    async fn terminal_segment_source_is_not_reused_as_comparison_material() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let collection_id = "series:archive";
        let config = CollectionConfig {
            name: collection_id.into(),
            media_type: "series".into(),
            roots: vec![root.path().to_path_buf()],
        };
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let generation = catalog.begin_scan(collection_id).await.unwrap();
        let root_token = kahawai_core::media::root_token(root.path());
        for episode in 1..=2 {
            catalog
                .upsert_file(
                    collection_id,
                    &FileRecord {
                        source: Some(SourcePath {
                            root_token: root_token.clone(),
                            path_rel: format!("Show/Show S01E{episode:02}.mkv"),
                        }),
                        size: 200,
                        mtime_unix: 8,
                        streams_json: serde_json::to_string(&media_info(true)).unwrap(),
                        ..Default::default()
                    },
                    generation,
                )
                .await
                .unwrap();
        }
        let job = catalog
            .next_segment_job(collection_id, "series")
            .await
            .unwrap()
            .unwrap();
        assert!(
            !job.request_id.contains(':'),
            "opaque request id must not encode the collection"
        );
        catalog
            .store_fact(HostToHub {
                msg: Some(host_to_hub::Msg::SegmentDetectionResult(
                    kahawai_proto::v1::SegmentDetectionResult {
                        request_id: job.request_id,
                        collection_id: collection_id.into(),
                        detector: kahawai_core::segments::DETECTOR_GENERATION,
                        episodes: vec![kahawai_proto::v1::SegmentEpisodeResult {
                            source: Some(SourcePath {
                                root_token,
                                path_rel: "Show/Show S01E01.mkv".into(),
                            }),
                            observed_size: 200,
                            observed_mtime_unix: 8,
                            unreadable: true,
                            error: "decoder failed".into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                )),
            })
            .await
            .unwrap();

        assert!(
            catalog
                .next_segment_job(collection_id, "series")
                .await
                .unwrap()
                .is_none(),
            "a terminal failure was reused to form the same doomed cohort"
        );
    }

    #[tokio::test]
    async fn long_running_results_cannot_attach_to_replacement_bytes() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let source = SourcePath {
            root_token: kahawai_core::media::root_token(root.path()),
            path_rel: "Film.mkv".into(),
        };
        let generation = catalog.begin_scan("movies").await.unwrap();
        catalog
            .upsert_file(
                "movies",
                &FileRecord {
                    source: Some(source.clone()),
                    size: 200,
                    mtime_unix: 8,
                    streams_json: serde_json::to_string(&media_info(true)).unwrap(),
                    ..Default::default()
                },
                generation,
            )
            .await
            .unwrap();
        catalog
            .claim_sources("movies", "file_loudness", 1)
            .await
            .unwrap();
        let error = catalog
            .store_fact(HostToHub {
                msg: Some(host_to_hub::Msg::FileLoudness(
                    kahawai_proto::v1::FileLoudness {
                        collection_id: "movies".into(),
                        source: Some(source),
                        size: 100,
                        mtime_unix: 7,
                        ..Default::default()
                    },
                )),
            })
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("old source revision"),
            "{error:#}"
        );
        assert_eq!(
            catalog
                .pending_sources("movies", "file_loudness", 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn same_size_hash_result_needs_its_claimed_revision() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let source = SourcePath {
            root_token: kahawai_core::media::root_token(root.path()),
            path_rel: "Film.mkv".into(),
        };
        let generation = catalog.begin_scan("movies").await.unwrap();
        catalog
            .upsert_file(
                "movies",
                &FileRecord {
                    source: Some(source.clone()),
                    size: 200,
                    mtime_unix: 8,
                    streams_json: serde_json::to_string(&kahawai_core::media::MediaInfo::default())
                        .unwrap(),
                    ..Default::default()
                },
                generation,
            )
            .await
            .unwrap();
        catalog
            .claim_sources("movies", "file_hashes", 1)
            .await
            .unwrap();
        catalog
            .upsert_file(
                "movies",
                &FileRecord {
                    source: Some(source.clone()),
                    size: 200,
                    mtime_unix: 9,
                    streams_json: serde_json::to_string(&kahawai_core::media::MediaInfo::default())
                        .unwrap(),
                    ..Default::default()
                },
                generation,
            )
            .await
            .unwrap();
        let error = catalog
            .store_fact(HostToHub {
                msg: Some(host_to_hub::Msg::FileHashes(
                    kahawai_proto::v1::FileHashes {
                        collection_id: "movies".into(),
                        hashes: vec![kahawai_proto::v1::FileHash {
                            source: Some(source),
                            size: 200,
                            ed2k_hex: "old".into(),
                            ..Default::default()
                        }],
                    },
                )),
            })
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("old source revision"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn terminal_hash_failure_settles_its_exact_claim() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let config = collection(root.path());
        let catalog = Catalog::open(state.path(), std::slice::from_ref(&config))
            .await
            .unwrap();
        let source = SourcePath {
            root_token: kahawai_core::media::root_token(root.path()),
            path_rel: "Film.mkv".into(),
        };
        let generation = catalog.begin_scan("movies").await.unwrap();
        catalog
            .upsert_file(
                "movies",
                &FileRecord {
                    source: Some(source.clone()),
                    size: 200,
                    mtime_unix: 8,
                    streams_json: serde_json::to_string(&kahawai_core::media::MediaInfo::default())
                        .unwrap(),
                    ..Default::default()
                },
                generation,
            )
            .await
            .unwrap();
        catalog
            .claim_sources("movies", "file_hashes", 1)
            .await
            .unwrap();
        catalog
            .store_fact(HostToHub {
                msg: Some(host_to_hub::Msg::FileHashes(
                    kahawai_proto::v1::FileHashes {
                        collection_id: "movies".into(),
                        hashes: vec![kahawai_proto::v1::FileHash {
                            source: Some(source),
                            error: "permission denied".into(),
                            ..Default::default()
                        }],
                    },
                )),
            })
            .await
            .unwrap();
        assert!(
            catalog
                .pending_sources("movies", "file_hashes", 1)
                .await
                .unwrap()
                .is_empty(),
            "a terminal hash failure remained queued forever"
        );
    }
}
