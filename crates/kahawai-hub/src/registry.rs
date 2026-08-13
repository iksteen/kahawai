//! Registry (HUB-1): connection state in memory, everything else in SQLite
//! so a hub restart recovers without a rescan (NFR-3).
//!
//! Item identity is `(module_id, collection_id, item_id)`: a library composes
//! collections and never changes, clones, or merges their items. A physical file
//! has one stable integer id and one optional `collection_roots` reference.
//! Exact-root adoption assigns that reference without rewriting paths or any
//! dependent row. Protocol values still use `(root_token, path_rel)` and are
//! translated only at the database boundary.

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
    /// The binary's build stamp from its Hello (commit + date).
    pub build: String,
    pub connected: bool,
    pub last_seen: SystemTime,
}

pub struct FileUpsertRecord {
    pub root_token: String,
    pub path_rel: String,
    pub size: u64,
    pub mtime_unix: i64,
    pub head_xxh3: u64,
    pub tail_xxh3: u64,
    pub oshash: u64,
    pub streams_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourcePath {
    pub root_token: String,
    pub path_rel: String,
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
    JOIN watch_state w ON w.item_id = f.item_id
    WHERE f.module_id = ? AND f.collection_id = ? AND f.path_rel = ?";

/// What a session needs from a transcoder (derived from plan + source).
#[derive(Debug, Clone, Default)]
pub struct PlacementNeed {
    pub encode_video: bool,
    pub encode_audio: bool,
    /// Source caps names per kind (any one must be decodable).
    pub video_caps: Vec<String>,
    pub audio_caps: Vec<String>,
    /// HUB-15a: the plan tone-maps — prefer a box reporting the GL
    /// segment (preference, not filter).
    pub needs_tonemap: bool,
    /// HUB-32a: the plan burns ASS subtitles. A HARD filter, unlike
    /// tone-map, because there is no honest degradation: dropping the
    /// burn would silently hand back a video with no subtitles at all.
    /// `assrender` is genuinely absent on some boxes (macOS here), so
    /// this is a real constraint and not a formality.
    pub needs_ass_burn: bool,
    /// HUB-15b: the encode TARGET codec ("h264"/"hevc"/"av1", empty =
    /// any video encoder qualifies). A HARD filter, unlike tone-map: a
    /// box without the target's encoder cannot degrade gracefully.
    pub video_codec: String,
    /// Same for audio ("aac"/"opus", empty = any).
    pub audio_codec: String,
    /// HUB-36: the kind of work this is (`crate::pace::work_class`), or
    /// None when there is no encode to predict. Placement looks up what
    /// each box has been MEASURED to achieve on exactly this.
    pub work_class: Option<String>,
    /// Source bitrate, for the link term of the prediction: a box that
    /// cannot pull the bytes fast enough cannot produce fast enough,
    /// however quick its encoder.
    pub source_kbps: Option<u32>,
}

/// Where a session should run, and how fast that is expected to go.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    /// `Some(module_id)` = dispatch to that satellite, `None` = run in
    /// the hub's own supervised worker.
    pub target: Option<String>,
    /// False when video work has neither a suitable satellite nor AIO's
    /// full local executor. `target = None` alone means local (including
    /// ordinary hub audio work), so absence needs separate representation.
    pub available: bool,
    /// Realtime multiple this placement is expected to sustain. None
    /// when nothing about this box and this work has been measured —
    /// which is NOT the same as slow, and is treated as capable.
    pub predicted: Option<f32>,
}

/// Below this, a box is not keeping ahead of a viewer with any margin.
/// Not 1.0: a box that exactly matches realtime stalls the moment
/// anything else happens on it.
pub const SUSTAINS: f32 = 1.2;

/// Does this prediction clear the bar? An unmeasured box counts as
/// sustaining — refusing work for lack of evidence would leave a fresh
/// fleet unused, and the first session it runs is what produces the
/// evidence.
fn sustains(predicted: Option<f32>) -> bool {
    predicted.is_none_or(|p| p >= SUSTAINS)
}

/// SEC-7: how long a renewed-but-unused fingerprint stays admitted.
pub const RENEWAL_GRACE_SECS: i64 = 24 * 3600;

/// Does this ELEMENT produce this codec? The local benchmark is keyed
/// by element (a box that gains a hardware encoder must not inherit the
/// software one's number), so the codec has to be read back off the
/// name. Substrings rather than a table: every family spells the codec
/// into the element (`nvh264enc`, `x265enc`, `vtenc_h265_hw`,
/// `svtav1enc`), and an unknown element simply matches nothing and is
/// left out of the estimate.
fn element_encodes(element: &str, codec: &str) -> bool {
    let e = element.to_ascii_lowercase();
    match codec {
        "h264" => e.contains("264"),
        "hevc" => e.contains("265") || e.contains("hevc"),
        "av1" => e.contains("av1"),
        _ => false,
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

pub struct Registry {
    db: SqlitePool,
    /// The live mTLS allowlist (SEC-5), mirrored from the satellites table.
    allowed: kahawai_transport::mtls::AllowedCerts,
    connected: Mutex<HashMap<String, SatelliteState>>,
    /// Live capability reports from connected transcoders (TC-1); cleared
    /// on disconnect — a report is only valid while the link is up.
    transcoder_caps: Mutex<HashMap<String, serde_json::Value>>,
    /// HUB-36: what AIO's optional full local transcoder measured about
    /// itself. Plain hub never fills this: its local worker is limited to
    /// remux and audio-only transcode, neither of which needs video pace.
    local_bench: Mutex<Option<kahawai_media::bench::BenchResults>>,
    /// Structural startup choice for FULL local video execution. The hub's
    /// lightweight remux/audio worker is always available; only AIO may add
    /// video encode, tone-map and subtitle burn-in here.
    local_video_executor_enabled: bool,
    tc_links: Mutex<
        HashMap<
            String,
            tokio::sync::mpsc::Sender<Result<kahawai_proto::v1::HubToTc, tonic::Status>>,
        >,
    >,
    /// Dispatched sessions per transcoder (inverse-load placement).
    tc_load: Mutex<HashMap<String, usize>>,
    /// HUB-36: measured pace per `(module_id, work_class)`, loaded from
    /// `transcoder_pace` at startup and written through on every fold.
    /// In memory because placement is synchronous and must not await a
    /// query to choose a box.
    tc_pace: Mutex<HashMap<(String, String), f64>>,
    /// Source-plane bytes/sec per transcoder, as IT measured. Deliberately
    /// NOT persisted (see the pace module doc): a rate describes one
    /// connection over one network, and a stale one lies confidently.
    /// Cleared on disconnect for the same reason.
    tc_link_rate: Mutex<HashMap<String, u64>>,
    /// Admin-disabled satellites: placement skips them; active sessions
    /// finish. Persisted in `satellites.disabled` and read back at startup by
    /// `load_allowlist`, so a drain survives a hub restart — the note that once
    /// stood here calling it a throwaway in-memory toggle is what made clearing
    /// it inside `unregister_link` look free. Only `set_disabled` and
    /// `delete_satellite` may touch it.
    disabled: Mutex<std::collections::HashSet<String>>,
    /// Command senders for connected hosts' Link streams.
    links: Mutex<
        HashMap<
            String,
            tokio::sync::mpsc::Sender<Result<kahawai_proto::v1::HubToHost, tonic::Status>>,
        >,
    >,
    /// Live per-collection scan progress (HUB-35): last report wins.
    scan_progress: Mutex<HashMap<(String, String), ScanState>>,
    /// Deep-refresh marks: the next manifest request for (module,
    /// collection) is answered EMPTY, so the host re-probes every file
    /// (first-scan semantics — works with any satellite version). This
    /// is how pre-extension streams_json rows pick up newly probed
    /// facts (HDR, profile/level): the incremental scan skips
    /// stat-unchanged files by design and would never heal them.
    deep_rescan: Mutex<std::collections::HashSet<(String, String)>>,
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
            local_bench: Mutex::new(None),
            local_video_executor_enabled: false,
            tc_links: Mutex::new(HashMap::new()),
            tc_load: Mutex::new(HashMap::new()),
            tc_pace: Mutex::new(HashMap::new()),
            tc_link_rate: Mutex::new(HashMap::new()),
            disabled: Mutex::new(std::collections::HashSet::new()),
            scan_progress: Mutex::new(HashMap::new()),
            deep_rescan: Mutex::new(std::collections::HashSet::new()),
            events: tokio::sync::broadcast::channel(256).0,
        }
    }

    /// Set whether AIO may perform VIDEO encode work in its own worker.
    /// Applied while constructing the registry, before it is shared.
    pub fn with_local_video_executor(mut self, enabled: bool) -> Self {
        self.local_video_executor_enabled = enabled;
        self
    }

    pub fn local_video_executor_enabled(&self) -> bool {
        self.local_video_executor_enabled
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
            ScanState {
                scanned,
                failed,
                skipped,
                complete,
                updated: SystemTime::now(),
            },
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
                !s.complete || s.updated.elapsed().unwrap_or_default() < Duration::from_secs(60)
            })
            .cloned()
    }

    /// AR-5: a satellites row for the in-process mediahost so admin
    /// views and cascades treat it like any satellite. No certificate —
    /// the marker fingerprint never matches a TLS peer.
    /// The in-process mediahost's stand-in for a certificate fingerprint.
    /// It has none: AR-5 replaces the link's transport with channels, so
    /// there is no TLS identity to pin, admit or revoke. Anything that
    /// means "enrolled satellite" must test for this first.
    pub const IN_PROCESS: &str = "in-process";

    pub async fn ensure_local_satellite(&self, module_id: &str, name: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO satellites (module_id, module_type, name, cert_fingerprint)
             VALUES (?, 'mediahost', ?, ?)
             ON CONFLICT (module_id) DO UPDATE SET name = excluded.name",
        )
        .bind(module_id)
        .bind(name)
        .bind(Self::IN_PROCESS)
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
            self.allowed
                .insert(&row.get::<String, _>("cert_fingerprint"));
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
        let old_pending: Option<Option<String>> =
            sqlx::query_scalar("SELECT pending_fingerprint FROM satellites WHERE module_id = ?")
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
        if let Some(Some(old)) = old_pending
            && old != new_fingerprint
        {
            self.allowed.remove(&old);
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
    ) -> Result<Vec<SourcePath>> {
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
        self.source_worklist("ed2k IS NULL", module_id, collection_id)
            .await
    }

    async fn source_worklist(
        &self,
        predicate: &'static str,
        module_id: &str,
        collection_id: &str,
    ) -> Result<Vec<SourcePath>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT r.root_token, f.path_rel FROM files f
             JOIN collection_roots r ON r.id=f.root_id
             WHERE f.module_id = ? AND f.collection_id = ?
               AND ({predicate})
             ORDER BY r.root_token, f.path_rel"
        )))
        .bind(module_id)
        .bind(collection_id)
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| SourcePath {
                root_token: r.get("root_token"),
                path_rel: r.get("path_rel"),
            })
            .collect())
    }

    /// Resolve protocol exact-source identity to the one physical row id.
    pub async fn source_id(
        &self,
        module_id: &str,
        collection_id: &str,
        root_token: &str,
        path_rel: &str,
    ) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT f.id FROM files f JOIN collection_roots r ON r.id=f.root_id
              WHERE f.module_id=? AND f.collection_id=?
                AND r.root_token=? AND r.configured=1 AND f.path_rel=?",
        )
        .bind(module_id)
        .bind(collection_id)
        .bind(root_token)
        .bind(path_rel)
        .fetch_optional(&self.db)
        .await?)
    }

    /// MH-9: store a reported hash — only if the row still describes the
    /// file that was hashed (size match; a changed file rehashes later).
    pub async fn record_ed2k(
        &self,
        module_id: &str,
        collection_id: &str,
        root_token: &str,
        path_rel: &str,
        ed2k: &str,
        size: u64,
    ) -> Result<bool> {
        let Some(source_id) = self
            .source_id(module_id, collection_id, root_token, path_rel)
            .await?
        else {
            return Ok(false);
        };
        let n = sqlx::query("UPDATE files SET ed2k=? WHERE id=? AND size=?")
            .bind(ed2k)
            .bind(source_id)
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
    ) -> Result<Vec<SourcePath>> {
        let media_type: Option<String> = sqlx::query_scalar(
            "SELECT media_type FROM collections WHERE module_id = ? AND collection_id = ?",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_optional(&self.db)
        .await?;
        if !matches!(
            media_type.as_deref(),
            Some("movies") | Some("series") | Some("anime")
        ) {
            return Ok(Vec::new());
        }
        self.source_worklist(
            "json_extract(streams_json, '$.container') IN ('matroska', 'webm')
             AND json_extract(streams_json, '$.attachments') IS NULL",
            module_id,
            collection_id,
        )
        .await
    }

    /// Files whose longest keyframe gap was never measured — rows
    /// scanned before it existed. Any container: the mediahost decides
    /// what it can read, and a file it cannot index reports UNKNOWN so
    /// the row stops coming back (a `-1` sentinel, since JSON null and
    /// "column absent" are the same query).
    ///
    /// Video only: the value bounds a video segment's length and means
    /// nothing for a music file.
    pub async fn keyframe_worklist(
        &self,
        module_id: &str,
        collection_id: &str,
    ) -> Result<Vec<SourcePath>> {
        self.source_worklist(
            "json_extract(streams_json, '$.video[0].codec') IS NOT NULL
             AND json_extract(streams_json, '$.video[0].max_keyframe_interval_ms') IS NULL",
            module_id,
            collection_id,
        )
        .await
    }

    /// Store a measured keyframe interval, size-guarded like the others.
    /// `None` means measured-and-unknown and is stored as -1: it has to
    /// be distinguishable from "never measured", or every unreadable
    /// file returns in the worklist forever. Readers treat any negative
    /// value as unknown.
    pub async fn record_file_keyframe_interval(
        &self,
        module_id: &str,
        collection_id: &str,
        root_token: &str,
        path_rel: &str,
        size: u64,
        ms: Option<u32>,
    ) -> Result<bool> {
        let Some(source_id) = self
            .source_id(module_id, collection_id, root_token, path_rel)
            .await?
        else {
            return Ok(false);
        };
        let n = sqlx::query(
            "UPDATE files SET streams_json=json_set(streams_json,
                '$.video[0].max_keyframe_interval_ms',?)
              WHERE id=? AND size=?
                AND json_extract(streams_json,'$.video[0].codec') IS NOT NULL",
        )
        .bind(ms.map(|v| v as i64).unwrap_or(-1))
        .bind(source_id)
        .bind(size as i64)
        .execute(&self.db)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    /// Exact video sources whose PAR/orientation/display geometry has not been
    /// targeted yet. A recorded success or failure is terminal for this source
    /// revision; a later FileUpsert replaces the JSON and makes changed content
    /// eligible again without a catalogue-wide reset.
    pub async fn video_geometry_worklist(
        &self,
        module_id: &str,
        collection_id: &str,
    ) -> Result<Vec<SourcePath>> {
        self.source_worklist(
            "json_extract(streams_json, '$.video[0].codec') IS NOT NULL
             AND COALESCE(json_extract(streams_json, '$.video_geometry_probed'),0)=0",
            module_id,
            collection_id,
        )
        .await
    }

    /// Store one targeted result against the exact stable source row. The size
    /// guard rejects an answer raced by content replacement. Failure is data,
    /// not silence: it prevents a corrupt file becoming an endless idle loop.
    #[allow(clippy::too_many_arguments)] // exact source tuple + stale-result guard + payload
    pub async fn record_file_video_geometry(
        &self,
        module_id: &str,
        collection_id: &str,
        root_token: &str,
        path_rel: &str,
        size: u64,
        geometry_json: &str,
        error: &str,
    ) -> Result<bool> {
        let Some(source_id) = self
            .source_id(module_id, collection_id, root_token, path_rel)
            .await?
        else {
            return Ok(false);
        };
        let geometry: Vec<kahawai_core::media::VideoGeometry> = if error.is_empty() {
            serde_json::from_str(geometry_json).context("invalid video geometry result")?
        } else {
            Vec::new()
        };
        let mut tx = self.db.begin().await?;
        let Some(mut info) =
            sqlx::query_scalar::<_, String>("SELECT streams_json FROM files WHERE id=? AND size=?")
                .bind(source_id)
                .bind(size as i64)
                .fetch_optional(&mut *tx)
                .await?
                .map(|json| serde_json::from_str::<kahawai_core::media::MediaInfo>(&json))
                .transpose()?
        else {
            return Ok(false);
        };
        if error.is_empty() {
            anyhow::ensure!(
                geometry.len() == info.video.len(),
                "geometry stream count {} does not match stored video stream count {}",
                geometry.len(),
                info.video.len()
            );
            for (video, value) in info.video.iter_mut().zip(geometry) {
                video.pixel_aspect_ratio = Some(value.pixel_aspect_ratio);
                video.orientation = Some(value.orientation);
                video.display_width = Some(value.display_width);
                video.display_height = Some(value.display_height);
            }
            info.video_geometry_error = None;
        } else {
            info.video_geometry_error = Some(error.to_string());
        }
        info.video_geometry_probed = true;
        let n = sqlx::query("UPDATE files SET streams_json=? WHERE id=? AND size=?")
            .bind(serde_json::to_string(&info)?)
            .bind(source_id)
            .bind(size as i64)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(n > 0)
    }

    /// Store a mediahost attachment declaration (size-guarded like ED2K:
    /// dropped when the row moved on). Writes into streams_json so the
    /// record looks exactly as if the scan had declared it.
    pub async fn record_file_attachments(
        &self,
        module_id: &str,
        collection_id: &str,
        root_token: &str,
        path_rel: &str,
        size: u64,
        attachments_json: &str,
    ) -> Result<bool> {
        // Reject junk before it reaches the row.
        let parsed: Result<Vec<kahawai_core::media::Attachment>, _> =
            serde_json::from_str(attachments_json);
        anyhow::ensure!(parsed.is_ok(), "malformed attachments json");
        let Some(source_id) = self
            .source_id(module_id, collection_id, root_token, path_rel)
            .await?
        else {
            return Ok(false);
        };
        let n = sqlx::query(
            "UPDATE files SET streams_json=json_set(streams_json,'$.attachments',json(?))
              WHERE id=? AND size=?",
        )
        .bind(attachments_json)
        .bind(source_id)
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
    ) -> Result<Vec<SourcePath>> {
        let media_type: Option<String> = sqlx::query_scalar(
            "SELECT media_type FROM collections WHERE module_id = ? AND collection_id = ?",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_optional(&self.db)
        .await?;
        if !matches!(
            media_type.as_deref(),
            Some("movies") | Some("series") | Some("anime")
        ) {
            return Ok(Vec::new());
        }
        self.source_worklist(
            "subs_extracted = 0 AND EXISTS (
                 SELECT 1 FROM json_each(json_extract(streams_json, '$.subtitles')) je
                 WHERE json_extract(je.value, '$.format')
                       IN ('ass','ssa','srt','subrip','text','vtt','webvtt'))",
            module_id,
            collection_id,
        )
        .await
    }

    /// Mark a file's subtitles extracted. `size` Some → guarded like
    /// ED2K results (stale reports dropped); None → unconditional
    /// (extraction errors: retrying an identical file fails identically,
    /// and a content change resets the flag via upsert).
    pub async fn set_subs_extracted(
        &self,
        module_id: &str,
        collection_id: &str,
        root_token: &str,
        path_rel: &str,
        size: Option<u64>,
    ) -> Result<bool> {
        let Some(source_id) = self
            .source_id(module_id, collection_id, root_token, path_rel)
            .await?
        else {
            return Ok(false);
        };
        let n = match size {
            Some(size) => sqlx::query("UPDATE files SET subs_extracted=1 WHERE id=? AND size=?")
                .bind(source_id)
                .bind(size as i64)
                .execute(&self.db)
                .await?
                .rows_affected(),
            None => sqlx::query("UPDATE files SET subs_extracted=1 WHERE id=?")
                .bind(source_id)
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

    /// Drop a host's send side. Called from every teardown path.
    ///
    /// It does NOT touch `disabled`. That set is the admin's drain toggle,
    /// persisted in `satellites` precisely so a box stays drained across a hub
    /// restart (see `set_disabled`) — and clearing it here undid that on
    /// something far more common than a restart: any disconnect. A drained
    /// satellite that bounced came back enabled in memory while the row still
    /// said disabled, placement started sending it work again, and the admin
    /// panel reported it as enabled to match.
    pub fn unregister_link(&self, module_id: &str) {
        self.links.lock().unwrap().remove(module_id);
    }

    /// Drop a host's send side, but only if it is still the one `tx` opened.
    ///
    /// Returns whether anything was removed, so a teardown can tell "I was the
    /// live link" from "somebody reconnected while I was dying".
    ///
    /// Without this, a link that died without a FIN — power, cable, wifi — sat
    /// in its 35 s heartbeat window while the box came back, connected, and was
    /// registered afresh; the old task's timeout then cleared the NEW link's
    /// entries by module id. Nothing restores them: `seen` only writes
    /// `last_seen`, which nothing reads. A healthy, heartbeating host stayed
    /// invisible until the hub or the host restarted.
    /// Forget a host's link AND mark it absent, as one step.
    ///
    /// Two calls could not hold the invariant they were written for. Between a
    /// `remove` and a `disconnected`, the box can reconnect and register: the
    /// late `disconnected` then flips the NEW entry to absent, and nothing
    /// sets it back — `seen` only writes `last_seen`. The host goes on
    /// heartbeating into a hub that will not offer its files, will not send it
    /// another manifest, and so will never scan it again.
    ///
    /// Both locks, `connected` before `links` is released, so no observer sees
    /// "present but unreachable" — the state that reads as a 409 give-up about
    /// a healthy host.
    pub fn unregister_link_if_current(
        &self,
        module_id: &str,
        tx: &tokio::sync::mpsc::Sender<Result<kahawai_proto::v1::HubToHost, tonic::Status>>,
    ) -> bool {
        let mut links = self.links.lock().unwrap();
        match links.get(module_id) {
            Some(current) if current.same_channel(tx) => {
                links.remove(module_id);
                if let Some(s) = self.connected.lock().unwrap().get_mut(module_id) {
                    s.connected = false;
                    s.last_seen = SystemTime::now();
                }
                drop(links);
                tracing::info!(%module_id, "satellite disconnected");
                self.emit(serde_json::json!({
                    "kind": "satellite", "module_id": module_id, "connected": false,
                }));
                true
            }
            _ => false,
        }
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
        tx.send(Ok(msg))
            .await
            .map_err(|_| anyhow::anyhow!("link to {module_id} closed"))
    }

    pub fn db(&self) -> &SqlitePool {
        &self.db
    }

    // ---- runtime connection state ----

    pub fn connected(
        &self,
        module_id: &str,
        module_type: &str,
        name: &str,
        fingerprint: &str,
        build: &str,
    ) {
        self.connected.lock().unwrap().insert(
            module_id.to_string(),
            SatelliteState {
                module_type: module_type.to_string(),
                name: name.to_string(),
                cert_fingerprint: fingerprint.to_string(),
                build: build.to_string(),
                connected: true,
                last_seen: SystemTime::now(),
            },
        );
        tracing::info!(%module_id, module_type, name, build, "satellite connected");
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
        let exact_roots: Vec<(String, String)> = roots
            .iter()
            .map(|path| {
                let path = std::path::Path::new(path);
                let normalized =
                    kahawai_core::media::normalize_root_path(path, std::path::Path::new("/"))
                        .map_err(anyhow::Error::msg)?;
                anyhow::ensure!(
                    path.is_absolute() && normalized == path,
                    "mediahost announced non-normalized protocol-3 root {}",
                    path.display()
                );
                Ok((
                    kahawai_core::media::root_token(&normalized),
                    path.to_string_lossy().into_owned(),
                ))
            })
            .collect::<Result<_>>()?;
        let mut tokens = std::collections::HashMap::<&str, &str>::new();
        for (token, path) in &exact_roots {
            if let Some(old) = tokens.insert(token, path) {
                anyhow::ensure!(
                    old == path,
                    "root token {token} names both {old} and {path}"
                );
            }
        }

        let mut tx = loop {
            match self.db.begin_with("BEGIN IMMEDIATE").await {
                Ok(tx) => break tx,
                Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("5") => {
                    // Startup derivations can legitimately own the sole SQLite
                    // writer while satellites reconnect. Dropping an
                    // announcement here loses its one root-adoption chance and
                    // turns the following generation mismatch into a scan.
                    tracing::warn!(%module_id, collection = collection_id,
                        "waiting for SQLite writer before root adoption");
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                Err(error) => return Err(error.into()),
            }
        };
        sqlx::query(
            "INSERT INTO collections (module_id,collection_id,media_type,roots_json)
             VALUES (?,?,?,?)
             ON CONFLICT(module_id,collection_id) DO UPDATE SET
               media_type=excluded.media_type, roots_json=excluded.roots_json",
        )
        .bind(module_id)
        .bind(collection_id)
        .bind(media_type)
        .bind(serde_json::to_string(roots)?)
        .execute(&mut *tx)
        .await?;

        // Historical roots remain as foreign-key targets for unavailable
        // sources, but only this announcement's roots may serve new reads.
        sqlx::query(
            "UPDATE collection_roots SET configured=0
              WHERE module_id=? AND collection_id=?",
        )
        .bind(module_id)
        .bind(collection_id)
        .execute(&mut *tx)
        .await?;
        for (token, path) in &exact_roots {
            let persisted: Option<String> = sqlx::query_scalar(
                "SELECT normalized_path FROM collection_roots WHERE root_token=? LIMIT 1",
            )
            .bind(token)
            .fetch_optional(&mut *tx)
            .await?;
            anyhow::ensure!(
                persisted.as_deref().is_none_or(|old| old == path),
                "root token {token} was previously stored for {}, not {path}",
                persisted.as_deref().unwrap_or_default()
            );
            sqlx::query(
                "INSERT INTO collection_roots
                   (module_id,collection_id,root_token,normalized_path,configured)
                 VALUES (?,?,?,?,1)
                 ON CONFLICT(module_id,collection_id,root_token) DO UPDATE SET
                   normalized_path=excluded.normalized_path, configured=1",
            )
            .bind(module_id)
            .bind(collection_id)
            .bind(token)
            .bind(path)
            .execute(&mut *tx)
            .await?;
        }

        // One root proves every legacy file's root. This is one indexed update;
        // dependent source, subtitle and failure rows already reference file id.
        let adopted = if let [(root_token, _)] = exact_roots.as_slice() {
            sqlx::query(
                "UPDATE files SET root_id=(
                    SELECT id FROM collection_roots
                     WHERE module_id=? AND collection_id=? AND root_token=?)
                  WHERE module_id=? AND collection_id=? AND root_id IS NULL",
            )
            .bind(module_id)
            .bind(collection_id)
            .bind(root_token)
            .bind(module_id)
            .bind(collection_id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
        } else {
            0
        };
        if adopted > 0 {
            sqlx::query(
                "UPDATE collections SET root_adoption_pending=1
                  WHERE module_id=? AND collection_id=?",
            )
            .bind(module_id)
            .bind(collection_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        tracing::info!(%module_id, collection = collection_id, media_type, adopted,
            "collection announced");
        self.ensure_library(module_id, collection_id, media_type)
            .await?;
        Ok(())
    }

    pub async fn root_adoption_pending(
        &self,
        module_id: &str,
        collection_id: &str,
    ) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT root_adoption_pending FROM collections
             WHERE module_id = ? AND collection_id = ?",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_optional(&self.db)
        .await?
        .unwrap_or_default()
            != 0)
    }

    pub async fn acknowledge_root_adoption(
        &self,
        module_id: &str,
        collection_id: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE collections SET root_adoption_pending = 0
             WHERE module_id = ? AND collection_id = ?
               AND NOT EXISTS (
                   SELECT 1 FROM files
                    WHERE module_id = ? AND collection_id = ? AND root_id IS NULL)",
        )
        .bind(module_id)
        .bind(collection_id)
        .bind(module_id)
        .bind(collection_id)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn unresolved_legacy_sources(
        &self,
        module_id: &str,
        collection_id: &str,
    ) -> Result<Vec<kahawai_proto::v1::LegacySource>> {
        let rows = sqlx::query(
            "SELECT path_rel,size,head_xxh3,tail_xxh3,oshash FROM files
             WHERE module_id=? AND collection_id=? AND root_id IS NULL",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| kahawai_proto::v1::LegacySource {
                path_rel: r.get("path_rel"),
                size: r.get::<i64, _>("size") as u64,
                head_xxh3: r.get::<i64, _>("head_xxh3") as u64,
                tail_xxh3: r.get::<i64, _>("tail_xxh3") as u64,
                oshash: r.get::<i64, _>("oshash") as u64,
            })
            .collect())
    }

    pub async fn adopt_legacy_source(
        &self,
        module_id: &str,
        collection_id: &str,
        root_token: &str,
        legacy_path: &str,
    ) -> Result<()> {
        let root_id: i64 = sqlx::query_scalar(
            "SELECT id FROM collection_roots
              WHERE module_id=? AND collection_id=? AND root_token=? AND configured=1",
        )
        .bind(module_id)
        .bind(collection_id)
        .bind(root_token)
        .fetch_one(&self.db)
        .await
        .context("unknown exact root token")?;
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let changed = sqlx::query(
            "UPDATE files SET root_id=?
              WHERE module_id=? AND collection_id=? AND path_rel=? AND root_id IS NULL",
        )
        .bind(root_id)
        .bind(module_id)
        .bind(collection_id)
        .bind(legacy_path)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        anyhow::ensure!(
            changed == 1,
            "legacy source vanished or was already adopted"
        );
        sqlx::query(
            "UPDATE collections SET root_adoption_pending = 1
             WHERE module_id = ? AND collection_id = ?",
        )
        .bind(module_id)
        .bind(collection_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Validate a protocol-3 exact root token against the collection's
    /// persisted token/path bindings. Empty tokens are never wire-compatible.
    pub async fn resolve_root_token(
        &self,
        module_id: &str,
        collection_id: &str,
        supplied: &str,
    ) -> Result<String> {
        anyhow::ensure!(!supplied.is_empty(), "exact source has an empty root token");
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM collection_roots
              WHERE module_id=? AND collection_id=? AND root_token=? AND configured=1)",
        )
        .bind(module_id)
        .bind(collection_id)
        .bind(supplied)
        .fetch_one(&self.db)
        .await?;
        anyhow::ensure!(
            exists,
            "unknown root token {supplied} for {module_id}/{collection_id}"
        );
        Ok(supplied.to_string())
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
        let lib: Option<(String, String)> =
            sqlx::query_as("SELECT id, media_type FROM libraries WHERE name = ?")
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
        self.attach_collection(&lib_id, module_id, collection_id)
            .await?;
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
    pub async fn collection_root_count(
        &self,
        module_id: &str,
        collection_id: &str,
    ) -> Result<usize> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM collection_roots
              WHERE module_id=? AND collection_id=? AND configured=1",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_optional(&self.db)
        .await?
        .unwrap_or(0) as usize)
    }

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
                let (module_id, collection_id) = (
                    r.get::<String, _>("module_id"),
                    r.get::<String, _>("collection_id"),
                );
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
        let resolve_series = matches!(media_type.as_deref(), Some("series") | Some("anime"));
        let anime = media_type.as_deref() == Some("anime");
        let resolve_music = media_type.as_deref() == Some("music");

        let mut tx = self.db.begin().await?;
        let n = files.len();
        for f in files {
            anyhow::ensure!(!f.root_token.is_empty(), "file record has no root token");
            let root_id: i64 = sqlx::query_scalar(
                "SELECT id FROM collection_roots
                  WHERE module_id=? AND collection_id=? AND root_token=? AND configured=1",
            )
            .bind(module_id)
            .bind(collection_id)
            .bind(&f.root_token)
            .fetch_one(&mut *tx)
            .await
            .context("file record names an unknown collection root")?;
            let source_id: i64 = sqlx::query_scalar(
                "INSERT INTO files
                   (module_id,collection_id,root_id,path_rel,size,mtime_unix,
                    head_xxh3,tail_xxh3,oshash,streams_json,revision)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?)
                 ON CONFLICT (module_id,collection_id,root_id,path_rel)
                   WHERE root_id IS NOT NULL DO UPDATE SET
                   revision=excluded.revision,
                   ed2k=CASE WHEN excluded.size=files.size
                              AND excluded.mtime_unix=files.mtime_unix
                             THEN files.ed2k ELSE NULL END,
                   subs_extracted=CASE WHEN excluded.size=files.size
                                        AND excluded.mtime_unix=files.mtime_unix
                                       THEN files.subs_extracted ELSE 0 END,
                   size=excluded.size,mtime_unix=excluded.mtime_unix,
                   head_xxh3=excluded.head_xxh3,tail_xxh3=excluded.tail_xxh3,
                   oshash=excluded.oshash,streams_json=excluded.streams_json
                 RETURNING id",
            )
            .bind(module_id)
            .bind(collection_id)
            .bind(root_id)
            .bind(&f.path_rel)
            .bind(f.size as i64)
            .bind(f.mtime_unix)
            .bind(f.head_xxh3 as i64)
            .bind(f.tail_xxh3 as i64)
            .bind(f.oshash as i64)
            .bind(&f.streams_json)
            .bind(names::release_revision(&f.path_rel) as i64)
            .fetch_one(&mut *tx)
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
                    "SELECT id FROM items WHERE module_id=? AND collection_id=?
                       AND kind='movie' AND norm_title=? AND year IS ?",
                )
                .bind(module_id)
                .bind(collection_id)
                .bind(&norm)
                .bind(guess.year)
                .fetch_optional(&mut *tx)
                .await?;
                Some(match existing {
                    Some(id) => id,
                    None => {
                        let id = ulid::Ulid::generate().to_string();
                        sqlx::query(
                            "INSERT INTO items
                               (id,kind,title,norm_title,year,module_id,collection_id)
                             VALUES (?,'movie',?,?,?,?,?)",
                        )
                        .bind(&id)
                        .bind(&guess.title)
                        .bind(&norm)
                        .bind(guess.year)
                        .bind(module_id)
                        .bind(collection_id)
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
                             WHERE module_id=? AND collection_id=? AND kind='album'
                               AND norm_title=? AND LOWER(artist)=LOWER(?)",
                        )
                        .bind(module_id)
                        .bind(collection_id)
                        .bind(&album_norm)
                        .bind(&artist)
                        .fetch_optional(&mut *tx)
                        .await?;
                        let album_id = match existing {
                            Some(id) => id,
                            None => {
                                let id = ulid::Ulid::generate().to_string();
                                sqlx::query(
                                    "INSERT INTO items
                                       (id,kind,title,norm_title,year,artist,norm_artist,
                                        module_id,collection_id)
                                     VALUES (?,'album',?,?,?,?,?,?,?)",
                                )
                                .bind(&id)
                                .bind(&album)
                                .bind(&album_norm)
                                .bind(album_year)
                                .bind(&artist)
                                // Folded like the search needle is, or an
                                // accented artist can never be found.
                                .bind(crate::enrich::fold(&artist))
                                .bind(module_id)
                                .bind(collection_id)
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
                                       (id,kind,title,norm_title,year,parent_id,season,episode,
                                        artist,norm_artist,module_id,collection_id)
                                     VALUES (?,'track',?,?,NULL,?,?,?,?,?,?,?)",
                                )
                                .bind(&id)
                                .bind(&title)
                                .bind(names::normalize_title(&title))
                                .bind(&album_id)
                                .bind(disc)
                                .bind(track)
                                .bind(&artist)
                                .bind(crate::enrich::fold(&artist))
                                .bind(module_id)
                                .bind(collection_id)
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
                    None if anime && let Some(mg) = names::parse_movie_file(&f.path_rel) => {
                        // Anime movies (HUB-30): no episode shape, so
                        // the file is a film — Ghibli et al. The year
                        // used to be required, which left 23 yearless
                        // films bare ("Akira.mkv", "Robot Carnival.mkv")
                        // for no benefit: the extras it was guarding
                        // against (NCOP/NCED) parse as designations into
                        // season 0 and never reach here. parse_movie_file
                        // handles the one shape that would mint junk, a
                        // bare "partN" naming a piece of a film.
                        source_part = mg.part;
                        let norm = names::normalize_title(&mg.title);
                        let existing: Option<String> = sqlx::query_scalar(
                            "SELECT id FROM items WHERE module_id=? AND collection_id=?
                               AND kind='movie' AND norm_title=? AND year IS ?",
                        )
                        .bind(module_id)
                        .bind(collection_id)
                        .bind(&norm)
                        .bind(mg.year)
                        .fetch_optional(&mut *tx)
                        .await?;
                        Some(match existing {
                            Some(id) => id,
                            None => {
                                let id = ulid::Ulid::generate().to_string();
                                sqlx::query(
                                    "INSERT INTO items
                                       (id,kind,title,norm_title,year,module_id,collection_id)
                                     VALUES (?,'movie',?,?,?,?,?)",
                                )
                                .bind(&id)
                                .bind(&mg.title)
                                .bind(&norm)
                                .bind(mg.year)
                                .bind(module_id)
                                .bind(collection_id)
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
                            "SELECT id FROM items WHERE module_id=? AND collection_id=?
                               AND kind='show' AND norm_title=? AND year IS ?",
                        )
                        .bind(module_id)
                        .bind(collection_id)
                        .bind(&norm)
                        .bind(g.show_year)
                        .fetch_optional(&mut *tx)
                        .await?;
                        let show_id = match show {
                            Some(id) => id,
                            None => {
                                let id = ulid::Ulid::generate().to_string();
                                sqlx::query(
                                    "INSERT INTO items
                                       (id,kind,title,norm_title,year,module_id,collection_id)
                                     VALUES (?,'show',?,?,?,?,?)",
                                )
                                .bind(&id)
                                .bind(&g.show_title)
                                .bind(&norm)
                                .bind(g.show_year)
                                .bind(module_id)
                                .bind(collection_id)
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
                            Some(id) => {
                                // A batch marker learned on a later scan
                                // widens the existing slot in place — and
                                // an auto-generated "Episode N" title
                                // widens with it (a provider/human title
                                // is never touched).
                                sqlx::query(
                                    "UPDATE items SET episode_end = ?1,
                                            title = CASE
                                              WHEN ?1 IS NOT NULL
                                               AND title = 'Episode ' || episode
                                              THEN 'Episodes ' || episode || '-' || ?1
                                              ELSE title END,
                                            norm_title = CASE
                                              WHEN ?1 IS NOT NULL
                                               AND norm_title = 'episode ' || episode
                                              THEN 'episodes ' || episode || '-' || ?1
                                              ELSE norm_title END
                                      WHERE id = ?2 AND episode_end IS NOT ?1",
                                )
                                .bind(g.episode_end)
                                .bind(&id)
                                .execute(&mut *tx)
                                .await?;
                                id
                            }
                            None => {
                                let id = ulid::Ulid::generate().to_string();
                                let title = g.episode_title.clone().unwrap_or_else(|| {
                                    match g.episode_end {
                                        Some(end) => {
                                            format!("Episodes {}-{}", g.episode, end)
                                        }
                                        None => format!("Episode {}", g.episode),
                                    }
                                });
                                sqlx::query(
                                    "INSERT INTO items
                                       (id,kind,title,norm_title,year,parent_id,season,episode,
                                        episode_end,module_id,collection_id)
                                     VALUES (?,'episode',?,?,NULL,?,?,?,?,?,?)",
                                )
                                .bind(&id)
                                .bind(&title)
                                .bind(names::normalize_title(&title))
                                .bind(&show_id)
                                .bind(g.season)
                                .bind(g.episode)
                                .bind(g.episode_end)
                                .bind(module_id)
                                .bind(collection_id)
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
                sqlx::query("UPDATE files SET item_id=?,part=? WHERE id=?")
                    .bind(&item_id)
                    .bind(source_part)
                    .bind(source_id)
                    .execute(&mut *tx)
                    .await?;
                Self::bind_playable_source(
                    &mut tx,
                    module_id,
                    collection_id,
                    root_id,
                    source_id,
                    &item_id,
                    &f.path_rel,
                    source_part,
                )
                .await?;

                // Subtitle tracks are first-class rows synced from the
                // probe (unification, 2026-07-31): only changed files
                // reach this loop, so syncing here IS scan-sync.
                if let Ok(info) =
                    serde_json::from_str::<kahawai_core::media::MediaInfo>(&f.streams_json)
                {
                    crate::tracks::sync_source_tracks(&mut tx, source_id, &info).await?;
                }

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
            "DELETE FROM items WHERE kind NOT IN ('show','album')
               AND NOT EXISTS (SELECT 1 FROM files f WHERE f.item_id=items.id)",
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

    /// Bind one physical file to one explicit playable rendition. The update
    /// touches only this file and, for multipart input, its one release family;
    /// no catalogue-wide regrouping occurs during ingestion.
    #[allow(clippy::too_many_arguments)]
    async fn bind_playable_source(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        module_id: &str,
        collection_id: &str,
        root_id: i64,
        file_id: i64,
        item_id: &str,
        path_rel: &str,
        part: Option<u32>,
    ) -> Result<()> {
        sqlx::query("DELETE FROM playable_source_parts WHERE file_id=?")
            .bind(file_id)
            .execute(&mut **tx)
            .await?;
        let family = part
            .map(|_| kahawai_core::names::rendition_family_key(path_rel))
            .unwrap_or_else(|| format!("file:{file_id}"));
        let expected = part.unwrap_or(1) as i64;
        let source_id: i64 = sqlx::query_scalar(
            "INSERT INTO playable_sources
               (module_id,collection_id,item_id,root_id,family_key,expected_parts)
             VALUES(?,?,?,?,?,?)
             ON CONFLICT(module_id,collection_id,root_id,family_key) DO UPDATE SET
               item_id=excluded.item_id,
               expected_parts=MAX(playable_sources.expected_parts,excluded.expected_parts)
             RETURNING id",
        )
        .bind(module_id)
        .bind(collection_id)
        .bind(item_id)
        .bind(root_id)
        .bind(&family)
        .bind(expected)
        .fetch_one(&mut **tx)
        .await?;
        sqlx::query(
            "INSERT INTO playable_source_parts
               (playable_source_id,module_id,collection_id,ordinal,file_id)
             VALUES(?,?,?,?,?)",
        )
        .bind(source_id)
        .bind(module_id)
        .bind(collection_id)
        .bind(expected)
        .bind(file_id)
        .execute(&mut **tx)
        .await?;
        // Rebinding may leave an empty ordinary source or obsolete family.
        sqlx::query(
            "DELETE FROM playable_sources
              WHERE NOT EXISTS(SELECT 1 FROM playable_source_parts p
                                WHERE p.playable_source_id=playable_sources.id)",
        )
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// After a completed scan, drop files the scan no longer reported and
    /// items left without any source. Watch state is archived keyed to
    /// content identity first (HUB-20), so moves/renames and returning
    /// media keep their history.
    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
                .bind(key)
                .fetch_optional(&self.db)
                .await?,
        )
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
            "SELECT r.root_token,f.path_rel,size,mtime_unix,
                    COALESCE(json_extract(streams_json,'$.nfo'),'') AS nfo,
                    COALESCE(json_extract(streams_json,'$.artwork'),'') AS art,
                    COALESCE(json_extract(streams_json,'$.external_subtitles'),'[]') AS subs
             FROM files f JOIN collection_roots r ON r.id=f.root_id
             WHERE f.module_id=? AND f.collection_id=?",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                // Distinct paths in sorted order — one .idx carries many
                // tracks but is one file on disk.
                let mut subs: Vec<String> =
                    serde_json::from_str::<Vec<serde_json::Value>>(&r.get::<String, _>("subs"))
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|v| v.get("path_rel")?.as_str().map(str::to_string))
                        .collect();
                subs.sort();
                subs.dedup();
                kahawai_proto::v1::FileStat {
                    source: Some(kahawai_proto::v1::SourcePath {
                        path_rel: r.get("path_rel"),
                        root_token: r.get("root_token"),
                    }),
                    size: r.get::<i64, _>("size") as u64,
                    mtime_unix: r.get("mtime_unix"),
                    sidecars: Self::sidecar_sig(
                        &r.get::<String, _>("nfo"),
                        &r.get::<String, _>("art"),
                        &subs,
                    ),
                }
            })
            .collect())
    }

    /// One line describing a file's sidecars, compared verbatim on both
    /// sides. Order is fixed so the same pair always spells the same way.
    /// Compared VERBATIM against the mediahost's own spelling
    /// (`kahawai_mediahost::scan::sidecar_sig`) — the two must build the
    /// same string from their own views (DB here, disk there). `subs`
    /// covers subtitle sidecars (.srt/.ass/.vtt and .idx pairs): without
    /// them in the signature, a pair dropped next to an unchanged movie
    /// stayed invisible until the movie itself changed (measured: all 42
    /// real .idx pairs in this library).
    pub fn sidecar_sig(nfo: &str, artwork: &str, subs: &[String]) -> String {
        if nfo.is_empty() && artwork.is_empty() && subs.is_empty() {
            return String::new();
        }
        format!("n:{nfo}|a:{artwork}|s:{}", subs.join(","))
    }

    pub async fn reconcile_files(
        &self,
        module_id: &str,
        collection_id: &str,
        seen: &std::collections::HashSet<SourcePath>,
    ) -> Result<usize> {
        let known = sqlx::query(
            "SELECT r.root_token,f.path_rel FROM files f
             JOIN collection_roots r ON r.id=f.root_id
             WHERE f.module_id=? AND f.collection_id=?",
        )
        .bind(module_id)
        .bind(collection_id)
        .fetch_all(&self.db)
        .await?
        .into_iter()
        .map(|r| SourcePath {
            root_token: r.get("root_token"),
            path_rel: r.get("path_rel"),
        });
        let stale: Vec<SourcePath> = known.filter(|p| !seen.contains(p)).collect();
        if stale.is_empty() {
            return Ok(0);
        }
        let mut tx = self.db.begin().await?;
        for source in &stale {
            sqlx::query(ARCHIVE_WATCH_FOR_FILE_SQL)
                .bind(module_id)
                .bind(collection_id)
                .bind(&source.path_rel)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "DELETE FROM files WHERE module_id=? AND collection_id=?
                  AND root_id=(SELECT id FROM collection_roots
                    WHERE module_id=? AND collection_id=? AND root_token=?)
                  AND path_rel=?",
            )
            .bind(module_id)
            .bind(collection_id)
            .bind(module_id)
            .bind(collection_id)
            .bind(&source.root_token)
            .bind(&source.path_rel)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "DELETE FROM items WHERE kind NOT IN ('show','album')
               AND NOT EXISTS (SELECT 1 FROM files f WHERE f.item_id=items.id)",
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
                    // HUB-36: 0 = unmeasured (legacy box or pre-benchmark).
                    "speed_1080": e.speed_1080, "speed_2160": e.speed_2160,
                }))
                .collect::<Vec<_>>(),
            "max_sessions": caps.max_sessions,
            "decode_caps": caps.decode_caps,
            "tonemap": caps.tonemap,
            "ass_burn": caps.ass_burn,
            "tonemap_speed_1080": caps.tonemap_speed_1080,
            "tonemap_speed_2160": caps.tonemap_speed_2160,
        });
        self.transcoder_caps
            .lock()
            .unwrap()
            .insert(module_id.to_string(), json);
    }

    pub fn mark_deep_rescan(&self, module_id: &str, collection_id: &str) {
        self.deep_rescan
            .lock()
            .unwrap()
            .insert((module_id.to_string(), collection_id.to_string()));
    }

    /// One-shot: consumed by the manifest answer, so a later ordinary
    /// refresh is incremental again.
    pub fn take_deep_rescan(&self, module_id: &str, collection_id: &str) -> bool {
        self.deep_rescan
            .lock()
            .unwrap()
            .remove(&(module_id.to_string(), collection_id.to_string()))
    }

    /// HUB-15b: the verified encoder codec names a transcoder reported
    /// ("h264", "hevc", "aac", …) — what negotiation may pick as an
    /// encode target when this box would run the session.
    pub fn transcoder_encoders(&self, module_id: &str) -> Vec<String> {
        self.transcoder_caps
            .lock()
            .unwrap()
            .get(module_id)
            .and_then(|c| c.get("encoders")?.as_array().cloned())
            .map(|a| {
                a.iter()
                    .filter_map(|e| e["codec"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// HUB-36: publish what AIO's full local transcoder measured.
    pub fn set_local_bench(&self, b: kahawai_media::bench::BenchResults) {
        *self.local_bench.lock().unwrap() = Some(b);
    }

    /// AIO's local video speeds, if its benchmark has landed.
    pub fn local_bench(&self) -> Option<kahawai_media::bench::BenchResults> {
        self.local_bench.lock().unwrap().clone()
    }

    /// HUB-36: a transcoder's measured speed for one codec at a source
    /// height, as a realtime multiple. None = unmeasured, which callers
    /// read as "no data", never as slow.
    pub fn transcoder_speed(&self, module_id: &str, codec: &str, height: u32) -> Option<f32> {
        let caps = self.transcoder_caps.lock().unwrap();
        let e = caps
            .get(module_id)?
            .get("encoders")?
            .as_array()?
            .iter()
            .find(|e| e["codec"] == codec)?;
        let key = if height > 1080 {
            "speed_2160"
        } else {
            "speed_1080"
        };
        // as_f64() on a JSON null returns None — absence stays absence,
        // and a tiny measured value survives as the measurement it is.
        Some(e.get(key)?.as_f64()? as f32)
    }

    /// Same for the GL tone-map segment (HUB-15a's boolean, measured).
    pub fn transcoder_tonemap_speed(&self, module_id: &str, height: u32) -> Option<f32> {
        let caps = self.transcoder_caps.lock().unwrap();
        let c = caps.get(module_id)?;
        let key = if height > 1080 {
            "tonemap_speed_2160"
        } else {
            "tonemap_speed_1080"
        };
        Some(c.get(key)?.as_f64()? as f32)
    }

    /// HUB-15a: does this transcoder report the GL tone-map segment?
    pub fn transcoder_reports_tonemap(&self, module_id: &str) -> bool {
        self.transcoder_caps
            .lock()
            .unwrap()
            .get(module_id)
            .and_then(|c| c.get("tonemap"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    pub fn transcoder_reports_ass_burn(&self, module_id: &str) -> bool {
        self.transcoder_caps
            .lock()
            .unwrap()
            .get(module_id)
            .and_then(|c| c.get("ass_burn"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// HUB-32a: can ANY connected transcoder burn ASS? A fleet-wide
    /// question, unlike `transcoder_reports_tonemap`, because the tier
    /// is decided before placement and a burn is a HARD filter: knowing
    /// some box can do it is what makes offering the tier honest, and
    /// `place()` then has to land on one of those boxes or the session
    /// refuses (there is no silent degradation for a burn).
    pub fn any_transcoder_ass_burn(&self) -> bool {
        self.transcoder_caps
            .lock()
            .unwrap()
            .values()
            .any(|c| c.get("ass_burn").and_then(|v| v.as_bool()).unwrap_or(false))
    }

    pub fn clear_transcoder_caps(&self, module_id: &str) {
        self.transcoder_caps.lock().unwrap().remove(module_id);
    }

    pub fn register_tc_link(
        &self,
        module_id: &str,
        tx: tokio::sync::mpsc::Sender<Result<kahawai_proto::v1::HubToTc, tonic::Status>>,
    ) {
        self.tc_links
            .lock()
            .unwrap()
            .insert(module_id.to_string(), tx);
    }

    /// Drop a transcoder link only if it is still the one the caller owns.
    ///
    /// The twin of `unregister_link_if_current`, and it was missed when that
    /// one was written. A transcoder that dies without a FIN sits in its
    /// 35-second heartbeat window; if the box comes back inside it, the old
    /// task's teardown deleted the LIVE connection's sender, its capabilities
    /// and its load accounting. Capabilities are sent once per connection, so
    /// `choose` — which requires both a link and caps — never saw that box
    /// again until the transcoder process itself restarted.
    pub fn unregister_tc_link_if_current(
        &self,
        module_id: &str,
        tx: &tokio::sync::mpsc::Sender<Result<kahawai_proto::v1::HubToTc, tonic::Status>>,
    ) -> bool {
        let mut links = self.tc_links.lock().unwrap();
        match links.get(module_id) {
            Some(current) if current.same_channel(tx) => {
                links.remove(module_id);
                drop(links);
                self.tc_load.lock().unwrap().remove(module_id);
                self.tc_link_rate.lock().unwrap().remove(module_id);
                true
            }
            _ => false,
        }
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
        tx.send(Ok(msg))
            .await
            .map_err(|_| anyhow::anyhow!("transcoder link closed"))
    }

    /// HUB-36: seed the in-memory pace map from what previous runs
    /// learned. Called once at startup — placement is synchronous and
    /// cannot await a query per candidate.
    pub async fn load_pace(&self) -> Result<usize> {
        let rows = crate::pace::load_all(&self.db).await?;
        let n = rows.len();
        let mut map = self.tc_pace.lock().unwrap();
        for (module_id, class, multiple) in rows {
            map.insert((module_id, class), multiple);
        }
        Ok(n)
    }

    /// Write through after a fold, so placement sees the new estimate
    /// without re-reading the table.
    pub fn set_pace(&self, module_id: &str, class: &str, multiple: f64) {
        self.tc_pace
            .lock()
            .unwrap()
            .insert((module_id.to_string(), class.to_string()), multiple);
    }

    /// What this box has been measured to achieve on this kind of work,
    /// or None if it has never done any.
    pub fn pace_of(&self, module_id: &str, class: &str) -> Option<f64> {
        self.tc_pace
            .lock()
            .unwrap()
            .get(&(module_id.to_string(), class.to_string()))
            .copied()
    }

    /// 0 from the wire means "not measured", never "no bandwidth".
    pub fn set_link_rate(&self, module_id: &str, bytes_per_sec: u64) {
        if bytes_per_sec == 0 {
            return;
        }
        self.tc_link_rate
            .lock()
            .unwrap()
            .insert(module_id.to_string(), bytes_per_sec);
    }

    pub fn link_rate_of(&self, module_id: &str) -> Option<u64> {
        self.tc_link_rate.lock().unwrap().get(module_id).copied()
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
    ///
    /// A QUERY: who would take this work, without claiming them. Used
    /// while planning, where the answer decides which encoders to offer
    /// and nothing is dispatched — including for sessions that turn out
    /// to be direct play. See [`Self::reserve_transcoder`] for the one
    /// that takes the slot.
    pub fn pick_transcoder(&self, need: &PlacementNeed) -> Option<String> {
        self.choose(need, false)
    }

    /// Pick AND take the slot, in the same critical section that read
    /// the load, so a concurrent placement cannot see the box as free.
    /// The caller owns the reservation and must return it with
    /// [`Self::tc_session_ended`] on every path that does not end in a
    /// running session.
    ///
    /// Counting only at dispatch-ready left a window up to 40 s wide in
    /// which every concurrent placement read the same load. Measured on
    /// a five-transcoder fleet: ten concurrent starts all chose one box
    /// and its `max_sessions = 2` stopped none of them, because the
    /// capacity filter and the least-loaded tie-break were both reading
    /// a number nothing had incremented yet.
    pub fn reserve_transcoder(&self, need: &PlacementNeed) -> Option<String> {
        self.choose(need, true)
    }

    fn choose(&self, need: &PlacementNeed, reserve: bool) -> Option<String> {
        let caps = self.transcoder_caps.lock().unwrap().clone();
        let links = self.tc_links.lock().unwrap();
        let mut load = self.tc_load.lock().unwrap();
        let disabled = self.disabled.lock().unwrap();
        let mut candidates: Vec<(bool, bool, bool, Option<f32>, usize, String)> = caps
            .iter()
            .filter(|(id, _)| links.contains_key(*id) && !disabled.contains(*id))
            .filter_map(|(id, c)| {
                let encoders = c.get("encoders")?.as_array()?;
                let has = |codec: &str| encoders.iter().any(|e| e["codec"] == codec);
                // HUB-15b: match the TARGET the plan asks for; an empty
                // need means "any encoder of that kind".
                let video_ok = || match need.video_codec.as_str() {
                    "" => ["h264", "hevc", "av1"].iter().any(|c| has(c)),
                    c => has(c),
                };
                let audio_ok = || match need.audio_codec.as_str() {
                    "" => ["aac", "opus"].iter().any(|c| has(c)),
                    c => has(c),
                };
                if (need.encode_video && !video_ok()) || (need.encode_audio && !audio_ok()) {
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
                // Rank hardware on the codec the session will actually
                // run (empty need: any hw video encoder counts).
                if need.needs_ass_burn
                    && !c.get("ass_burn").and_then(|v| v.as_bool()).unwrap_or(false)
                {
                    return None; // cannot burn ASS; not a candidate at all
                }
                let hw = encoders.iter().any(|e| {
                    e["hardware"] == true
                        && match need.video_codec.as_str() {
                            "" => true,
                            c => e["codec"] == c,
                        }
                });
                // HUB-15a: an HDR encode prefers a box that can tone-map
                // — a preference, not a filter: with no capable box the
                // job still runs (worker encodes as-is, verdict said so).
                let tm = !need.needs_tonemap
                    || c.get("tonemap").and_then(|v| v.as_bool()).unwrap_or(false);
                // HUB-36: what this box is expected to sustain on
                // exactly this work. None = never measured, which ranks
                // as neutral rather than last: a fresh box has to run
                // something before it can be known, and refusing it for
                // want of evidence is how a fleet stays unused.
                let predicted = self.predict_fleet(id, need);
                Some((sustains(predicted), tm, hw, predicted, current, id.clone()))
            })
            .collect();
        // Sustaining first — a box that keeps ahead of the viewer beats
        // a faster-on-paper one that does not — then tone-map fit, then
        // hardware, then the prediction itself, then least loaded.
        candidates.sort_by(|a, b| {
            let rank = |p: Option<f32>| p.unwrap_or(SUSTAINS);
            b.0.cmp(&a.0)
                .then(b.1.cmp(&a.1))
                .then(b.2.cmp(&a.2))
                .then(
                    rank(b.3)
                        .partial_cmp(&rank(a.3))
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
                .then(a.4.cmp(&b.4))
        });
        let winner = candidates.first().map(|c| c.5.clone())?;
        if reserve {
            // Still holding `load`: the slot is taken before any other
            // placement can read the count.
            *load.entry(winner.clone()).or_insert(0) += 1;
        }
        Some(winner)
    }

    /// HUB-36 phase 5: where this session should run, and how fast that
    /// is expected to go.
    ///
    /// Audio-only encode is lightweight hub work (AR-10/HUB-16), so it
    /// never consumes a fleet slot. Video encode is full-transcoder work:
    /// external fleet first, with AIO's enabled local video executor as
    /// the measured fallback/repatriation candidate.
    pub fn place(&self, need: &PlacementNeed) -> Placement {
        if !need.encode_video {
            return Placement {
                target: None,
                available: true,
                predicted: None,
            };
        }
        let fleet = self.reserve_transcoder(need);
        let local = self
            .local_video_executor_enabled
            .then(|| self.predict_local(need))
            .flatten();
        match fleet {
            None => Placement {
                target: None,
                available: self.local_video_executor_enabled,
                predicted: local,
            },
            Some(id) => {
                let fleet_pred = self.predict_fleet(&id, need);
                if self.local_video_executor_enabled
                    && !sustains(fleet_pred)
                    && sustains(local)
                    && local.is_some()
                {
                    // Reserved above and not used: hand it straight back
                    // or the box stays counted busy for nothing.
                    self.tc_session_ended(&id);
                    tracing::info!(
                        box_id = %id,
                        class = need.work_class.as_deref().unwrap_or("-"),
                        fleet = fleet_pred.unwrap_or(0.0),
                        local = local.unwrap_or(0.0),
                        "no fleet box sustains this work; keeping it local"
                    );
                    return Placement {
                        target: None,
                        available: true,
                        predicted: local,
                    };
                }
                Placement {
                    target: Some(id),
                    available: true,
                    predicted: fleet_pred,
                }
            }
        }
    }

    /// 2160-class work? Read off the class key rather than passed
    /// separately, so the prediction and the thing being learned can
    /// never disagree about which bucket they are in.
    fn is_2160(need: &PlacementNeed) -> bool {
        need.work_class
            .as_deref()
            .is_some_and(|c| c.starts_with("2160|"))
    }

    /// What a satellite is expected to sustain on this work.
    ///
    /// OBSERVED wins outright when present: a measured run already
    /// contains the decode, the tone-map, the encode AND that box's link
    /// stalls, so folding the component terms in on top would count the
    /// same cost twice. Only when nothing has been observed do the parts
    /// stand in, and then the SLOWEST of them governs — a chain is its
    /// narrowest link.
    fn predict_fleet(&self, id: &str, need: &PlacementNeed) -> Option<f32> {
        if let Some(class) = need.work_class.as_deref()
            && let Some(observed) = self.pace_of(id, class)
        {
            return Some(observed as f32);
        }
        let caps = self.transcoder_caps.lock().unwrap().get(id).cloned()?;
        let big = Self::is_2160(need);
        let key = if big { "speed_2160" } else { "speed_1080" };
        let pos = |v: f32| (v > 0.0).then_some(v); // 0 on the wire = unmeasured

        let mut terms: Vec<f32> = Vec::new();
        if let Some(encoders) = caps.get("encoders").and_then(|e| e.as_array()) {
            let best = encoders
                .iter()
                .filter(|e| match need.video_codec.as_str() {
                    "" => true,
                    c => e["codec"] == c,
                })
                .filter_map(|e| e.get(key).and_then(|v| v.as_f64()).map(|v| v as f32))
                .filter_map(pos)
                .fold(None::<f32>, |acc, v| Some(acc.map_or(v, |a| a.max(v))));
            terms.extend(best);
        }
        if need.needs_tonemap {
            let tm = caps
                .get(if big {
                    "tonemap_speed_2160"
                } else {
                    "tonemap_speed_1080"
                })
                .and_then(|v| v.as_f64())
                .map(|v| v as f32)
                .and_then(pos);
            terms.extend(tm);
        }
        // The link term applies to DISPATCHED work only: the bytes have
        // to cross the wire before they can be encoded.
        if let (Some(kbps), Some(bps)) = (need.source_kbps, self.link_rate_of(id))
            && kbps > 0
        {
            terms.push((bps as f32 * 8.0 / 1000.0) / kbps as f32);
        }
        terms
            .into_iter()
            .fold(None::<f32>, |acc, v| Some(acc.map_or(v, |a: f32| a.min(v))))
    }

    /// The same question for AIO's full local transcoder. No link term:
    /// the bytes are already here, which is precisely why repatriating can
    /// beat a faster satellite on a thin wire.
    fn predict_local(&self, need: &PlacementNeed) -> Option<f32> {
        if let Some(class) = need.work_class.as_deref()
            && let Some(observed) = self.pace_of(crate::pace::LOCAL, class)
        {
            return Some(observed as f32);
        }
        let bench = self.local_bench.lock().unwrap().clone()?;
        let big = Self::is_2160(need);
        let pick = |s: &kahawai_media::bench::Speeds| if big { s.s2160 } else { s.s1080 };
        let mut terms: Vec<f32> = Vec::new();
        let best = bench
            .encoders
            .iter()
            .filter(|(element, _)| match need.video_codec.as_str() {
                "" => true,
                c => element_encodes(element, c),
            })
            .filter_map(|(_, s)| pick(s))
            .fold(None::<f32>, |acc, v| Some(acc.map_or(v, |a| a.max(v))));
        terms.extend(best);
        if need.needs_tonemap {
            terms.extend(bench.tonemap.as_ref().and_then(pick));
        }
        terms
            .into_iter()
            .fold(None::<f32>, |acc, v| Some(acc.map_or(v, |a: f32| a.min(v))))
    }

    /// Enrolled satellites (DB) merged with live connection state.
    /// Is this the hub's own in-process mediahost (AR-5)? Callers that
    /// mean "an enrolled satellite" ask this before acting.
    pub async fn is_in_process(&self, module_id: &str) -> Result<bool> {
        let fp: Option<String> =
            sqlx::query_scalar("SELECT cert_fingerprint FROM satellites WHERE module_id = ?")
                .bind(module_id)
                .fetch_optional(&self.db)
                .await?;
        Ok(fp.as_deref() == Some(Self::IN_PROCESS))
    }

    pub async fn satellites_overview(&self) -> Result<Vec<serde_json::Value>> {
        let rows = sqlx::query(
            "SELECT module_id, module_type, name, cert_fingerprint, enrolled_at
             FROM satellites ORDER BY enrolled_at",
        )
        .fetch_all(&self.db)
        .await?;
        let connected = self.connected.lock().unwrap().clone();
        let caps = self.transcoder_caps.lock().unwrap().clone();
        // HUB-36: what each box has been MEASURED doing, beside what it
        // claims it can do. Sorted so the admin page renders stably
        // rather than in hash order.
        let pace = self.tc_pace.lock().unwrap().clone();
        let link_rates = self.tc_link_rate.lock().unwrap().clone();
        Ok(rows
            .iter()
            .map(|r| {
                let id: String = r.get("module_id");
                let state = connected.get(&id);
                let mut observed: Vec<(&str, f64)> = pace
                    .iter()
                    .filter(|((m, _), _)| *m == id)
                    .map(|((_, class), v)| (class.as_str(), *v))
                    .collect();
                observed.sort_by(|a, b| a.0.cmp(b.0));
                serde_json::json!({
                    "module_id": id,
                    "module_type": r.get::<String, _>("module_type"),
                    "name": r.get::<String, _>("name"),
                    "cert_fingerprint": r.get::<String, _>("cert_fingerprint"),
                    "enrolled_at": r.get::<i64, _>("enrolled_at"),
                    "connected": state.is_some_and(|s| s.connected),
                    "build": state.map(|s| s.build.as_str()),
                    "capabilities": caps.get(&id),
                    "disabled": self.disabled.lock().unwrap().contains(&id),
                    // Measured, not claimed: per work class, and the
                    // source-plane rate this box actually sustained.
                    "pace": observed.iter()
                        .map(|(c, v)| serde_json::json!({"class": c, "multiple": v}))
                        .collect::<Vec<_>>(),
                    "link_bytes_per_sec": link_rates.get(&id),
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
        let fingerprint: String =
            sqlx::query_scalar("SELECT cert_fingerprint FROM satellites WHERE module_id = ?")
                .bind(module_id)
                .fetch_optional(&self.db)
                .await?
                .with_context(|| format!("no such satellite: {module_id}"))?;
        // The hub's own mediahost is not a satellite in any sense this
        // operation means. It cannot be enrolled, so there is no
        // certificate to revoke and no reconnection to refuse — deleting
        // it only wipes the index of everything it serves, which for an
        // all-in-one deployment is the entire library. It would come back
        // on the next hub start (ensure_local_satellite) and re-probe from
        // nothing, so the button was pure cost.
        anyhow::ensure!(
            fingerprint != Self::IN_PROCESS,
            "the in-process mediahost cannot be deleted: it is the hub itself"
        );

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
             FROM files f JOIN watch_state w ON w.item_id=f.item_id
             WHERE f.module_id=?",
        )
        .bind(module_id)
        .execute(&mut *tx)
        .await?;
        for sql in [
            "DELETE FROM files WHERE module_id=?",
            "DELETE FROM collections WHERE module_id = ?",
            // HUB-36: what it achieved described hardware the fleet no
            // longer has. A re-enrolment mints a new id and learns again.
            "DELETE FROM transcoder_pace WHERE module_id = ?",
            "DELETE FROM satellites WHERE module_id = ?",
        ] {
            sqlx::query(sql).bind(module_id).execute(&mut *tx).await?;
        }
        sqlx::query(
            "DELETE FROM items WHERE kind NOT IN ('show','album')
               AND NOT EXISTS (SELECT 1 FROM files f WHERE f.item_id=items.id)",
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
        // Deleting the satellite forgets the drain with it. This used to happen
        // by accident, because `unregister_link` cleared the set as a side
        // effect; with that gone it has to be said where it is actually true.
        // Otherwise a re-enrolled module id came back drained in memory against
        // a fresh row saying enabled.
        self.disabled.lock().unwrap().remove(module_id);
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
