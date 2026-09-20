//! Hub-owned subtitle prewarm work, one row per probed file and kind.
//!
//! `subtitle_jobs` is queue state, not artifact state. The hub's subtitle
//! cache on disk stays the truth of what has been extracted; a row only
//! says whether the hub still has to offer the file to its mediahost. Rows
//! exist for every file with a probe, whether or not it carries subtitle
//! tracks: deciding that needs the probe decoded, which is the hub's job,
//! and a row it finishes on first claim costs less than a catalogue walk
//! to avoid creating it.
//!
//! Two kinds, one mechanism. `text` is the embedded text tracks, walked
//! out of the container by the mediahost in one pass and settled by its
//! `FileSubtitles`. `sets` is the image tracks' display sets, walked one
//! track at a time and settled once every image track's sets have
//! landed (`ImageSubtitles`). Both are offered as ranked worklists to the
//! host that holds the bytes, leased the same way, released together on
//! that host's reconnect, and reset together by a byte change.
//!
//! Lifecycle: `pending`/`retry` are claimable once `due_at` passes,
//! `running` is leased to the mediahost named in `host` and reclaimable
//! after `lease_until`, `blocked` waits for an administrator, `done` is
//! settled — by the host's `FileSubtitles` landing, or by the hub finding
//! nothing to ask for. A byte change on the file (size, mtime or hashes)
//! resets the row, because the cache key carries the content revision and
//! the old artifacts no longer answer for the new bytes; a metadata-only
//! version bump does not. A host reconnect releases what was offered to it:
//! its queue died with its process.
//!
//! OCR itself is not a kind: it is a function of display sets on disk, and
//! the hub runs it when they land (migration 0007 dropped the rows that
//! once tried to queue it).
//!
//! Ranking happens at claim time, from a caller-supplied set of in-flight
//! library items (watch state lives in the hub database), then movies
//! before episodes, newest mtime first, then path — the order the hub's
//! `workorder` module documents.
use crate::*;
use serde::{Deserialize, Serialize};
use sqlx::Row;

/// The work kinds, in the order the driver offers them.
pub const SUBTITLE_KINDS: [&str; 2] = ["text", "sets"];

fn kind_ok(kind: &str) -> Result<()> {
    ensure!(SUBTITLE_KINDS.contains(&kind), "unknown subtitle work kind");
    Ok(())
}

/// One probed file as the subtitle machinery addresses it: where the
/// bytes are, which host serves them, and the probe.
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub file_id: String,
    pub host: String,
    pub collection_id: String,
    pub remote_id: String,
    pub media_type: MediaType,
    pub root_token: String,
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub head_hash: u64,
    pub tail_hash: u64,
    pub media: kahawai_core::media::MediaInfo,
    pub item_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SubtitleJob {
    pub kind: String,
    pub token: String,
    pub attempts: i64,
    pub file: SourceFile,
}

/// Counts per kind and state, shaped like [ProviderWorkStatus].
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SubtitleWorkStatus {
    pub kind: String,
    pub state: String,
    pub count: i64,
    pub due_at: i64,
    pub error: Option<String>,
}

/// Failures the mediahost reports beyond this park the row for an
/// administrator instead of spending another attempt on the clock.
pub const BLOCK_AFTER_ATTEMPTS: i64 = 6;

const SOURCE_COLUMNS: &str = "f.id AS file_id,f.path,f.size,f.mtime,f.head_hash,f.tail_hash,f.media_json,
    r.token,c.id AS collection_id,c.remote_id,c.mediahost_id,c.media_type,
    (SELECT e.item_id FROM media_parts p JOIN media_entries e ON e.id=p.entry_id WHERE p.file_id=f.id) AS item_id";

fn source_file(row: &sqlx::sqlite::SqliteRow) -> Result<SourceFile> {
    Ok(SourceFile {
        file_id: row.get("file_id"),
        host: row.get("mediahost_id"),
        collection_id: row.get("collection_id"),
        remote_id: row.get("remote_id"),
        media_type: MediaType::parse(row.get("media_type"))?,
        root_token: row.get("token"),
        path: row.get("path"),
        size: row.get::<i64, _>("size") as u64,
        mtime: row.get::<Option<i64>, _>("mtime").unwrap_or(0),
        head_hash: crate::catalogue::hash(row.get("head_hash"))?.unwrap_or(0),
        tail_hash: crate::catalogue::hash(row.get("tail_hash"))?.unwrap_or(0),
        media: serde_json::from_str(row.get("media_json"))?,
        item_id: row.get("item_id"),
    })
}

impl Store {
    /// Claim up to `limit` rows of one kind for one mediahost, most wanted
    /// first. `in_flight` are library item ids somebody is part-way through.
    pub async fn claim_subtitle_jobs(
        &self,
        kind: &str,
        host: &str,
        in_flight: &[String],
        now: i64,
        lease_seconds: i64,
        limit: usize,
    ) -> Result<Vec<SubtitleJob>> {
        kind_ok(kind)?;
        ensure!(lease_seconds > 0, "lease must be positive");
        let in_flight = serde_json::to_string(in_flight)?;
        let mut tx = self.db.begin().await?;
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT j.attempts,{SOURCE_COLUMNS}
             FROM subtitle_jobs j
             JOIN files f ON f.id=j.file_id
             JOIN collection_roots r ON r.id=f.root_id
             JOIN collections c ON c.id=f.collection_id
             WHERE j.kind=?5 AND c.mediahost_id=?1
               AND c.snapshot_active=0 AND f.media_json IS NOT NULL AND f.size IS NOT NULL
               AND ((j.state IN ('pending','retry') AND j.due_at<=?2)
                    OR (j.state='running' AND j.lease_until<=?2))
             ORDER BY (item_id IN (SELECT value FROM json_each(?3))) DESC,
                      (c.media_type='movies') DESC, f.mtime DESC, r.token, f.path
             LIMIT ?4"
        )))
        .bind(host)
        .bind(now)
        .bind(&in_flight)
        .bind(limit as i64)
        .bind(kind)
        .fetch_all(&mut *tx)
        .await?;
        let mut jobs = Vec::with_capacity(rows.len());
        for row in rows {
            let job = SubtitleJob {
                kind: kind.into(),
                token: id(),
                attempts: row.get::<i64, _>("attempts") + 1,
                file: source_file(&row)?,
            };
            sqlx::query(
                "UPDATE subtitle_jobs SET state='running',token=?,lease_until=?,attempts=?,host=?
                 WHERE file_id=? AND kind=?",
            )
            .bind(&job.token)
            .bind(now + lease_seconds)
            .bind(job.attempts)
            .bind(&job.file.host)
            .bind(&job.file.file_id)
            .bind(kind)
            .execute(&mut *tx)
            .await?;
            jobs.push(job);
        }
        tx.commit().await?;
        Ok(jobs)
    }

    /// The probed file a mediahost message names. A sidecar's path (a
    /// VobSub `.idx`) resolves to the media file that lists it, because the
    /// host reports what it walked and the catalogue knows only the media.
    pub async fn source_file(
        &self,
        host: &str,
        collection: &str,
        source: &kahawai_proto::v1::SourcePath,
    ) -> Result<Option<SourceFile>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {SOURCE_COLUMNS}
             FROM files f
             JOIN collection_roots r ON r.id=f.root_id
             JOIN collections c ON c.id=f.collection_id
             WHERE c.mediahost_id=?1 AND c.remote_id=?2 AND r.token=?3
               AND f.media_json IS NOT NULL AND f.size IS NOT NULL
               AND (f.path=?4 OR EXISTS(
                     SELECT 1 FROM json_each(f.media_json,'$.external_subtitles') e
                     WHERE json_extract(e.value,'$.path_rel')=?4))
             LIMIT 1"
        )))
        .bind(host)
        .bind(collection)
        .bind(&source.root_token)
        .bind(&source.path_rel)
        .fetch_optional(self.db.read_pool())
        .await?;
        row.as_ref().map(source_file).transpose()
    }

    /// Settled, whoever did the work: an urgent extraction for a viewer
    /// answers the same question the queue asked.
    pub async fn finish_subtitle_job(&self, file_id: &str, kind: &str) -> Result<()> {
        kind_ok(kind)?;
        sqlx::query(
            "UPDATE subtitle_jobs SET state='done',token=NULL,lease_until=0,due_at=0,error=NULL
             WHERE file_id=? AND kind=?",
        )
        .bind(file_id)
        .bind(kind)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// The `FileSubtitles` landing names its source, not our file id.
    /// Returns whether a row was settled.
    pub async fn finish_subtitle_source(
        &self,
        host: &str,
        collection: &str,
        source: &kahawai_proto::v1::SourcePath,
        kind: &str,
    ) -> Result<bool> {
        kind_ok(kind)?;
        Ok(sqlx::query(
            "UPDATE subtitle_jobs SET state='done',token=NULL,lease_until=0,due_at=0,error=NULL
             WHERE kind=?1 AND file_id=(SELECT f.id FROM files f JOIN collections c ON c.id=f.collection_id
                  JOIN collection_roots r ON r.id=f.root_id
                  WHERE c.mediahost_id=?2 AND c.remote_id=?3 AND r.token=?4
                    AND (f.path=?5 OR EXISTS(SELECT 1 FROM json_each(f.media_json,'$.external_subtitles') e
                                             WHERE json_extract(e.value,'$.path_rel')=?5)))",
        )
        .bind(kind)
        .bind(host)
        .bind(collection)
        .bind(&source.root_token)
        .bind(&source.path_rel)
        .execute(&self.db)
        .await?
        .rows_affected()
            > 0)
    }

    /// A failure the mediahost reported for one file: retry at `due_at`,
    /// or park the row once it has been reported too often.
    pub async fn fail_subtitle_source(
        &self,
        host: &str,
        collection: &str,
        source: &kahawai_proto::v1::SourcePath,
        kind: &str,
        due_at: i64,
        error: &str,
    ) -> Result<bool> {
        kind_ok(kind)?;
        Ok(sqlx::query(
            "UPDATE subtitle_jobs SET state=CASE WHEN attempts>=?6 THEN 'blocked' ELSE 'retry' END,
                    due_at=?7,error=?8,token=NULL,lease_until=0
             WHERE kind=?1 AND state<>'done' AND file_id=(SELECT f.id FROM files f JOIN collections c ON c.id=f.collection_id
                  JOIN collection_roots r ON r.id=f.root_id
                  WHERE c.mediahost_id=?2 AND c.remote_id=?3 AND r.token=?4 AND f.path=?5)",
        )
        .bind(kind)
        .bind(host)
        .bind(collection)
        .bind(&source.root_token)
        .bind(&source.path_rel)
        .bind(BLOCK_AFTER_ATTEMPTS)
        .bind(due_at)
        .bind(error)
        .execute(&self.db)
        .await?
        .rows_affected()
            > 0)
    }

    /// A mediahost came back: its offered work is gone with its old
    /// process. A reconnect is a state change, not a failure, so the
    /// attempt count starts over.
    pub async fn release_subtitle_host(&self, host: &str) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE subtitle_jobs SET state='pending',due_at=0,attempts=0,token=NULL,lease_until=0
             WHERE host=? AND state='running'",
        )
        .bind(host)
        .execute(&self.db)
        .await?
        .rows_affected())
    }

    /// Administrator rerun: parked, waiting and settled rows go again from
    /// the top. Also the repair for a cache deleted by hand, since a claim
    /// re-checks the disk.
    pub async fn rerun_subtitle_jobs(&self, kind: &str) -> Result<u64> {
        kind_ok(kind)?;
        Ok(sqlx::query(
            "UPDATE subtitle_jobs SET state='pending',due_at=0,attempts=0,token=NULL,lease_until=0,error=NULL
             WHERE kind=? AND state IN ('blocked','retry','done')",
        )
        .bind(kind)
        .execute(&self.db)
        .await?
        .rows_affected())
    }

    /// When the clock next makes a row claimable, if ever.
    pub async fn subtitle_jobs_next_due(&self, kind: &str) -> Result<Option<i64>> {
        kind_ok(kind)?;
        Ok(sqlx::query_scalar(
            "SELECT min(CASE state WHEN 'running' THEN lease_until ELSE due_at END)
             FROM subtitle_jobs WHERE kind=? AND state IN ('pending','retry','running')",
        )
        .bind(kind)
        .fetch_one(self.db.read_pool())
        .await?)
    }

    pub async fn subtitle_jobs_status(&self) -> Result<Vec<SubtitleWorkStatus>> {
        Ok(sqlx::query(
            "SELECT kind,state,count(*) AS n,min(due_at) AS due_at,max(error) AS error
             FROM subtitle_jobs GROUP BY kind,state ORDER BY kind,state",
        )
        .fetch_all(self.db.read_pool())
        .await?
        .into_iter()
        .map(|r| SubtitleWorkStatus {
            kind: r.get("kind"),
            state: r.get("state"),
            count: r.get("n"),
            due_at: r.get("due_at"),
            error: r.get("error"),
        })
        .collect())
    }
}

/// Insert or reset both rows for one probed file inside the catalogue
/// transaction. `changed` is the byte-change verdict `apply_catalogue`
/// already computed.
pub(crate) async fn upsert_subtitle_jobs(
    tx: &mut sqlx::SqliteConnection,
    file_id: &str,
    changed: bool,
) -> Result<()> {
    for kind in SUBTITLE_KINDS {
        sqlx::query(
            "INSERT INTO subtitle_jobs(file_id,kind) VALUES(?1,?2)
         ON CONFLICT(file_id,kind) DO UPDATE SET
           state=CASE WHEN ?3 THEN 'pending' ELSE state END,
           due_at=CASE WHEN ?3 THEN 0 ELSE due_at END,
           attempts=CASE WHEN ?3 THEN 0 ELSE attempts END,
           token=CASE WHEN ?3 THEN NULL ELSE token END,
           lease_until=CASE WHEN ?3 THEN 0 ELSE lease_until END,
           host=CASE WHEN ?3 THEN NULL ELSE host END,
           error=CASE WHEN ?3 THEN NULL ELSE error END",
        )
        .bind(file_id)
        .bind(kind)
        .bind(changed)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}
