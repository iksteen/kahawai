//! Play sessions (HUB-18 minimal): direct play and in-hub remux (AR-10).
//!
//! ponytail: sessions are in-memory (lost on hub restart, clients reopen);
//! idle timeout and per-user concurrency limits land with HUB-18 proper.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use kahawai_proto::v1::{hub_to_host, HubToHost, OpenRead};
use sqlx::Row;

use crate::leases::{new_lease_token, Lease, Leases};
use crate::registry::Registry;

pub enum Mode {
    Direct { lease: Lease },
    Remux { dir: PathBuf, job: Arc<kahawai_media::remux::RemuxJob> },
}

pub struct Session {
    pub id: String,
    pub user_id: String,
    pub item_id: String,
    pub module_id: String,
    pub size: u64,
    pub container: Option<String>,
    pub duration_ms: Option<u64>,
    pub mode: Mode,
    touched: Mutex<std::time::Instant>,
}

impl Session {
    /// Any client activity (stream chunks, playlist/segment fetches,
    /// progress pings) keeps the session alive (HUB-18).
    pub fn touch(&self) {
        *self.touched.lock().unwrap() = std::time::Instant::now();
    }

    pub fn idle_for(&self) -> Duration {
        self.touched.lock().unwrap().elapsed()
    }
}

/// Adapts a mediahost read lease to the remuxer's random-access source
/// trait; runs on the remux feeder thread, bridging into the runtime.
struct LeaseSource {
    lease: Lease,
    size: u64,
    handle: tokio::runtime::Handle,
}

impl kahawai_media::remux::RemuxSource for LeaseSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        if offset >= self.size {
            return Ok(0);
        }
        let len = (buf.len() as u64).min(self.size - offset);
        let _guard = self.handle.enter();
        let mut stream = self.lease.read_range(offset, len).into_inner();
        self.handle.block_on(async {
            let mut filled = 0usize;
            while filled < len as usize {
                match stream.recv().await {
                    Some(Ok(bytes)) => {
                        let n = bytes.len().min(buf.len() - filled);
                        buf[filled..filled + n].copy_from_slice(&bytes[..n]);
                        filled += n;
                    }
                    Some(Err(e)) => return Err(std::io::Error::other(e)),
                    None => break,
                }
            }
            Ok(filled)
        })
    }
}

pub struct Sessions {
    pub leases: Leases,
    /// Scratch space for remux sessions (`<data_dir>/sessions`).
    scratch_root: PathBuf,
    max_per_user: usize,
    idle_timeout: Duration,
    active: Mutex<HashMap<String, Arc<Session>>>,
}

impl Sessions {
    pub fn new(scratch_root: PathBuf) -> Self {
        Self::with_limits(scratch_root, 4, Duration::from_secs(90))
    }

    pub fn with_limits(scratch_root: PathBuf, max_per_user: usize, idle_timeout: Duration) -> Self {
        // Sessions never survive a restart; stale scratch is garbage.
        let _ = std::fs::remove_dir_all(&scratch_root);
        Self {
            leases: Leases::default(),
            scratch_root,
            max_per_user,
            idle_timeout,
            active: Mutex::new(HashMap::new()),
        }
    }

    /// Reap idle sessions (HUB-18: no fetch or progress ping → teardown).
    pub fn spawn_janitor(self: &Arc<Self>) {
        let sessions = self.clone();
        // Check at half the timeout (tests use tiny timeouts), capped at 15 s.
        let period = (sessions.idle_timeout / 2)
            .clamp(Duration::from_millis(50), Duration::from_secs(15));
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            loop {
                tick.tick().await;
                let idle: Vec<String> = sessions
                    .active
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|s| s.idle_for() > sessions.idle_timeout)
                    .map(|s| s.id.clone())
                    .collect();
                for id in idle {
                    tracing::info!(session = %id, "ending idle session");
                    sessions.end(&id);
                }
            }
        });
    }

    /// Start a session for an item: pick the best available source, open a
    /// read lease on its mediahost, and for remux start the in-hub pipeline.
    pub async fn start(
        self: &Arc<Self>,
        registry: &Registry,
        user_id: &str,
        item_id: &str,
        mode: &str,
    ) -> Result<Arc<Session>> {
        let rows = sqlx::query(
            "SELECT s.module_id, s.collection_id, s.path_rel, f.size, f.streams_json
             FROM item_sources s
             JOIN files f ON (f.module_id, f.collection_id, f.path_rel)
                           = (s.module_id, s.collection_id, s.path_rel)
             WHERE s.item_id = ? ORDER BY f.size DESC",
        )
        .bind(item_id)
        .fetch_all(registry.db())
        .await?;
        if rows.is_empty() {
            bail!("no sources for item");
        }
        let user_active = self
            .active
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.user_id == user_id)
            .count();
        if user_active >= self.max_per_user {
            bail!("too many concurrent streams ({user_active}); close one first");
        }
        let source = rows
            .iter()
            .find(|r| registry.is_connected(&r.get::<String, _>("module_id")))
            .context("no source is currently available (mediahost offline)")?;

        let module_id: String = source.get("module_id");
        let collection_id: String = source.get("collection_id");
        let path_rel: String = source.get("path_rel");
        let size = source.get::<i64, _>("size") as u64;
        let info: kahawai_core::media::MediaInfo =
            serde_json::from_str(source.get::<String, _>("streams_json").as_str())
                .unwrap_or_default();

        let token = new_lease_token();
        let msg = HubToHost {
            msg: Some(hub_to_host::Msg::OpenRead(OpenRead {
                lease_token: token.clone(),
                collection_id,
                path_rel: path_rel.clone(),
            })),
        };
        let lease = self
            .leases
            .establish(&token, registry.send_to_host(&module_id, msg))
            .await?;

        let id = ulid::Ulid::new().to_string();
        let session_mode = match mode {
            "direct" => Mode::Direct { lease },
            "remux" => Mode::Remux {
                job: self.start_remux(&id, &info, lease, size).await?,
                dir: self.scratch_root.join(&id),
            },
            other => bail!("unknown mode {other:?} (direct|remux)"),
        };

        let session = Arc::new(Session {
            id,
            user_id: user_id.to_string(),
            item_id: item_id.to_string(),
            module_id,
            size,
            container: info.container.clone(),
            duration_ms: info.duration_ms,
            mode: session_mode,
            touched: Mutex::new(std::time::Instant::now()),
        });
        self.active.lock().unwrap().insert(session.id.clone(), session.clone());
        tracing::info!(session = %session.id, item = item_id, path = %path_rel, mode, "session started");
        Ok(session)
    }

    /// Spin up the remux pipeline, feed it from the lease, and wait for the
    /// playlist to materialize so the returned URL is immediately playable.
    async fn start_remux(
        &self,
        session_id: &str,
        info: &kahawai_core::media::MediaInfo,
        lease: Lease,
        size: u64,
    ) -> Result<Arc<kahawai_media::remux::RemuxJob>> {
        // The muxer stalls on unfed pads, so only claim what TS can carry —
        // decided by the muxer's own templates (single source of truth
        // with the pipeline's link logic).
        let (has_video, has_audio) = kahawai_media::remux::ts_stream_flags(info);
        if !has_video && !has_audio {
            bail!("no TS-compatible streams — this source needs a transcoder");
        }

        let dir = self.scratch_root.join(session_id);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        // The pipeline pulls (and seeks — MP4 moov-at-end needs it) from
        // the lease via a blocking adapter on the remux feeder thread.
        let source = LeaseSource { lease, size, handle: tokio::runtime::Handle::current() };
        let job = Arc::new(kahawai_media::remux::start(&dir, has_video, has_audio, Box::new(source))?);

        // Return once the playlist exists (or the pipeline died).
        let playlist = dir.join("master.m3u8");
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(e) = job.failed() {
                bail!("remux failed to start: {e}");
            }
            if playlist.exists() {
                return Ok(job);
            }
            if std::time::Instant::now() > deadline {
                bail!("remux produced no playlist in time");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Active sessions for the admin dashboard (HUB-18).
    pub fn list(&self) -> Vec<Arc<Session>> {
        let mut v: Vec<_> = self.active.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// End every session backed by a given mediahost (satellite deletion).
    pub fn end_for_module(&self, module_id: &str) -> usize {
        let ids: Vec<String> = self
            .active
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.module_id == module_id)
            .map(|s| s.id.clone())
            .collect();
        let n = ids.len();
        for id in ids {
            self.end(&id);
        }
        n
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.active.lock().unwrap().get(id).cloned()
    }

    /// Remove a session: direct leases drop (closing the byte channel);
    /// remux pipelines stop and their scratch dir is deleted.
    pub fn end(&self, id: &str) -> bool {
        let Some(session) = self.active.lock().unwrap().remove(id) else {
            return false;
        };
        if let Mode::Remux { dir, job } = &session.mode {
            job.stop();
            let _ = std::fs::remove_dir_all(dir);
        }
        tracing::info!(session = id, "session ended");
        true
    }
}
