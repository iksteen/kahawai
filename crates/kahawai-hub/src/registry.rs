//! Registry (HUB-1): connection state in memory, everything else in SQLite
//! so a hub restart recovers without a rescan (NFR-3).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

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

/// What a session needs from a transcoder (derived from plan + source).
#[derive(Debug, Clone, Default)]
pub struct PlacementNeed {
    pub encode_video: bool,
    pub encode_audio: bool,
    /// Source caps names per kind (any one must be decodable).
    pub video_caps: Vec<String>,
    pub audio_caps: Vec<String>,
}

/// SEC-7: how long a renewed-but-unused fingerprint stays admitted.
pub const RENEWAL_GRACE_SECS: i64 = 24 * 3600;

fn unix_now() -> i64 {
    SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

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
    /// Live per-collection scan progress (HUB-35): last report wins.
    scan_progress: Mutex<HashMap<(String, String), ScanState>>,
    /// HUB-11 event bus: invalidation hints pushed to /api/v1/events
    /// subscribers ({kind, ...} JSON). Lagging receivers drop events —
    /// hints, not state; clients refetch what a hint names.
    events: tokio::sync::broadcast::Sender<serde_json::Value>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ScanState {
    pub scanned: u32,
    pub failed: u32,
    pub skipped: u32,
    pub complete: bool,
    #[serde(skip)]
    pub updated: SystemTime,
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
            scan_progress: Mutex::new(HashMap::new()),
            events: tokio::sync::broadcast::channel(256).0,
        }
    }

    /// Push an event hint to /api/v1/events subscribers (HUB-11).
    pub fn emit(&self, event: serde_json::Value) {
        let _ = self.events.send(event); // no subscribers = no-op
    }

    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<serde_json::Value> {
        self.events.subscribe()
    }

    pub fn update_scan_progress(
        &self,
        module_id: &str,
        collection_id: &str,
        scanned: u32,
        failed: u32,
        skipped: u32,
        complete: bool,
    ) {
        self.scan_progress.lock().unwrap().insert(
            (module_id.to_string(), collection_id.to_string()),
            ScanState { scanned, failed, skipped, complete, updated: SystemTime::now() },
        );
        self.emit(serde_json::json!({
            "kind": "scan",
            "module_id": module_id,
            "collection_id": collection_id,
            "scanned": scanned,
            "failed": failed,
            "skipped": skipped,
            "complete": complete,
        }));
    }

    /// Live scan state for the admin overview. Completed states linger a
    /// minute (so the finished counts are visible), then disappear.
    pub fn scan_state(&self, module_id: &str, collection_id: &str) -> Option<ScanState> {
        self.scan_progress
            .lock()
            .unwrap()
            .get(&(module_id.to_string(), collection_id.to_string()))
            .filter(|s| {
                !s.complete
                    || s.updated.elapsed().unwrap_or_default() < Duration::from_secs(60)
            })
            .cloned()
    }

    /// AR-5: a satellites row for the in-process mediahost so admin
    /// views and cascades treat it like any satellite. No certificate —
    /// the marker fingerprint never matches a TLS peer.
    pub async fn ensure_local_satellite(&self, module_id: &str, name: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO satellites (module_id, module_type, name, cert_fingerprint)
             VALUES (?, 'mediahost', ?, 'in-process')
             ON CONFLICT (module_id) DO UPDATE SET name = excluded.name",
        )
        .bind(module_id)
        .bind(name)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// Populate the allowlist from the satellites table (hub startup).
    /// Pending renewal fingerprints (SEC-7) are admitted while their grace
    /// holds; lapsed ones are swept here. Grace: [`RENEWAL_GRACE_SECS`].
    pub async fn load_allowlist(&self) -> Result<usize> {
        sqlx::query(
            "UPDATE satellites SET pending_fingerprint = NULL, pending_issued_at = NULL
             WHERE pending_issued_at IS NOT NULL AND pending_issued_at < unixepoch() - ?",
        )
        .bind(RENEWAL_GRACE_SECS)
        .execute(&self.db)
        .await?;
        let rows = sqlx::query(
            "SELECT cert_fingerprint, pending_fingerprint, module_id, disabled FROM satellites",
        )
        .fetch_all(&self.db)
        .await?;
        let n = rows.len();
        let mut disabled = self.disabled.lock().unwrap();
        for row in rows {
            self.allowed.insert(&row.get::<String, _>("cert_fingerprint"));
            if let Some(pending) = row.get::<Option<String>, _>("pending_fingerprint") {
                self.allowed.insert(&pending);
            }
            if row.get::<i64, _>("disabled") != 0 {
                disabled.insert(row.get::<String, _>("module_id"));
            }
        }
        Ok(n)
    }

    /// SEC-7: admit a freshly renewed certificate alongside the current one.
    /// The new fingerprint is in the DB and the live allowlist before this
    /// returns — i.e. before the certificate ever leaves the hub.
    pub async fn record_renewal(&self, module_id: &str, new_fingerprint: &str) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let old_pending: Option<Option<String>> = sqlx::query_scalar(
            "SELECT pending_fingerprint FROM satellites WHERE module_id = ?",
        )
        .bind(module_id)
        .fetch_optional(&mut *tx)
        .await?;
        anyhow::ensure!(old_pending.is_some(), "unknown satellite {module_id}");
        sqlx::query(
            "UPDATE satellites SET pending_fingerprint = ?, pending_issued_at = unixepoch()
             WHERE module_id = ?",
        )
        .bind(new_fingerprint)
        .bind(module_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO satellite_audit (module_id, fingerprint, action) VALUES (?, ?, 'renewed')",
        )
        .bind(module_id)
        .bind(new_fingerprint)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.allowed.insert(new_fingerprint);
        // A superseded pending renewal (satellite retried) is dead weight.
        if let Some(Some(old)) = old_pending {
            if old != new_fingerprint {
                self.allowed.remove(&old);
            }
        }
        Ok(())
    }

    /// MH-9: paths in an anime collection still lacking an ED2K hash.
    /// Copy-forward first: identical content identity elsewhere (renames,
    /// moves, duplicates) donates its hash — full reads happen at most
    /// once per content identity, with the files table as the journal.
    pub async fn ed2k_worklist(
        &self,
        module_id: &str,
        collection_id: &str,
    ) -> Result<Vec<String>> {
        let media_type: Option<String> = sqlx::query_scalar(
            "SELECT media_type FROM collections WHERE module_id = ? AND collection_id = ?",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_optional(&self.db)
        .await?;
        if media_type.as_deref() != Some("anime") {
            return Ok(Vec::new());
        }
        sqlx::query(
            "UPDATE files SET ed2k = (
                SELECT e.ed2k FROM files e
                WHERE e.ed2k IS NOT NULL AND e.size = files.size
                  AND e.head_xxh3 = files.head_xxh3 AND e.tail_xxh3 = files.tail_xxh3
                  AND e.oshash = files.oshash LIMIT 1)
             WHERE module_id = ? AND collection_id = ? AND ed2k IS NULL
               AND EXISTS (
                SELECT 1 FROM files e
                WHERE e.ed2k IS NOT NULL AND e.size = files.size
                  AND e.head_xxh3 = files.head_xxh3 AND e.tail_xxh3 = files.tail_xxh3
                  AND e.oshash = files.oshash)",
        )
        .bind(module_id)
        .bind(collection_id)
        .execute(&self.db)
        .await?;
        let paths = sqlx::query_scalar(
            "SELECT path_rel FROM files
             WHERE module_id = ? AND collection_id = ? AND ed2k IS NULL
             ORDER BY path_rel",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_all(&self.db)
        .await?;
        Ok(paths)
    }

    /// MH-9: store a reported hash — only if the row still describes the
    /// file that was hashed (size match; a changed file rehashes later).
    pub async fn record_ed2k(
        &self,
        module_id: &str,
        collection_id: &str,
        path_rel: &str,
        ed2k: &str,
        size: u64,
    ) -> Result<bool> {
        let n = sqlx::query(
            "UPDATE files SET ed2k = ?
             WHERE module_id = ? AND collection_id = ? AND path_rel = ? AND size = ?",
        )
        .bind(ed2k)
        .bind(module_id)
        .bind(collection_id)
        .bind(path_rel)
        .bind(size as i64)
        .execute(&self.db)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    /// MH-4 backfill: matroska files whose records predate attachment
    /// declaration. The mediahost declares them in its cheapest idle
    /// tier; "attachments":[] marks checked-and-none so the file drops
    /// out of this list.
    pub async fn attachments_worklist(
        &self,
        module_id: &str,
        collection_id: &str,
    ) -> Result<Vec<String>> {
        let media_type: Option<String> = sqlx::query_scalar(
            "SELECT media_type FROM collections WHERE module_id = ? AND collection_id = ?",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_optional(&self.db)
        .await?;
        if !matches!(media_type.as_deref(), Some("movies") | Some("series") | Some("anime")) {
            return Ok(Vec::new());
        }
        let paths = sqlx::query_scalar(
            "SELECT path_rel FROM files
             WHERE module_id = ? AND collection_id = ?
               AND json_extract(streams_json, '$.container') IN ('matroska', 'webm')
               AND json_extract(streams_json, '$.attachments') IS NULL
             ORDER BY path_rel",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_all(&self.db)
        .await?;
        Ok(paths)
    }

    /// Store a mediahost attachment declaration (size-guarded like ED2K:
    /// dropped when the row moved on). Writes into streams_json so the
    /// record looks exactly as if the scan had declared it.
    pub async fn record_file_attachments(
        &self,
        module_id: &str,
        collection_id: &str,
        path_rel: &str,
        size: u64,
        attachments_json: &str,
    ) -> Result<bool> {
        // Reject junk before it reaches the row.
        let parsed: Result<Vec<kahawai_core::media::Attachment>, _> =
            serde_json::from_str(attachments_json);
        anyhow::ensure!(parsed.is_ok(), "malformed attachments json");
        let n = sqlx::query(
            "UPDATE files SET streams_json = json_set(streams_json, '$.attachments', json(?))
             WHERE module_id = ? AND collection_id = ? AND path_rel = ? AND size = ?",
        )
        .bind(attachments_json)
        .bind(module_id)
        .bind(collection_id)
        .bind(path_rel)
        .bind(size as i64)
        .execute(&self.db)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    /// Efficiency ladder step 2: video-collection files with embedded
    /// text subtitle tracks not yet extracted — the background pre-warm
    /// worklist, drained below ED2K on the mediahost.
    pub async fn subs_worklist(
        &self,
        module_id: &str,
        collection_id: &str,
    ) -> Result<Vec<String>> {
        let media_type: Option<String> = sqlx::query_scalar(
            "SELECT media_type FROM collections WHERE module_id = ? AND collection_id = ?",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_optional(&self.db)
        .await?;
        if !matches!(media_type.as_deref(), Some("movies") | Some("series") | Some("anime")) {
            return Ok(Vec::new());
        }
        let paths = sqlx::query_scalar(
            "SELECT path_rel FROM files
             WHERE module_id = ? AND collection_id = ? AND subs_extracted = 0
               AND EXISTS (
                 SELECT 1 FROM json_each(json_extract(streams_json, '$.subtitles')) je
                 WHERE json_extract(je.value, '$.format')
                       IN ('ass','ssa','srt','subrip','text','vtt','webvtt'))
             ORDER BY path_rel",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_all(&self.db)
        .await?;
        Ok(paths)
    }

    /// Mark a file's subtitles extracted. `size` Some → guarded like
    /// ED2K results (stale reports dropped); None → unconditional
    /// (extraction errors: retrying an identical file fails identically,
    /// and a content change resets the flag via upsert).
    pub async fn set_subs_extracted(
        &self,
        module_id: &str,
        collection_id: &str,
        path_rel: &str,
        size: Option<u64>,
    ) -> Result<bool> {
        let n = match size {
            Some(size) => sqlx::query(
                "UPDATE files SET subs_extracted = 1
                 WHERE module_id = ? AND collection_id = ? AND path_rel = ? AND size = ?",
            )
            .bind(module_id)
            .bind(collection_id)
            .bind(path_rel)
            .bind(size as i64)
            .execute(&self.db)
            .await?
            .rows_affected(),
            None => sqlx::query(
                "UPDATE files SET subs_extracted = 1
                 WHERE module_id = ? AND collection_id = ? AND path_rel = ?",
            )
            .bind(module_id)
            .bind(collection_id)
            .bind(path_rel)
            .execute(&self.db)
            .await?
            .rows_affected(),
        };
        Ok(n > 0)
    }

    /// SEC-7 settlement, called on every satellite connection: reconnecting
    /// with the renewed cert retires the old fingerprint; reconnecting on
    /// the old cert after the grace lapsed retires the unused renewal.
    pub async fn settle_renewal(&self, module_id: &str, presented: &str) -> Result<()> {
        let Some(row) = sqlx::query(
            "SELECT cert_fingerprint, pending_fingerprint, pending_issued_at
             FROM satellites WHERE module_id = ?",
        )
        .bind(module_id)
        .fetch_optional(&self.db)
        .await?
        else {
            return Ok(());
        };
        let current: String = row.get("cert_fingerprint");
        let Some(pending) = row.get::<Option<String>, _>("pending_fingerprint") else {
            return Ok(());
        };
        let issued_at: i64 = row.get::<Option<i64>, _>("pending_issued_at").unwrap_or(0);

        if pending == presented {
            let mut tx = self.db.begin().await?;
            sqlx::query(
                "UPDATE satellites SET cert_fingerprint = pending_fingerprint,
                 pending_fingerprint = NULL, pending_issued_at = NULL WHERE module_id = ?",
            )
            .bind(module_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO satellite_audit (module_id, fingerprint, action)
                 VALUES (?, ?, 'renewal-promoted')",
            )
            .bind(module_id)
            .bind(&pending)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            self.allowed.remove(&current);
            tracing::info!(%module_id, retired = %current, "renewed certificate in use; old fingerprint retired");
        } else if issued_at < unix_now() - RENEWAL_GRACE_SECS {
            // Still on the old cert, grace lapsed: the renewal never landed
            // (the satellite will simply renew again while in the window).
            sqlx::query(
                "UPDATE satellites SET pending_fingerprint = NULL, pending_issued_at = NULL
                 WHERE module_id = ?",
            )
            .bind(module_id)
            .execute(&self.db)
            .await?;
            self.allowed.remove(&pending);
            tracing::warn!(%module_id, "renewal grace lapsed without reconnect; pending fingerprint retired");
        }
        Ok(())
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
        self.emit(serde_json::json!({
            "kind": "satellite", "module_id": module_id, "connected": true,
        }));
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
            self.emit(serde_json::json!({
                "kind": "satellite", "module_id": module_id, "connected": false,
            }));
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
        self.ensure_library(module_id, collection_id, media_type).await?;
        Ok(())
    }

    /// Every collection lives in at least one library: on first sight,
    /// create (or reuse) a same-named library of its type and attach.
    async fn ensure_library(
        &self,
        module_id: &str,
        collection_id: &str,
        media_type: &str,
    ) -> Result<()> {
        let assigned: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM library_collections WHERE module_id = ? AND collection_id = ?",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_one(&self.db)
        .await?;
        if assigned > 0 {
            return Ok(());
        }
        let lib: Option<(String, String)> = sqlx::query_as(
            "SELECT id, media_type FROM libraries WHERE name = ?",
        )
        .bind(collection_id)
        .fetch_optional(&self.db)
        .await?;
        let lib_id = match lib {
            Some((id, ty)) if ty == media_type => id,
            Some(_) => {
                // Name taken by a different type: disambiguate.
                self.create_library(&format!("{collection_id} ({media_type})"), media_type)
                    .await?
            }
            None => self.create_library(collection_id, media_type).await?,
        };
        self.attach_collection(&lib_id, module_id, collection_id).await?;
        Ok(())
    }

    pub async fn create_library(&self, name: &str, media_type: &str) -> Result<String> {
        anyhow::ensure!(
            matches!(media_type, "movies" | "series" | "anime" | "music"),
            "unknown media type {media_type:?}"
        );
        let id = ulid::Ulid::generate().to_string();
        sqlx::query("INSERT INTO libraries (id, name, media_type) VALUES (?, ?, ?)")
            .bind(&id)
            .bind(name)
            .bind(media_type)
            .execute(&self.db)
            .await
            .context("library name already in use")?;
        tracing::info!(library = name, media_type, "library created");
        Ok(id)
    }

    pub async fn delete_library(&self, id: &str) -> Result<bool> {
        let n = sqlx::query("DELETE FROM libraries WHERE id = ?")
            .bind(id)
            .execute(&self.db)
            .await?
            .rows_affected();
        Ok(n > 0)
    }

    /// Attach a collection — only if the types match (a movies library
    /// never contains a series collection).
    pub async fn attach_collection(
        &self,
        library_id: &str,
        module_id: &str,
        collection_id: &str,
    ) -> Result<()> {
        let lib_type: Option<String> =
            sqlx::query_scalar("SELECT media_type FROM libraries WHERE id = ?")
                .bind(library_id)
                .fetch_optional(&self.db)
                .await?;
        let lib_type = lib_type.context("no such library")?;
        let col_type: Option<String> = sqlx::query_scalar(
            "SELECT media_type FROM collections WHERE module_id = ? AND collection_id = ?",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_optional(&self.db)
        .await?;
        let col_type = col_type.context("no such collection")?;
        anyhow::ensure!(
            lib_type == col_type,
            "type mismatch: library is {lib_type}, collection is {col_type}"
        );
        sqlx::query(
            "INSERT OR IGNORE INTO library_collections (library_id, module_id, collection_id)
             VALUES (?, ?, ?)",
        )
        .bind(library_id)
        .bind(module_id)
        .bind(collection_id)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn detach_collection(
        &self,
        library_id: &str,
        module_id: &str,
        collection_id: &str,
    ) -> Result<bool> {
        let n = sqlx::query(
            "DELETE FROM library_collections
             WHERE library_id = ? AND module_id = ? AND collection_id = ?",
        )
        .bind(library_id)
        .bind(module_id)
        .bind(collection_id)
        .execute(&self.db)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    /// Libraries with their member collections, for the admin UI.
    pub async fn libraries_overview(&self) -> Result<Vec<serde_json::Value>> {
        let libs = sqlx::query("SELECT id, name, media_type FROM libraries ORDER BY name")
            .fetch_all(&self.db)
            .await?;
        let members = sqlx::query(
            "SELECT lc.library_id, lc.module_id, lc.collection_id, s.name AS host_name
             FROM library_collections lc
             LEFT JOIN satellites s ON s.module_id = lc.module_id
             ORDER BY lc.collection_id",
        )
        .fetch_all(&self.db)
        .await?;
        Ok(libs
            .iter()
            .map(|l| {
                let id: String = l.get("id");
                let cols: Vec<serde_json::Value> = members
                    .iter()
                    .filter(|m| m.get::<String, _>("library_id") == id)
                    .map(|m| {
                        serde_json::json!({
                            "module_id": m.get::<String, _>("module_id"),
                            "collection_id": m.get::<String, _>("collection_id"),
                            "host_name": m.get::<Option<String>, _>("host_name"),
                        })
                    })
                    .collect();
                serde_json::json!({
                    "id": id,
                    "name": l.get::<String, _>("name"),
                    "media_type": l.get::<String, _>("media_type"),
                    "collections": cols,
                })
            })
            .collect())
    }

    /// All known collections (for the admin attach picker).
    pub async fn collections_overview(&self) -> Result<Vec<serde_json::Value>> {
        let rows = sqlx::query(
            "SELECT c.module_id, c.collection_id, c.media_type, s.name AS host_name
             FROM collections c LEFT JOIN satellites s ON s.module_id = c.module_id
             ORDER BY c.collection_id",
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let (module_id, collection_id) =
                    (r.get::<String, _>("module_id"), r.get::<String, _>("collection_id"));
                let scan = self.scan_state(&module_id, &collection_id);
                serde_json::json!({
                    "module_id": module_id,
                    "collection_id": collection_id,
                    "media_type": r.get::<String, _>("media_type"),
                    "host_name": r.get::<Option<String>, _>("host_name"),
                    "connected": self.is_connected(&module_id),
                    "scan": scan,
                })
            })
            .collect())
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
        let resolve_series =
            matches!(media_type.as_deref(), Some("series") | Some("anime"));
        let anime = media_type.as_deref() == Some("anime");
        let resolve_music = media_type.as_deref() == Some("music");

        let mut tx = self.db.begin().await?;
        let n = files.len();
        for f in files {
            sqlx::query(
                "INSERT INTO files
                   (module_id, collection_id, path_rel, size, mtime_unix,
                    head_xxh3, tail_xxh3, oshash, streams_json)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT (module_id, collection_id, path_rel) DO UPDATE SET
                   ed2k = CASE WHEN excluded.size = files.size
                                AND excluded.mtime_unix = files.mtime_unix
                               THEN files.ed2k ELSE NULL END,
                   subs_extracted = CASE WHEN excluded.size = files.size
                                          AND excluded.mtime_unix = files.mtime_unix
                                         THEN files.subs_extracted ELSE 0 END,
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

            // Resolve to a playable item: movies map straight to a
            // movie item; series files map to an episode under a show.
            let mut source_part: Option<u32> = None;
            let resolved_item: Option<String> = if resolve_movies {
                let filename = f.path_rel.rsplit('/').next().unwrap_or(&f.path_rel);
                let guess = names::parse_movie(filename);
                source_part = guess.part;
                let norm = names::normalize_title(&guess.title);
                let existing: Option<String> = sqlx::query_scalar(
                    "SELECT id FROM items WHERE kind = 'movie' AND norm_title = ? AND year IS ?",
                )
                .bind(&norm)
                .bind(guess.year)
                .fetch_optional(&mut *tx)
                .await?;
                Some(match existing {
                    Some(id) => id,
                    None => {
                        let id = ulid::Ulid::generate().to_string();
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
                })
            } else if resolve_music {
                // Tags win (the scanner extracted them); the Lidarr
                // filename layout is the fallback for untagged rips.
                let info: kahawai_core::media::MediaInfo =
                    serde_json::from_str(&f.streams_json).unwrap_or_default();
                let tags = &info.tags;
                let tag = |k: &str| tags.get(k).map(|s| s.trim()).filter(|s| !s.is_empty());
                let parsed = names::parse_music(&f.path_rel);
                let artist = tag("artist")
                    .map(str::to_string)
                    .or_else(|| parsed.as_ref().map(|g| g.artist.clone()));
                let album = tag("album")
                    .map(str::to_string)
                    .or_else(|| parsed.as_ref().map(|g| g.album.clone()));
                let track_no: Option<u32> = tags
                    .get("track_number")
                    .and_then(|v| v.parse().ok())
                    .or_else(|| parsed.as_ref().map(|g| g.track));
                let title = tag("title")
                    .map(str::to_string)
                    .or_else(|| parsed.as_ref().map(|g| g.title.clone()));
                let disc: Option<u32> = tags.get("disc_number").and_then(|v| v.parse().ok());
                let album_year = parsed.as_ref().and_then(|g| g.album_year);
                match (artist, album, track_no, title) {
                    (Some(artist), Some(album), Some(track), Some(title)) => {
                        let album_norm = names::normalize_title(&album);
                        let existing: Option<String> = sqlx::query_scalar(
                            "SELECT id FROM items
                             WHERE kind = 'album' AND norm_title = ?
                               AND LOWER(artist) = LOWER(?)",
                        )
                        .bind(&album_norm)
                        .bind(&artist)
                        .fetch_optional(&mut *tx)
                        .await?;
                        let album_id = match existing {
                            Some(id) => id,
                            None => {
                                let id = ulid::Ulid::generate().to_string();
                                sqlx::query(
                                    "INSERT INTO items (id, kind, title, norm_title, year, artist,
                                                        norm_artist)
                                     VALUES (?, 'album', ?, ?, ?, ?, ?)",
                                )
                                .bind(&id)
                                .bind(&album)
                                .bind(&album_norm)
                                .bind(album_year)
                                .bind(&artist)
                                // Folded like the search needle is, or an
                                // accented artist can never be found.
                                .bind(crate::enrich::fold(&artist))
                                .execute(&mut *tx)
                                .await?;
                                id
                            }
                        };
                        // Album artist for dedup is normalized lowercase;
                        // display keeps the first-seen casing.
                        let existing_track: Option<String> = sqlx::query_scalar(
                            "SELECT id FROM items
                             WHERE kind = 'track' AND parent_id = ?
                               AND season IS ? AND episode = ?",
                        )
                        .bind(&album_id)
                        .bind(disc)
                        .bind(track)
                        .fetch_optional(&mut *tx)
                        .await?;
                        Some(match existing_track {
                            Some(id) => id,
                            None => {
                                let id = ulid::Ulid::generate().to_string();
                                sqlx::query(
                                    "INSERT INTO items
                                       (id, kind, title, norm_title, year,
                                        parent_id, season, episode, artist, norm_artist)
                                     VALUES (?, 'track', ?, ?, NULL, ?, ?, ?, ?, ?)",
                                )
                                .bind(&id)
                                .bind(&title)
                                .bind(names::normalize_title(&title))
                                .bind(&album_id)
                                .bind(disc)
                                .bind(track)
                                .bind(&artist)
                                .bind(crate::enrich::fold(&artist))
                                .execute(&mut *tx)
                                .await?;
                                id
                            }
                        })
                    }
                    _ => {
                        tracing::debug!(path = %f.path_rel, "no music identity; unresolved");
                        None
                    }
                }
            } else if resolve_series {
                let guess = if anime {
                    names::parse_anime(&f.path_rel)
                } else {
                    names::parse_episode(&f.path_rel)
                };
                match guess {
                    None if anime
                        && let filename = f.path_rel.rsplit('/').next().unwrap_or(&f.path_rel)
                        && let mg = names::parse_movie(filename)
                        && mg.year.is_some() =>
                    {
                        // Anime movies (HUB-30): no episode shape, but a
                        // credible "Title (Year)" resolves as a movie —
                        // Ghibli films et al. Yearless non-parses (NCOP/
                        // NCED extras) stay bare; ed2k matching will
                        // identify those precisely later.
                        source_part = mg.part;
                        let norm = names::normalize_title(&mg.title);
                        let existing: Option<String> = sqlx::query_scalar(
                            "SELECT id FROM items
                             WHERE kind = 'movie' AND norm_title = ? AND year IS ?",
                        )
                        .bind(&norm)
                        .bind(mg.year)
                        .fetch_optional(&mut *tx)
                        .await?;
                        Some(match existing {
                            Some(id) => id,
                            None => {
                                let id = ulid::Ulid::generate().to_string();
                                sqlx::query(
                                    "INSERT INTO items (id, kind, title, norm_title, year)
                                     VALUES (?, 'movie', ?, ?, ?)",
                                )
                                .bind(&id)
                                .bind(&mg.title)
                                .bind(&norm)
                                .bind(mg.year)
                                .execute(&mut *tx)
                                .await?;
                                id
                            }
                        })
                    }
                    None => {
                        // Unparseable stays a bare file (review queue,
                        // later) — never guess an identity.
                        tracing::debug!(path = %f.path_rel, "no episode parse; unresolved");
                        None
                    }
                    Some(g) => {
                        let norm = names::normalize_title(&g.show_title);
                        let show: Option<String> = sqlx::query_scalar(
                            "SELECT id FROM items
                             WHERE kind = 'show' AND norm_title = ? AND year IS ?",
                        )
                        .bind(&norm)
                        .bind(g.show_year)
                        .fetch_optional(&mut *tx)
                        .await?;
                        let show_id = match show {
                            Some(id) => id,
                            None => {
                                let id = ulid::Ulid::generate().to_string();
                                sqlx::query(
                                    "INSERT INTO items (id, kind, title, norm_title, year)
                                     VALUES (?, 'show', ?, ?, ?)",
                                )
                                .bind(&id)
                                .bind(&g.show_title)
                                .bind(&norm)
                                .bind(g.show_year)
                                .execute(&mut *tx)
                                .await?;
                                id
                            }
                        };
                        let ep: Option<String> = sqlx::query_scalar(
                            "SELECT id FROM items
                             WHERE kind = 'episode' AND parent_id = ?
                               AND season IS ? AND episode = ?",
                        )
                        .bind(&show_id)
                        .bind(g.season)
                        .bind(g.episode)
                        .fetch_optional(&mut *tx)
                        .await?;
                        Some(match ep {
                            Some(id) => id,
                            None => {
                                let id = ulid::Ulid::generate().to_string();
                                let title = g
                                    .episode_title
                                    .clone()
                                    .unwrap_or_else(|| format!("Episode {}", g.episode));
                                sqlx::query(
                                    "INSERT INTO items
                                       (id, kind, title, norm_title, year,
                                        parent_id, season, episode)
                                     VALUES (?, 'episode', ?, ?, NULL, ?, ?, ?)",
                                )
                                .bind(&id)
                                .bind(&title)
                                .bind(names::normalize_title(&title))
                                .bind(&show_id)
                                .bind(g.season)
                                .bind(g.episode)
                                .execute(&mut *tx)
                                .await?;
                                id
                            }
                        })
                    }
                }
            } else {
                None
            };

            if let Some(item_id) = resolved_item {
                sqlx::query(
                    "INSERT INTO item_sources (module_id, collection_id, path_rel, item_id, part)
                     VALUES (?, ?, ?, ?, ?)
                     ON CONFLICT (module_id, collection_id, path_rel) DO UPDATE
                     SET item_id = excluded.item_id, part = excluded.part",
                )
                .bind(module_id)
                .bind(collection_id)
                .bind(&f.path_rel)
                .bind(&item_id)
                .bind(source_part)
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
        // Re-resolution can repoint every source away from an item
        // (multi-part regrouping, better parses) without any file being
        // deleted — sweep orphans here, not only in reconciliation.
        sqlx::query(
            "DELETE FROM items WHERE kind NOT IN ('show', 'album') AND id IN (
                SELECT i.id FROM items i
                LEFT JOIN item_sources s ON s.item_id = i.id
                WHERE s.item_id IS NULL)",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM items WHERE kind IN ('show', 'album') AND id NOT IN (
                SELECT DISTINCT p.parent_id FROM items p WHERE p.parent_id IS NOT NULL)",
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(n)
    }

    /// After a completed scan, drop files the scan no longer reported and
    /// items left without any source. Watch state is archived keyed to
    /// content identity first (HUB-20), so moves/renames and returning
    /// media keep their history.
    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.db)
            .await?)
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES (?, ?)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn collection_sync_version(
        &self,
        module_id: &str,
        collection_id: &str,
    ) -> Result<u64> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT sync_version FROM collections WHERE module_id = ? AND collection_id = ?",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_optional(&self.db)
        .await?
        .unwrap_or(0) as u64)
    }

    pub async fn set_collection_sync_version(
        &self,
        module_id: &str,
        collection_id: &str,
        version: u64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE collections SET sync_version = ? WHERE module_id = ? AND collection_id = ?",
        )
        .bind(version as i64)
        .bind(module_id)
        .bind(collection_id)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// (path, size, mtime) for every known file of a collection — the
    /// incremental-rescan manifest (MH-5).
    pub async fn file_stats(
        &self,
        module_id: &str,
        collection_id: &str,
    ) -> Result<Vec<kahawai_proto::v1::FileStat>> {
        let rows = sqlx::query(
            "SELECT path_rel, size, mtime_unix,
                    COALESCE(json_extract(streams_json, '$.nfo'), '') AS nfo,
                    COALESCE(json_extract(streams_json, '$.artwork'), '') AS art
             FROM files WHERE module_id = ? AND collection_id = ?",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| kahawai_proto::v1::FileStat {
                path_rel: r.get("path_rel"),
                size: r.get::<i64, _>("size") as u64,
                mtime_unix: r.get("mtime_unix"),
                sidecars: Self::sidecar_sig(&r.get::<String, _>("nfo"), &r.get::<String, _>("art")),
            })
            .collect())
    }

    /// One line describing a file's sidecars, compared verbatim on both
    /// sides. Order is fixed so the same pair always spells the same way.
    pub fn sidecar_sig(nfo: &str, artwork: &str) -> String {
        if nfo.is_empty() && artwork.is_empty() {
            return String::new();
        }
        format!("n:{nfo}|a:{artwork}")
    }

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
                // Safe by construction: `table` comes from the literal array
                // above, never from a caller.
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DELETE FROM {table} WHERE module_id = ? AND collection_id = ? AND path_rel = ?"
                )))
                .bind(module_id)
                .bind(collection_id)
                .bind(path)
                .execute(&mut *tx)
                .await?;
            }
        }
        sqlx::query(
            "DELETE FROM items WHERE kind NOT IN ('show', 'album') AND id IN (
                SELECT i.id FROM items i
                LEFT JOIN item_sources s ON s.item_id = i.id
                WHERE s.item_id IS NULL)",
        )
        .execute(&mut *tx)
        .await?;
        // Shows and albums never have direct sources; they die of
        // childlessness.
        sqlx::query(
            "DELETE FROM items WHERE kind IN ('show', 'album') AND id NOT IN (
                SELECT DISTINCT parent_id FROM items WHERE parent_id IS NOT NULL)",
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
            "decode_caps": caps.decode_caps,
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

    /// Placement (§4.5): capability fit (encoders AND source decoders)
    /// ≥ capacity ≥ hw-accel ≥ inverse load.
    pub fn pick_transcoder(&self, need: &PlacementNeed) -> Option<String> {
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
                if (need.encode_video && !has("h264")) || (need.encode_audio && !has("aac")) {
                    return None;
                }
                // Decode fit: the box must decode at least one source
                // stream of each kind it will encode. Empty inventory =
                // older satellite that didn't report; assume capable
                // (OPS-7 tolerance).
                let decoders: Vec<&str> = c
                    .get("decode_caps")
                    .and_then(|d| d.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                let can = |wanted: &[String]| {
                    decoders.is_empty() || wanted.iter().any(|w| decoders.contains(&w.as_str()))
                };
                if (need.encode_video && !can(&need.video_caps))
                    || (need.encode_audio && !can(&need.audio_caps))
                {
                    return None;
                }
                let current = load.get(id).copied().unwrap_or(0);
                let max = c.get("max_sessions").and_then(|m| m.as_u64()).unwrap_or(0) as usize;
                if max > 0 && current >= max {
                    return None; // at capacity (TC-6)
                }
                let hw = encoders
                    .iter()
                    .any(|e| e["codec"] == "h264" && e["hardware"] == true);
                Some((hw, current, id.clone()))
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
            "DELETE FROM items WHERE kind NOT IN ('show', 'album') AND id IN (
                SELECT i.id FROM items i
                LEFT JOIN item_sources s ON s.item_id = i.id
                WHERE s.item_id IS NULL)",
        )
        .execute(&mut *tx)
        .await?;
        // Shows and albums never have direct sources; they die of
        // childlessness.
        sqlx::query(
            "DELETE FROM items WHERE kind IN ('show', 'album') AND id NOT IN (
                SELECT DISTINCT parent_id FROM items WHERE parent_id IS NOT NULL)",
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
