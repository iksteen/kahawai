//! Registry (HUB-1): connection state in memory, everything else in SQLite
//! so a hub restart recovers without a rescan (NFR-3).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::{Context, Result};
use kahawai_core::names;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone)]
pub struct SatelliteState {
    pub module_type: String,
    pub name: String,
    pub cert_fingerprint: String,
    pub connected: bool,
    pub last_seen: SystemTime,
}

pub struct FileUpsertRecord {
    pub path_rel: String,
    pub size: u64,
    pub mtime_unix: i64,
    pub head_xxh3: u64,
    pub tail_xxh3: u64,
    pub oshash: u64,
    pub streams_json: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CollectionRow {
    pub module_id: String,
    pub collection_id: String,
    pub media_type: String,
    pub available: bool,
    pub file_count: i64,
}

pub struct Registry {
    db: SqlitePool,
    connected: Mutex<HashMap<String, SatelliteState>>,
    /// Command senders for connected hosts' Link streams.
    links: Mutex<HashMap<String, tokio::sync::mpsc::Sender<Result<kahawai_proto::v1::HubToHost, tonic::Status>>>>,
}

impl Registry {
    pub fn new(db: SqlitePool) -> Self {
        Self { db, connected: Mutex::new(HashMap::new()), links: Mutex::new(HashMap::new()) }
    }

    pub fn register_link(
        &self,
        module_id: &str,
        tx: tokio::sync::mpsc::Sender<Result<kahawai_proto::v1::HubToHost, tonic::Status>>,
    ) {
        self.links.lock().unwrap().insert(module_id.to_string(), tx);
    }

    pub fn unregister_link(&self, module_id: &str) {
        self.links.lock().unwrap().remove(module_id);
    }

    /// Send a command down a connected host's Link stream.
    pub async fn send_to_host(
        &self,
        module_id: &str,
        msg: kahawai_proto::v1::HubToHost,
    ) -> Result<()> {
        let tx = self
            .links
            .lock()
            .unwrap()
            .get(module_id)
            .cloned()
            .with_context(|| format!("mediahost {module_id} is not connected"))?;
        tx.send(Ok(msg)).await.map_err(|_| anyhow::anyhow!("link to {module_id} closed"))
    }

    pub fn db(&self) -> &SqlitePool {
        &self.db
    }

    // ---- runtime connection state ----

    pub fn connected(&self, module_id: &str, module_type: &str, name: &str, fingerprint: &str) {
        self.connected.lock().unwrap().insert(
            module_id.to_string(),
            SatelliteState {
                module_type: module_type.to_string(),
                name: name.to_string(),
                cert_fingerprint: fingerprint.to_string(),
                connected: true,
                last_seen: SystemTime::now(),
            },
        );
        tracing::info!(%module_id, module_type, name, "satellite connected");
    }

    pub fn seen(&self, module_id: &str) {
        if let Some(s) = self.connected.lock().unwrap().get_mut(module_id) {
            s.last_seen = SystemTime::now();
        }
    }

    pub fn disconnected(&self, module_id: &str) {
        // AR-6: collections of a disconnected host are unavailable (their
        // availability is derived from this map), never deleted.
        if let Some(s) = self.connected.lock().unwrap().get_mut(module_id) {
            s.connected = false;
            s.last_seen = SystemTime::now();
            tracing::info!(%module_id, "satellite disconnected");
        }
    }

    pub fn is_connected(&self, module_id: &str) -> bool {
        self.connected
            .lock()
            .unwrap()
            .get(module_id)
            .is_some_and(|s| s.connected)
    }

    pub fn snapshot(&self) -> Vec<(String, SatelliteState)> {
        let mut v: Vec<_> = self
            .connected
            .lock()
            .unwrap()
            .iter()
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    // ---- persistent state ----

    /// Record an approved satellite (SEC-4 bookkeeping).
    pub async fn record_satellite(
        &self,
        module_id: &str,
        module_type: &str,
        name: &str,
        cert_fingerprint: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO satellites (module_id, module_type, name, cert_fingerprint)
             VALUES (?, ?, ?, ?)
             ON CONFLICT (module_id) DO UPDATE
             SET name = excluded.name, cert_fingerprint = excluded.cert_fingerprint",
        )
        .bind(module_id)
        .bind(module_type)
        .bind(name)
        .bind(cert_fingerprint)
        .execute(&self.db)
        .await
        .context("recording satellite")?;
        Ok(())
    }

    pub async fn announce_collection(
        &self,
        module_id: &str,
        collection_id: &str,
        media_type: &str,
        roots: &[String],
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO collections (module_id, collection_id, media_type, roots_json)
             VALUES (?, ?, ?, ?)
             ON CONFLICT (module_id, collection_id) DO UPDATE
             SET media_type = excluded.media_type, roots_json = excluded.roots_json",
        )
        .bind(module_id)
        .bind(collection_id)
        .bind(media_type)
        .bind(serde_json::to_string(roots)?)
        .execute(&self.db)
        .await?;
        tracing::info!(%module_id, collection = collection_id, media_type, "collection announced");
        Ok(())
    }

    /// Upsert file records and resolve them to items (movies for now).
    pub async fn upsert_files(
        &self,
        module_id: &str,
        collection_id: &str,
        files: Vec<FileUpsertRecord>,
    ) -> Result<usize> {
        let media_type: Option<String> = sqlx::query_scalar(
            "SELECT media_type FROM collections WHERE module_id = ? AND collection_id = ?",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_optional(&self.db)
        .await?;
        let resolve_movies = media_type.as_deref() == Some("movies");

        let mut tx = self.db.begin().await?;
        let n = files.len();
        for f in files {
            sqlx::query(
                "INSERT INTO files
                   (module_id, collection_id, path_rel, size, mtime_unix,
                    head_xxh3, tail_xxh3, oshash, streams_json)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT (module_id, collection_id, path_rel) DO UPDATE SET
                   size = excluded.size, mtime_unix = excluded.mtime_unix,
                   head_xxh3 = excluded.head_xxh3, tail_xxh3 = excluded.tail_xxh3,
                   oshash = excluded.oshash, streams_json = excluded.streams_json",
            )
            .bind(module_id)
            .bind(collection_id)
            .bind(&f.path_rel)
            .bind(f.size as i64)
            .bind(f.mtime_unix)
            .bind(f.head_xxh3 as i64)
            .bind(f.tail_xxh3 as i64)
            .bind(f.oshash as i64)
            .bind(&f.streams_json)
            .execute(&mut *tx)
            .await?;

            if resolve_movies {
                let filename = f.path_rel.rsplit('/').next().unwrap_or(&f.path_rel);
                let guess = names::parse_movie(filename);
                let norm = names::normalize_title(&guess.title);
                let existing: Option<String> = sqlx::query_scalar(
                    "SELECT id FROM items WHERE kind = 'movie' AND norm_title = ? AND year IS ?",
                )
                .bind(&norm)
                .bind(guess.year)
                .fetch_optional(&mut *tx)
                .await?;
                let item_id = match existing {
                    Some(id) => id,
                    None => {
                        let id = ulid::Ulid::new().to_string();
                        sqlx::query(
                            "INSERT INTO items (id, kind, title, norm_title, year)
                             VALUES (?, 'movie', ?, ?, ?)",
                        )
                        .bind(&id)
                        .bind(&guess.title)
                        .bind(&norm)
                        .bind(guess.year)
                        .execute(&mut *tx)
                        .await?;
                        id
                    }
                };
                sqlx::query(
                    "INSERT INTO item_sources (module_id, collection_id, path_rel, item_id)
                     VALUES (?, ?, ?, ?)
                     ON CONFLICT (module_id, collection_id, path_rel) DO UPDATE
                     SET item_id = excluded.item_id",
                )
                .bind(module_id)
                .bind(collection_id)
                .bind(&f.path_rel)
                .bind(&item_id)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(n)
    }

    pub async fn collections(&self) -> Result<Vec<CollectionRow>> {
        let rows = sqlx::query(
            "SELECT c.module_id, c.collection_id, c.media_type,
                    (SELECT COUNT(*) FROM files f
                      WHERE f.module_id = c.module_id
                        AND f.collection_id = c.collection_id) AS file_count
             FROM collections c ORDER BY c.module_id, c.collection_id",
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let module_id: String = r.get("module_id");
                CollectionRow {
                    available: self.is_connected(&module_id),
                    module_id,
                    collection_id: r.get("collection_id"),
                    media_type: r.get("media_type"),
                    file_count: r.get("file_count"),
                }
            })
            .collect())
    }
}
