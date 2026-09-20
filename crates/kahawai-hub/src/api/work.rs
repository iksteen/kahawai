//! One view of every background queue the deployment runs, in one shape.
//!
//! Three areas feed it: hub enrichment (one queue per provider), hub
//! subtitle work (text prewarm, display sets, OCR) and mediahost discovery (one queue
//! per collection and kind, from the host's last status report). The
//! first two are durable tables an administrator can rerun; discovery
//! selects its own work from missing local facts and only reports.
use super::*;

#[derive(Serialize, ToSchema)]
pub struct WorkQueue {
    /// `enrichment`, `subtitles` or `discovery`.
    pub area: String,
    /// The provider, the subtitle kind, or the discovery kind.
    pub queue: String,
    #[schema(required)]
    pub host: Option<String>,
    #[schema(required)]
    pub collection: Option<String>,
    pub pending: i64,
    pub running: i64,
    pub retry: i64,
    pub blocked: i64,
    pub done: i64,
    /// Unix time the clock next makes a row claimable, when known.
    #[schema(required)]
    pub next_due: Option<i64>,
    #[schema(required)]
    pub error: Option<String>,
    /// Whether `POST /admin/v1/work/rerun` applies to this queue.
    pub rerun: bool,
}

#[derive(Serialize, ToSchema)]
pub struct WorkResponse {
    pub queues: Vec<WorkQueue>,
}

#[derive(Deserialize, ToSchema)]
pub struct WorkRerun {
    pub area: String,
    pub queue: String,
}

fn queue(area: &str, name: &str) -> WorkQueue {
    WorkQueue {
        area: area.into(),
        queue: name.into(),
        host: None,
        collection: None,
        pending: 0,
        running: 0,
        retry: 0,
        blocked: 0,
        done: 0,
        next_due: None,
        error: None,
        rerun: false,
    }
}

impl WorkQueue {
    fn fold(&mut self, state: &str, count: i64, due_at: i64, error: Option<String>) {
        match state {
            "pending" => self.pending += count,
            "running" => self.running += count,
            "retry" => self.retry += count,
            "blocked" => self.blocked += count,
            "done" => self.done += count,
            _ => {}
        }
        if state != "done" && due_at > 0 {
            self.next_due = Some(self.next_due.map_or(due_at, |d| d.min(due_at)));
        }
        // A settled row's old error is history, not a reason to look.
        if state != "done" && error.is_some() && self.error.is_none() {
            self.error = error;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_settled_rows_old_error_is_not_carried_as_attention() {
        let mut q = queue("enrichment", "tmdb");
        q.fold("done", 4, 0, Some("timed out once".into()));
        q.fold("retry", 1, 1_700, Some("rate limited".into()));
        q.fold("blocked", 1, 0, None);
        assert_eq!((q.done, q.retry, q.blocked), (4, 1, 1));
        assert_eq!(q.next_due, Some(1_700));
        assert_eq!(q.error.as_deref(), Some("rate limited"));
    }
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/v1/work", get(work_status))
        .route("/admin/v1/work/rerun", post(work_rerun))
}

/// Every background queue and how far it is
///
/// Admin only. Enrichment and subtitle queues come from the hub's durable
/// job tables; discovery queues are each mediahost's last report for a
/// collection, so they have no retry or done counts.
#[utoipa::path(get, path = "/admin/v1/work", tag = "Admin work",
    security(("bearer_auth" = [])),
    responses((status = 200, body = WorkResponse), (status = 401, body = ApiErrorBody), (status = 403, body = ApiErrorBody)))]
pub(super) async fn work_status(State(s): State<AppState>) -> Result<Json<WorkResponse>, ApiError> {
    let store = s.registry.catalogue();
    let mut queues: Vec<WorkQueue> = Vec::new();
    for row in store.enrichment_status().await.map_err(internal)? {
        let q = match queues
            .iter_mut()
            .find(|q| q.area == "enrichment" && q.queue == row.provider)
        {
            Some(q) => q,
            None => {
                let mut q = queue("enrichment", &row.provider);
                q.rerun = true;
                queues.push(q);
                queues.last_mut().expect("just pushed")
            }
        };
        q.fold(&row.state, row.count, row.due_at, row.error);
    }
    {
        for kind in kahawai_mediadb::SUBTITLE_KINDS {
            let mut q = queue("subtitles", kind);
            q.rerun = true;
            queues.push(q);
        }
        for row in store.subtitle_jobs_status().await.map_err(internal)? {
            if let Some(q) = queues
                .iter_mut()
                .find(|q| q.area == "subtitles" && q.queue == row.kind)
            {
                q.fold(&row.state, row.count, row.due_at, row.error);
            }
        }
        // OCR is not a table: the worker counts what it has in hand.
        let counters = s.subtitles.ocr_counters();
        let mut ocr = queue("subtitles", "ocr");
        ocr.rerun = true;
        ocr.pending = counters.queued.load(std::sync::atomic::Ordering::Relaxed) as i64;
        ocr.running = counters.busy.load(std::sync::atomic::Ordering::Relaxed) as i64;
        ocr.done = counters.done.load(std::sync::atomic::Ordering::Relaxed) as i64;
        ocr.blocked = counters.failed.load(std::sync::atomic::Ordering::Relaxed) as i64;
        queues.push(ocr);
    }
    let hosts = s.registry.satellites_overview().await.map_err(internal)?;
    for row in store.collection_summaries().await.map_err(internal)? {
        let c = row.collection;
        let Some(report) = s.registry.discovery_status(&c.mediahost_id, &c.remote_id) else {
            continue;
        };
        let host = hosts
            .iter()
            .find(|h| h.module_id == c.mediahost_id)
            .map(|h| h.name.clone())
            .unwrap_or_else(|| c.mediahost_id.clone());
        let counts = [
            ("scan", 0_u64, report.scanning),
            ("cheap", report.pending_cheap, false),
            ("hashes", report.pending_hashes, false),
            (
                "segments",
                report.pending_segments,
                report.segments_enabled == Some(false),
            ),
            ("loudness", report.pending_loudness, false),
        ];
        for (kind, pending, flag) in counts {
            if kind == "segments" && flag {
                continue; // detection is off on that host: not a queue
            }
            if kind == "scan" && !flag && pending == 0 {
                continue; // a finished scan has nothing to report
            }
            let mut q = queue("discovery", kind);
            q.host = Some(host.clone());
            q.collection = Some(c.remote_id.clone());
            q.pending = pending as i64;
            q.running = i64::from(kind == "scan" && flag);
            queues.push(q);
        }
    }
    Ok(Json(WorkResponse { queues }))
}

/// Run a queue's parked and waiting rows again
///
/// Admin only. Enrichment: releases the provider's configuration block and
/// its blocked jobs. Subtitle text and sets: blocked, waiting and done rows
/// go again, which also repairs a cache deleted by hand. Subtitle OCR: failure markers
/// are forgotten and the worker is seeded again from the sets on disk.
/// Discovery queues select their own work and cannot be rerun from here.
#[utoipa::path(post, path = "/admin/v1/work/rerun", tag = "Admin work",
    request_body = WorkRerun, security(("bearer_auth" = [])),
    responses((status = 200, body = OkResponse), (status = 400, body = ApiErrorBody), (status = 401, body = ApiErrorBody), (status = 403, body = ApiErrorBody)))]
pub(super) async fn work_rerun(
    State(s): State<AppState>,
    ApiJson(body): ApiJson<WorkRerun>,
) -> Result<Json<OkResponse>, ApiError> {
    let store = s.registry.catalogue();
    match body.area.as_str() {
        "enrichment" => {
            store
                .wake_enrichment(Some(&body.queue))
                .await
                .map_err(internal)?;
            s.enricher.request_run(s.registry.clone());
        }
        "subtitles" => match body.queue.as_str() {
            kind @ ("text" | "sets") => {
                store.rerun_subtitle_jobs(kind).await.map_err(internal)?;
                s.subtitles.wake();
            }
            "ocr" => s.subtitles.ocr_rerun(&s.registry).await,
            _ => {
                return Err(ApiError::new(
                    ErrorCode::BadRequest,
                    "unknown subtitle work kind",
                ));
            }
        },
        _ => {
            return Err(ApiError::new(
                ErrorCode::BadRequest,
                "only enrichment and subtitle queues can be rerun",
            ));
        }
    }
    Ok(Json(OkResponse { ok: true }))
}
