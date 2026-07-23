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

/// Archive the watch state reachable through one file, keyed by that
/// file's content identity.
const ARCHIVE_WATCH_FOR_FILE_SQL: &str = "
    INSERT OR REPLACE INTO watch_state_archive
      (user_id, size, head_xxh3, tail_xxh3, position_ms, duration_ms, played, play_count)
    SELECT w.user_id, f.size, f.head_xxh3, f.tail_xxh3,
           w.position_ms, w.duration_ms, w.played, w.play_count
    FROM files f
    JOIN item_sources s ON (s.module_id, s.collection_id, s.path_rel)
                         = (f.module_id, f.collection_id, f.path_rel)
    JOIN watch_state w ON w.item_id = s.item_id
    WHERE f.module_id = ? AND f.collection_id = ? AND f.path_rel = ?";

pub struct Registry {
    db: SqlitePool,
    /// The live mTLS allowlist (SEC-5), mirrored from the satellites table.
    allowed: kahawai_transport::mtls::AllowedCerts,
    connected: Mutex<HashMap<String, SatelliteState>>,
    /// Live capability reports from connected transcoders (TC-1); cleared
    /// on disconnect — a report is only valid while the link is up.
    transcoder_caps: Mutex<HashMap<String, serde_json::Value>>,
    tc_links: Mutex<HashMap<String, tokio::sync::mpsc::Sender<Result<kahawai_proto::v1::HubToTc, tonic::Status>>>>,
    /// Dispatched sessions per transcoder (inverse-load placement).
    tc_load: Mutex<HashMap<String, usize>>,
    /// Admin-disabled satellites: placement skips them; active sessions
    /// finish. ponytail: in-memory (an ops/testing toggle) — persist in
    /// the satellites table if drain-across-restarts is ever needed.
    disabled: Mutex<std::collections::HashSet<String>>,
    /// Command senders for connected hosts' Link streams.
    links: Mutex<HashMap<String, tokio::sync::mpsc::Sender<Result<kahawai_proto::v1::HubToHost, tonic::Status>>>>,
}

impl Registry {
    pub fn new(db: SqlitePool, allowed: kahawai_transport::mtls::AllowedCerts) -> Self {
        Self {
            db,
            allowed,
            connected: Mutex::new(HashMap::new()),
            links: Mutex::new(HashMap::new()),
            transcoder_caps: Mutex::new(HashMap::new()),
            tc_links: Mutex::new(HashMap::new()),
            tc_load: Mutex::new(HashMap::new()),
            disabled: Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Populate the allowlist from the satellites table (hub startup).
    pub async fn load_allowlist(&self) -> Result<usize> {
        let rows = sqlx::query("SELECT cert_fingerprint, module_id, disabled FROM satellites")
            .fetch_all(&self.db)
            .await?;
        let n = rows.len();
        let mut disabled = self.disabled.lock().unwrap();
        for row in rows {
            self.allowed.insert(&row.get::<String, _>("cert_fingerprint"));
            if row.get::<i64, _>("disabled") != 0 {
                disabled.insert(row.get::<String, _>("module_id"));
            }
        }
        Ok(n)
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
        self.disabled.lock().unwrap().remove(module_id);
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

    /// Record an approved satellite and admit its certificate (SEC-4/5):
    /// the DB row and the live allowlist change together, with an audit row.
    pub async fn record_satellite(
        &self,
        module_id: &str,
        module_type: &str,
        name: &str,
        cert_fingerprint: &str,
    ) -> Result<()> {
        let mut tx = self.db.begin().await?;
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
        .execute(&mut *tx)
        .await
        .context("recording satellite")?;
        sqlx::query(
            "INSERT INTO satellite_audit (module_id, fingerprint, action) VALUES (?, ?, 'enrolled')",
        )
        .bind(module_id)
        .bind(cert_fingerprint)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.allowed.insert(cert_fingerprint);
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

                // HUB-20/MH-5: the same bytes came back (any host, any
                // path) — restore archived watch state. Live rows win.
                sqlx::query(
                    "INSERT INTO watch_state
                       (user_id, item_id, position_ms, duration_ms, played, play_count)
                     SELECT user_id, ?, position_ms, duration_ms, played, play_count
                     FROM watch_state_archive
                     WHERE size = ? AND head_xxh3 = ? AND tail_xxh3 = ?
                     ON CONFLICT (user_id, item_id) DO NOTHING",
                )
                .bind(&item_id)
                .bind(f.size as i64)
                .bind(f.head_xxh3 as i64)
                .bind(f.tail_xxh3 as i64)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "DELETE FROM watch_state_archive
                     WHERE size = ? AND head_xxh3 = ? AND tail_xxh3 = ?",
                )
                .bind(f.size as i64)
                .bind(f.head_xxh3 as i64)
                .bind(f.tail_xxh3 as i64)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(n)
    }

    /// After a completed scan, drop files the scan no longer reported and
    /// items left without any source. Watch state is archived keyed to
    /// content identity first (HUB-20), so moves/renames and returning
    /// media keep their history.
    pub async fn reconcile_files(
        &self,
        module_id: &str,
        collection_id: &str,
        seen: &std::collections::HashSet<String>,
    ) -> Result<usize> {
        let known: Vec<String> = sqlx::query_scalar(
            "SELECT path_rel FROM files WHERE module_id = ? AND collection_id = ?",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_all(&self.db)
        .await?;
        let stale: Vec<&String> = known.iter().filter(|p| !seen.contains(*p)).collect();
        if stale.is_empty() {
            return Ok(0);
        }
        let mut tx = self.db.begin().await?;
        for path in &stale {
            sqlx::query(ARCHIVE_WATCH_FOR_FILE_SQL)
                .bind(module_id)
                .bind(collection_id)
                .bind(path)
                .execute(&mut *tx)
                .await?;
            for table in ["item_sources", "files"] {
                sqlx::query(&format!(
                    "DELETE FROM {table} WHERE module_id = ? AND collection_id = ? AND path_rel = ?"
                ))
                .bind(module_id)
                .bind(collection_id)
                .bind(path)
                .execute(&mut *tx)
                .await?;
            }
        }
        sqlx::query(
            "DELETE FROM items WHERE id IN (
                SELECT i.id FROM items i
                LEFT JOIN item_sources s ON s.item_id = i.id
                WHERE s.item_id IS NULL)",
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        tracing::info!(%module_id, collection = collection_id, removed = stale.len(),
            "reconciled files gone from disk");
        Ok(stale.len())
    }

    pub fn set_transcoder_caps(&self, module_id: &str, caps: &kahawai_proto::v1::CapabilityReport) {
        let json = serde_json::json!({
            "encoders": caps.encoders.iter()
                .map(|e| serde_json::json!({
                    "codec": e.codec, "element": e.element, "hardware": e.hardware,
                }))
                .collect::<Vec<_>>(),
            "max_sessions": caps.max_sessions,
        });
        self.transcoder_caps.lock().unwrap().insert(module_id.to_string(), json);
    }

    pub fn clear_transcoder_caps(&self, module_id: &str) {
        self.transcoder_caps.lock().unwrap().remove(module_id);
    }

    pub fn register_tc_link(
        &self,
        module_id: &str,
        tx: tokio::sync::mpsc::Sender<Result<kahawai_proto::v1::HubToTc, tonic::Status>>,
    ) {
        self.tc_links.lock().unwrap().insert(module_id.to_string(), tx);
    }

    pub fn unregister_tc_link(&self, module_id: &str) {
        self.tc_links.lock().unwrap().remove(module_id);
        self.tc_load.lock().unwrap().remove(module_id);
    }

    pub async fn send_to_tc(
        &self,
        module_id: &str,
        msg: kahawai_proto::v1::HubToTc,
    ) -> anyhow::Result<()> {
        let tx = self
            .tc_links
            .lock()
            .unwrap()
            .get(module_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("transcoder {module_id} not connected"))?;
        tx.send(Ok(msg)).await.map_err(|_| anyhow::anyhow!("transcoder link closed"))
    }

    pub fn tc_session_started(&self, module_id: &str) {
        *self.tc_load.lock().unwrap().entry(module_id.to_string()).or_insert(0) += 1;
    }

    pub fn tc_session_ended(&self, module_id: &str) {
        if let Some(n) = self.tc_load.lock().unwrap().get_mut(module_id) {
            *n = n.saturating_sub(1);
        }
    }

    /// Admin toggle: a disabled satellite is skipped by placement.
    /// Persisted — a drained box must not rejoin because the hub bounced.
    pub async fn set_disabled(&self, module_id: &str, disabled: bool) -> Result<()> {
        sqlx::query("UPDATE satellites SET disabled = ? WHERE module_id = ?")
            .bind(disabled as i64)
            .bind(module_id)
            .execute(&self.db)
            .await?;
        let mut set = self.disabled.lock().unwrap();
        if disabled {
            set.insert(module_id.to_string());
        } else {
            set.remove(module_id);
        }
        Ok(())
    }

    /// Placement (§4.5): capability fit ≥ hw-accel ≥ inverse load.
    /// ponytail: per-box decode capability and max_sessions enforcement
    /// come with the negotiation-aware capability report.
    pub fn pick_transcoder(&self, need_h264: bool, need_aac: bool) -> Option<String> {
        let caps = self.transcoder_caps.lock().unwrap().clone();
        let links = self.tc_links.lock().unwrap();
        let load = self.tc_load.lock().unwrap();
        let disabled = self.disabled.lock().unwrap();
        let mut candidates: Vec<(bool, usize, String)> = caps
            .iter()
            .filter(|(id, _)| links.contains_key(*id) && !disabled.contains(*id))
            .filter_map(|(id, c)| {
                let encoders = c.get("encoders")?.as_array()?;
                let has = |codec: &str| encoders.iter().any(|e| e["codec"] == codec);
                if (need_h264 && !has("h264")) || (need_aac && !has("aac")) {
                    return None;
                }
                let hw = encoders
                    .iter()
                    .any(|e| e["codec"] == "h264" && e["hardware"] == true);
                Some((hw, load.get(id).copied().unwrap_or(0), id.clone()))
            })
            .collect();
        // Most hardware, least loaded.
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        candidates.first().map(|(_, _, id)| id.clone())
    }

    /// Enrolled satellites (DB) merged with live connection state.
    pub async fn satellites_overview(&self) -> Result<Vec<serde_json::Value>> {
        let rows = sqlx::query(
            "SELECT module_id, module_type, name, cert_fingerprint, enrolled_at
             FROM satellites ORDER BY enrolled_at",
        )
        .fetch_all(&self.db)
        .await?;
        let connected = self.connected.lock().unwrap().clone();
        let caps = self.transcoder_caps.lock().unwrap().clone();
        Ok(rows
            .iter()
            .map(|r| {
                let id: String = r.get("module_id");
                let state = connected.get(&id);
                serde_json::json!({
                    "module_id": id,
                    "module_type": r.get::<String, _>("module_type"),
                    "name": r.get::<String, _>("name"),
                    "cert_fingerprint": r.get::<String, _>("cert_fingerprint"),
                    "enrolled_at": r.get::<i64, _>("enrolled_at"),
                    "connected": state.is_some_and(|s| s.connected),
                    "capabilities": caps.get(&id),
                    "disabled": self.disabled.lock().unwrap().contains(&id),
                })
            })
            .collect())
    }

    /// Delete a satellite (SEC-6/HUB-20): remove its cert from the
    /// allowlist, close its link, archive watch state by content identity,
    /// cascade-delete its collections/files/sources and orphaned items.
    /// Returns the removed fingerprint. Transient disconnects never come
    /// here.
    pub async fn delete_satellite(&self, module_id: &str) -> Result<String> {
        let fingerprint: String = sqlx::query_scalar(
            "SELECT cert_fingerprint FROM satellites WHERE module_id = ?",
        )
        .bind(module_id)
        .fetch_optional(&self.db)
        .await?
        .with_context(|| format!("no such satellite: {module_id}"))?;

        let mut tx = self.db.begin().await?;
        sqlx::query(
            "INSERT INTO satellite_audit (module_id, fingerprint, action) VALUES (?, ?, 'deleted')",
        )
        .bind(module_id)
        .bind(&fingerprint)
        .execute(&mut *tx)
        .await?;
        // Archive watch state for every file this host serves (identity-
        // keyed; restore drops it again if the item still has live sources).
        sqlx::query(
            "INSERT OR REPLACE INTO watch_state_archive
               (user_id, size, head_xxh3, tail_xxh3, position_ms, duration_ms, played, play_count)
             SELECT w.user_id, f.size, f.head_xxh3, f.tail_xxh3,
                    w.position_ms, w.duration_ms, w.played, w.play_count
             FROM files f
             JOIN item_sources s ON (s.module_id, s.collection_id, s.path_rel)
                                  = (f.module_id, f.collection_id, f.path_rel)
             JOIN watch_state w ON w.item_id = s.item_id
             WHERE f.module_id = ?",
        )
        .bind(module_id)
        .execute(&mut *tx)
        .await?;
        for sql in [
            "DELETE FROM item_sources WHERE module_id = ?",
            "DELETE FROM files WHERE module_id = ?",
            "DELETE FROM collections WHERE module_id = ?",
            "DELETE FROM satellites WHERE module_id = ?",
        ] {
            sqlx::query(sql).bind(module_id).execute(&mut *tx).await?;
        }
        sqlx::query(
            "DELETE FROM items WHERE id IN (
                SELECT i.id FROM items i
                LEFT JOIN item_sources s ON s.item_id = i.id
                WHERE s.item_id IS NULL)",
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        // Off the allowlist and off the wire: the satellite's reconnects
        // die at the TLS handshake from here on (SEC-6).
        self.allowed.remove(&fingerprint);
        self.links.lock().unwrap().remove(module_id);
        self.connected.lock().unwrap().remove(module_id);
        tracing::info!(%module_id, fingerprint = %fingerprint, "satellite deleted; cert no longer admitted");
        Ok(fingerprint)
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
