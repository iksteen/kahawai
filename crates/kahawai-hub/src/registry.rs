//! Registry (HUB-1): live satellite connections, enrollment, discovery status,
//! placement and settings. Durable operational state belongs to the hub DB;
//! collections, physical source facts and replay cursors belong to mediadb.
//! Link generations prevent late replies from replacing a newer connection.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use kahawai_sqlite::Database;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::Row;
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoudnessPreference {
    Off,
    Encoded,
    Force,
}

impl LoudnessPreference {
    pub fn enabled(self) -> bool {
        self != Self::Off
    }

    pub fn force(self) -> bool {
        self == Self::Force
    }
}

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

pub(crate) fn source_fingerprint(parts: &[(i64, i64, i64)]) -> String {
    use data_encoding::BASE64URL_NOPAD;

    let mut hash = Sha256::new();
    hash.update(b"kahawai-playable-source-v1\0");
    for (size, head, tail) in parts {
        hash.update(size.to_be_bytes());
        hash.update(head.to_be_bytes());
        hash.update(tail.to_be_bytes());
    }
    format!("source-sha256-{}", BASE64URL_NOPAD.encode(&hash.finalize()))
}

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
    /// Additive wire feature this plan requires. A HARD filter: choosing a
    /// peer without it would silently drop behavior.
    pub required_protocol_feature: Option<kahawai_proto::ProtocolFeature>,
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

static NEXT_HOST_LINK_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct HostLink {
    tx: tokio::sync::mpsc::Sender<Result<kahawai_proto::v1::HubToHost, tonic::Status>>,
    protocol_minor: u32,
    generation: u64,
    segment_detector_generation: i64,
    discovery: Arc<Mutex<HashMap<String, kahawai_proto::v1::DiscoveryStatus>>>,
    current: Arc<AtomicBool>,
}

impl HostLink {
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn supports_segment_detection(&self) -> bool {
        kahawai_proto::ProtocolFeatures::new(self.protocol_minor)
            .supports(kahawai_proto::ProtocolFeature::SegmentDetection)
            && self.segment_detector_generation == kahawai_core::segments::DETECTOR_GENERATION
    }
    pub(crate) fn supports_revisioned_subtitles(&self) -> bool {
        kahawai_proto::ProtocolFeatures::new(self.protocol_minor)
            .supports(kahawai_proto::ProtocolFeature::RevisionedSubtitles)
    }
    pub(crate) fn supports_loudness_analysis(&self) -> bool {
        kahawai_proto::ProtocolFeatures::new(self.protocol_minor)
            .supports(kahawai_proto::ProtocolFeature::AudioLoudnessAnalysis)
    }

    pub(crate) async fn send(&self, msg: kahawai_proto::v1::HubToHost) -> Result<()> {
        anyhow::ensure!(
            self.current.load(Ordering::Acquire),
            "mediahost link generation was replaced"
        );
        self.tx
            .send(Ok(msg))
            .await
            .map_err(|_| anyhow::anyhow!("mediahost link closed"))
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct DeletedSatellite {
    pub fingerprint: String,
    /// Exact mediahost link generation removed with the row. The API retires
    /// this generation's segment waiter before returning to the administrator.
    pub mediahost_link_generation: Option<u64>,
}

type TcSender = tokio::sync::mpsc::Sender<Result<kahawai_proto::v1::HubToTc, tonic::Status>>;

struct TcLink {
    sender: TcSender,
    protocol_minor: u32,
}

pub enum RescanResult {
    Requested,
    Offline,
    Unsupported,
}

pub struct Registry {
    db: Database,
    catalogue: kahawai_mediadb::Store,
    /// The credential store. `None` only in tests that never reach one — the
    /// composition root always sets it, and a key it cannot load is fatal
    /// there rather than absent here.
    credentials: Option<Arc<crate::secrets::Credentials>>,
    /// The live mTLS allowlist (SEC-5), mirrored from the satellites table.
    allowed: kahawai_transport::mtls::AllowedCerts,
    connected: Mutex<HashMap<String, SatelliteState>>,
    /// Live capability reports from connected transcoders (TC-1); cleared
    /// on disconnect — a report is only valid while the link is up.
    transcoder_caps: Mutex<HashMap<String, TranscoderCapabilities>>,
    /// HUB-36: what AIO's optional full local transcoder measured about
    /// itself. Plain hub never fills this: its local worker is limited to
    /// remux and audio-only transcode, neither of which needs video pace.
    local_bench: Mutex<Option<kahawai_media::bench::BenchResults>>,
    /// Structural startup choice for FULL local video execution. The hub's
    /// lightweight remux/audio worker is always available; only AIO may add
    /// video encode, tone-map and subtitle burn-in here.
    local_video_executor_enabled: bool,
    /// Sender and negotiated minor are one link fact. Keeping them in separate
    /// maps let a reconnect briefly pair the new sender with the old minor,
    /// defeating protocol-feature hard filters during exactly that window.
    tc_links: Mutex<HashMap<String, TcLink>>,
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
    /// Command senders and negotiated protocol minors for connected hosts.
    links: Mutex<HashMap<String, HostLink>>,
    catalog_apply_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Live per-collection scan progress (HUB-35): last report wins.
    scan_progress: Mutex<HashMap<(String, String), ScanState>>,
    /// HUB-11 event bus: invalidation hints pushed to /api/v1/events
    /// subscribers ({kind, ...} JSON). Lagging receivers drop events —
    /// hints, not state; clients refetch what a hint names.
    events: tokio::sync::broadcast::Sender<RegistryEvent>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ScanState {
    pub scanned: u32,
    pub failed: u32,
    pub skipped: u32,
    pub complete: bool,
    #[serde(skip)]
    pub updated: SystemTime,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EncoderCapability {
    pub codec: String,
    pub element: String,
    pub hardware: bool,
    #[schema(required)]
    pub speed_1080: Option<f32>,
    #[schema(required)]
    pub speed_2160: Option<f32>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TranscoderCapabilities {
    pub encoders: Vec<EncoderCapability>,
    pub max_sessions: u32,
    pub decode_caps: Vec<String>,
    pub tonemap: bool,
    pub ass_burn: bool,
    #[schema(required)]
    pub tonemap_speed_1080: Option<f32>,
    #[schema(required)]
    pub tonemap_speed_2160: Option<f32>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SatellitePace {
    pub class: String,
    pub multiple: f64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SatelliteOverview {
    pub module_id: String,
    pub module_type: String,
    pub name: String,
    pub cert_fingerprint: String,
    pub enrolled_at: i64,
    pub connected: bool,
    #[schema(required)]
    pub build: Option<String>,
    #[schema(required)]
    pub capabilities: Option<TranscoderCapabilities>,
    pub disabled: bool,
    pub pace: Vec<SatellitePace>,
    #[schema(required)]
    pub link_bytes_per_sec: Option<u64>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(untagged)]
pub enum RegistryEvent {
    Scan {
        kind: &'static str,
        module_id: String,
        collection_id: String,
        scanned: u32,
        failed: u32,
        skipped: u32,
        complete: bool,
    },
    Satellite {
        kind: &'static str,
        module_id: String,
        connected: bool,
    },
    EnrichChain {
        kind: &'static str,
        chain: String,
    },
    EnrichRunning {
        kind: &'static str,
        running: bool,
    },
    Sessions {
        kind: &'static str,
    },
}

impl Registry {
    pub fn new(
        db: Database,
        allowed: kahawai_transport::mtls::AllowedCerts,
        catalogue: kahawai_mediadb::Store,
    ) -> Self {
        Self {
            db,
            catalogue,
            credentials: None,
            allowed,
            connected: Mutex::new(HashMap::new()),
            links: Mutex::new(HashMap::new()),
            catalog_apply_locks: Mutex::new(HashMap::new()),
            transcoder_caps: Mutex::new(HashMap::new()),
            local_bench: Mutex::new(None),
            local_video_executor_enabled: false,
            tc_links: Mutex::new(HashMap::new()),
            tc_load: Mutex::new(HashMap::new()),
            tc_pace: Mutex::new(HashMap::new()),
            tc_link_rate: Mutex::new(HashMap::new()),
            disabled: Mutex::new(std::collections::HashSet::new()),
            scan_progress: Mutex::new(HashMap::new()),
            events: tokio::sync::broadcast::channel(256).0,
        }
    }

    /// Applied while constructing the registry, before it is shared.
    pub fn with_credentials(mut self, credentials: Arc<crate::secrets::Credentials>) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// The credential store, where there is one.
    pub fn credentials(&self) -> Option<&crate::secrets::Credentials> {
        self.credentials.as_deref()
    }

    /// Everything the hub holds for one provider, empty when nothing is
    /// configured — and also when there is no store, which only a test
    /// registry can be. Here so each provider's helper does not repeat that
    /// second decision.
    pub async fn hub_credential(
        &self,
        provider: &str,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        match self.credentials() {
            Some(store) => store.get_provider(crate::secrets::HUB, provider).await,
            None => Ok(Default::default()),
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
    pub fn emit(&self, event: RegistryEvent) {
        let _ = self.events.send(event); // no subscribers = no-op
    }

    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<RegistryEvent> {
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
        self.emit(RegistryEvent::Scan {
            kind: "scan",
            module_id: module_id.to_string(),
            collection_id: collection_id.to_string(),
            scanned,
            failed,
            skipped,
            complete,
        });
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

    /// Default is normalization when audio is already encoded. `off` disables
    /// it; `force` may turn a direct/copy audio path into an encode while
    /// retaining the video mode. Unknown values preserve the default.
    pub async fn loudness_normalization(&self, user_id: &str) -> Result<LoudnessPreference> {
        let value: Option<String> = sqlx::query_scalar(
            "SELECT value FROM user_prefs
              WHERE user_id=? AND scope='' AND key='loudness_normalization'",
        )
        .bind(user_id)
        .fetch_optional(&self.db)
        .await?;
        Ok(match value.as_deref() {
            Some("off") => LoudnessPreference::Off,
            Some("force") => LoudnessPreference::Force,
            _ => LoudnessPreference::Encoded,
        })
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
        protocol_minor: u32,
        segment_detector_generation: i64,
    ) -> (u64, Option<u64>) {
        let generation = NEXT_HOST_LINK_GENERATION.fetch_add(1, Ordering::Relaxed);
        let mut links = self.links.lock().unwrap();
        let replaced = links.get(module_id).map(|link| {
            // Invalidate clones before the replacement becomes observable.
            // Segment waiters carry this token, so an old result racing the
            // later waiter-drain cannot win after publication.
            link.current.store(false, Ordering::Release);
            link.generation
        });
        links.insert(
            module_id.to_string(),
            HostLink {
                tx,
                protocol_minor,
                generation,
                segment_detector_generation,
                discovery: Default::default(),
                current: Arc::new(AtomicBool::new(true)),
            },
        );
        (generation, replaced)
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
    pub fn unregister_link(&self, module_id: &str) -> Option<u64> {
        self.links.lock().unwrap().remove(module_id).map(|link| {
            link.current.store(false, Ordering::Release);
            link.generation
        })
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
            Some(current) if current.tx.same_channel(tx) => {
                current.current.store(false, Ordering::Release);
                links.remove(module_id);
                if let Some(s) = self.connected.lock().unwrap().get_mut(module_id) {
                    s.connected = false;
                    s.last_seen = SystemTime::now();
                }
                drop(links);
                tracing::info!(%module_id, "satellite disconnected");
                self.emit(RegistryEvent::Satellite {
                    kind: "satellite",
                    module_id: module_id.to_string(),
                    connected: false,
                });
                true
            }
            _ => false,
        }
    }

    /// Capture the sender and its negotiated feature level together. Older hosts
    /// must never silently turn an explicit deep scan into an incremental scan.
    pub async fn rescan_collection(
        &self,
        module: &str,
        collection: &str,
        deep: bool,
    ) -> RescanResult {
        let sender = {
            let links = self.links.lock().unwrap();
            let Some(link) = links.get(module) else {
                return RescanResult::Offline;
            };
            if deep
                && !kahawai_proto::ProtocolFeatures::new(link.protocol_minor)
                    .supports(kahawai_proto::ProtocolFeature::DeepRescan)
            {
                return RescanResult::Unsupported;
            }
            link.tx.clone()
        };
        let message = kahawai_proto::v1::HubToHost {
            msg: Some(kahawai_proto::v1::hub_to_host::Msg::RescanRequest(
                kahawai_proto::v1::RescanRequest {
                    collection_id: collection.into(),
                    deep,
                },
            )),
        };
        if sender.send(Ok(message)).await.is_ok() {
            RescanResult::Requested
        } else {
            RescanResult::Offline
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
            .map(|link| link.tx.clone())
            .with_context(|| format!("mediahost {module_id} is not connected"))?;
        tx.send(Ok(msg))
            .await
            .map_err(|_| anyhow::anyhow!("link to {module_id} closed"))
    }

    pub(crate) async fn send_to_host_generation(
        &self,
        module_id: &str,
        generation: u64,
        msg: kahawai_proto::v1::HubToHost,
    ) -> Result<()> {
        let link = self
            .host_link(module_id)
            .filter(|link| link.generation() == generation)
            .with_context(|| format!("mediahost link generation {generation} was replaced"))?;
        link.send(msg).await
    }

    /// Ephemeral reports belong to a live link, not a second durable work queue.
    /// Reconnect starts unknown; late messages cannot repopulate the new link.
    pub fn report_discovery(
        &self,
        module: &str,
        generation: u64,
        status: kahawai_proto::v1::DiscoveryStatus,
    ) {
        if let Some(link) = self
            .links
            .lock()
            .unwrap()
            .get(module)
            .filter(|l| l.generation == generation)
        {
            link.discovery
                .lock()
                .unwrap()
                .insert(status.collection_id.clone(), status);
        }
    }

    pub fn discovery_status(
        &self,
        module: &str,
        collection: &str,
    ) -> Option<kahawai_proto::v1::DiscoveryStatus> {
        self.links
            .lock()
            .unwrap()
            .get(module)?
            .discovery
            .lock()
            .unwrap()
            .get(collection)
            .cloned()
    }

    /// Administrative wake only. Protocol-4 mediahosts own queue selection;
    /// the hub broadcasts interest without naming a season or exact source.
    pub async fn wake_discovery(&self, kind: &str, modules: &[String]) -> usize {
        let links: Vec<_> = self
            .links
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| modules.contains(id))
            .map(|(_, link)| link.tx.clone())
            .collect();
        let mut accepted = 0;
        for link in links {
            if link
                .try_send(Ok(kahawai_proto::v1::HubToHost {
                    msg: Some(kahawai_proto::v1::hub_to_host::Msg::DiscoveryWake(
                        kahawai_proto::v1::DiscoveryWake {
                            kind: kind.to_string(),
                            collection_id: String::new(),
                        },
                    )),
                }))
                .is_ok()
            {
                accepted += 1;
            }
        }
        accepted
    }

    /// Best-effort, short-lived demand signal. It is deliberately lossy: the
    /// durable catalogue remains authoritative and the mediahost will still
    /// complete ordinary backfill if this hint misses a reconnect window.
    pub fn hint_discovery(
        &self,
        module_id: &str,
        kind: &str,
        collection_id: &str,
        source: kahawai_proto::v1::SourcePath,
        reason: &str,
        ttl_seconds: u32,
    ) -> bool {
        let link = self.links.lock().unwrap();
        let Some(link) = link.get(module_id) else {
            return false;
        };
        if !kahawai_proto::ProtocolFeatures::new(link.protocol_minor)
            .supports(kahawai_proto::ProtocolFeature::DiscoveryPriorityHints)
        {
            return false;
        }
        link.tx
            .try_send(Ok(kahawai_proto::v1::HubToHost {
                msg: Some(kahawai_proto::v1::hub_to_host::Msg::DiscoveryPriorityHint(
                    kahawai_proto::v1::DiscoveryPriorityHint {
                        kind: kind.to_string(),
                        collection_id: collection_id.to_string(),
                        source: Some(source),
                        reason: reason.to_string(),
                        ttl_seconds,
                    },
                )),
            }))
            .is_ok()
    }

    pub(crate) fn host_link(&self, module_id: &str) -> Option<HostLink> {
        self.links.lock().unwrap().get(module_id).cloned()
    }

    pub(crate) fn host_link_is_current(&self, module_id: &str, generation: u64) -> bool {
        self.links
            .lock()
            .unwrap()
            .get(module_id)
            .is_some_and(|link| {
                link.generation == generation && link.current.load(Ordering::Acquire)
            })
    }

    pub(crate) fn catalog_apply_lock(&self, module_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.catalog_apply_locks
            .lock()
            .unwrap()
            .entry(module_id.to_string())
            .or_default()
            .clone()
    }

    pub fn host_supports_segment_detection(&self, module_id: &str) -> bool {
        self.host_link(module_id)
            .is_some_and(|link| link.supports_segment_detection())
    }
    pub fn host_supports_loudness_analysis(&self, module_id: &str) -> bool {
        self.host_link(module_id)
            .is_some_and(|link| link.supports_loudness_analysis())
    }

    pub fn catalogue(&self) -> &kahawai_mediadb::Store {
        &self.catalogue
    }

    pub fn db(&self) -> &Database {
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
        self.emit(RegistryEvent::Satellite {
            kind: "satellite",
            module_id: module_id.to_string(),
            connected: true,
        });
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
            self.emit(RegistryEvent::Satellite {
                kind: "satellite",
                module_id: module_id.to_string(),
                connected: false,
            });
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

    pub fn set_transcoder_caps(&self, module_id: &str, caps: &kahawai_proto::v1::CapabilityReport) {
        let capabilities = TranscoderCapabilities {
            encoders: caps
                .encoders
                .iter()
                .map(|e| EncoderCapability {
                    codec: e.codec.clone(),
                    element: e.element.clone(),
                    hardware: e.hardware,
                    speed_1080: e.speed_1080,
                    speed_2160: e.speed_2160,
                })
                .collect(),
            max_sessions: caps.max_sessions,
            decode_caps: caps.decode_caps.clone(),
            tonemap: caps.tonemap,
            ass_burn: caps.ass_burn,
            tonemap_speed_1080: caps.tonemap_speed_1080,
            tonemap_speed_2160: caps.tonemap_speed_2160,
        };
        self.transcoder_caps
            .lock()
            .unwrap()
            .insert(module_id.to_string(), capabilities);
    }

    /// HUB-15b: the verified encoder codec names a transcoder reported
    /// ("h264", "hevc", "aac", …) — what negotiation may pick as an
    /// encode target when this box would run the session.
    pub fn transcoder_encoders(&self, module_id: &str) -> Vec<String> {
        self.transcoder_caps
            .lock()
            .unwrap()
            .get(module_id)
            .map(|c| c.encoders.iter().map(|e| e.codec.clone()).collect())
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
        let encoder = caps
            .get(module_id)?
            .encoders
            .iter()
            .find(|encoder| encoder.codec == codec)?;
        if height > 1080 {
            encoder.speed_2160
        } else {
            encoder.speed_1080
        }
    }

    /// Same for the GL tone-map segment (HUB-15a's boolean, measured).
    pub fn transcoder_tonemap_speed(&self, module_id: &str, height: u32) -> Option<f32> {
        let caps = self.transcoder_caps.lock().unwrap();
        let capabilities = caps.get(module_id)?;
        if height > 1080 {
            capabilities.tonemap_speed_2160
        } else {
            capabilities.tonemap_speed_1080
        }
    }

    /// HUB-15a: does this transcoder report the GL tone-map segment?
    pub fn transcoder_reports_tonemap(&self, module_id: &str) -> bool {
        self.transcoder_caps
            .lock()
            .unwrap()
            .get(module_id)
            .is_some_and(|c| c.tonemap)
    }

    pub fn transcoder_reports_ass_burn(&self, module_id: &str) -> bool {
        self.transcoder_caps
            .lock()
            .unwrap()
            .get(module_id)
            .is_some_and(|c| c.ass_burn)
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
            .any(|c| c.ass_burn)
    }

    pub fn clear_transcoder_caps(&self, module_id: &str) {
        self.transcoder_caps.lock().unwrap().remove(module_id);
    }

    pub fn register_tc_link(&self, module_id: &str, protocol_minor: u32, tx: TcSender) {
        self.tc_links.lock().unwrap().insert(
            module_id.to_string(),
            TcLink {
                sender: tx,
                protocol_minor,
            },
        );
    }

    pub fn transcoder_protocol_minor(&self, module_id: &str) -> Option<u32> {
        self.tc_links
            .lock()
            .unwrap()
            .get(module_id)
            .map(|link| link.protocol_minor)
    }

    pub fn transcoder_protocol_features(
        &self,
        module_id: &str,
    ) -> Option<kahawai_proto::ProtocolFeatures> {
        self.transcoder_protocol_minor(module_id)
            .map(kahawai_proto::ProtocolFeatures::new)
    }

    pub fn transcoder_supports_layout_gains(&self, module_id: &str) -> bool {
        self.transcoder_protocol_features(module_id)
            .is_some_and(|features| {
                features.supports(kahawai_proto::ProtocolFeature::ExactAudioLoudnessGains)
            })
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
    pub fn unregister_tc_link_if_current(&self, module_id: &str, tx: &TcSender) -> bool {
        let mut links = self.tc_links.lock().unwrap();
        match links.get(module_id) {
            Some(current) if current.sender.same_channel(tx) => {
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
        self.send_to_tc_requiring(module_id, msg, None).await
    }

    pub async fn send_to_tc_requiring(
        &self,
        module_id: &str,
        msg: kahawai_proto::v1::HubToTc,
        required: Option<kahawai_proto::ProtocolFeature>,
    ) -> anyhow::Result<()> {
        let tx = {
            let links = self.tc_links.lock().unwrap();
            let link = links
                .get(module_id)
                .ok_or_else(|| anyhow::anyhow!("transcoder {module_id} not connected"))?;
            anyhow::ensure!(
                required.is_none_or(|feature| {
                    kahawai_proto::ProtocolFeatures::new(link.protocol_minor).supports(feature)
                }),
                "transcoder {module_id} no longer supports the required protocol feature"
            );
            // The sender and minor came from one locked link entry. A
            // reconnect after this clone can only close this compatible
            // sender; it cannot redirect the message to an incompatible one.
            link.sender.clone()
        };
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
                let encoders = &c.encoders;
                let has = |codec: &str| encoders.iter().any(|e| e.codec == codec);
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
                let can = |wanted: &[String]| {
                    c.decode_caps.is_empty() || wanted.iter().any(|w| c.decode_caps.contains(w))
                };
                if (need.encode_video && !can(&need.video_caps))
                    || (need.encode_audio && !can(&need.audio_caps))
                {
                    return None;
                }
                let current = load.get(id).copied().unwrap_or(0);
                let max = c.max_sessions as usize;
                if max > 0 && current >= max {
                    return None; // at capacity (TC-6)
                }
                // Rank hardware on the codec the session will actually
                // run (empty need: any hw video encoder counts).
                if need.required_protocol_feature.is_some_and(|feature| {
                    !links.get(id).is_some_and(|link| {
                        kahawai_proto::ProtocolFeatures::new(link.protocol_minor).supports(feature)
                    })
                }) {
                    return None;
                }
                if need.needs_ass_burn && !c.ass_burn {
                    return None; // cannot burn ASS; not a candidate at all
                }
                let hw = encoders.iter().any(|e| {
                    e.hardware
                        && match need.video_codec.as_str() {
                            "" => true,
                            c => e.codec == c,
                        }
                });
                // HUB-15a: an HDR encode prefers a box that can tone-map
                // — a preference, not a filter: with no capable box the
                // job still runs (worker encodes as-is, verdict said so).
                let tm = !need.needs_tonemap || c.tonemap;
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
        let pos = |v: f32| (v > 0.0).then_some(v); // 0 on the wire = unmeasured

        let mut terms: Vec<f32> = Vec::new();
        let best = caps
            .encoders
            .iter()
            .filter(|e| match need.video_codec.as_str() {
                "" => true,
                c => e.codec == c,
            })
            .filter_map(|e| if big { e.speed_2160 } else { e.speed_1080 })
            .filter_map(pos)
            .fold(None::<f32>, |acc, v| Some(acc.map_or(v, |a| a.max(v))));
        terms.extend(best);
        if need.needs_tonemap {
            let tm = if big {
                caps.tonemap_speed_2160
            } else {
                caps.tonemap_speed_1080
            }
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
            .filter(|(element, _)| bench.encoder_ready(element))
            .filter(|(element, _)| match need.video_codec.as_str() {
                "" => true,
                c => element_encodes(element, c),
            })
            .filter_map(|(_, s)| pick(s))
            .fold(None::<f32>, |acc, v| Some(acc.map_or(v, |a| a.max(v))));
        terms.extend(best);
        if need.needs_tonemap && bench.tonemap_ready() {
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

    pub async fn satellites_overview(&self) -> Result<Vec<SatelliteOverview>> {
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
                let mut observed: Vec<SatellitePace> = pace
                    .iter()
                    .filter(|((m, _), _)| *m == id)
                    .map(|((_, class), multiple)| SatellitePace {
                        class: class.clone(),
                        multiple: *multiple,
                    })
                    .collect();
                observed.sort_by(|a, b| a.class.cmp(&b.class));
                SatelliteOverview {
                    module_id: id.clone(),
                    module_type: r.get("module_type"),
                    name: r.get("name"),
                    cert_fingerprint: r.get("cert_fingerprint"),
                    enrolled_at: r.get("enrolled_at"),
                    connected: state.is_some_and(|s| s.connected),
                    build: state.map(|s| s.build.clone()),
                    capabilities: caps.get(&id).cloned(),
                    disabled: self.disabled.lock().unwrap().contains(&id),
                    pace: observed,
                    link_bytes_per_sec: link_rates.get(&id).copied(),
                }
            })
            .collect())
    }

    /// Remove a satellite's mediadb catalogue, then revoke enrollment and its
    /// active link. Legacy catalogue/history rows in hub.db remain untouched.
    /// Both steps share the per-host ingestion/registration gate. Catalogue
    /// deletion commits first, so a crash leaves either a revoked empty host
    /// or an enrolled host that can reimport; no cross-database outbox is needed.
    /// Returns the removed identity and exact mediahost link generation so the
    /// composition layer can retire work owned by the deleted connection.
    /// Transient disconnects never come here.
    pub async fn delete_satellite(&self, module_id: &str) -> Result<DeletedSatellite> {
        let gate = self.catalog_apply_lock(module_id);
        let _guard = gate.lock().await;
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

        let pending: Option<String> =
            sqlx::query_scalar("SELECT pending_fingerprint FROM satellites WHERE module_id=?")
                .bind(module_id)
                .fetch_one(&self.db)
                .await?;
        // Delete catalogue data before durable revocation. If the second commit
        // fails or the process stops, an enrolled peer can reimport on reconnect.
        let mediahost_link_generation = self.unregister_link(module_id);
        self.catalogue.remove_mediahost(module_id).await?;
        let mut tx = self.db.begin().await?;
        sqlx::query(
            "INSERT INTO satellite_audit (module_id, fingerprint, action) VALUES (?, ?, 'deleted')",
        )
        .bind(module_id)
        .bind(&fingerprint)
        .execute(&mut *tx)
        .await?;
        for sql in [
            "DELETE FROM transcoder_pace WHERE module_id=?",
            "DELETE FROM satellites WHERE module_id=?",
        ] {
            sqlx::query(sql).bind(module_id).execute(&mut *tx).await?;
        }
        tx.commit().await?;

        // Off the allowlist and off the wire: the satellite's reconnects
        // die at the TLS handshake from here on (SEC-6).
        self.allowed.remove(&fingerprint);
        if let Some(pending) = pending {
            self.allowed.remove(&pending);
        }
        self.connected.lock().unwrap().remove(module_id);
        // Deleting the satellite forgets the drain with it. This used to happen
        // by accident, because `unregister_link` cleared the set as a side
        // effect; with that gone it has to be said where it is actually true.
        // Otherwise a re-enrolled module id came back drained in memory against
        // a fresh row saying enabled.
        self.disabled.lock().unwrap().remove(module_id);
        tracing::info!(%module_id, fingerprint = %fingerprint, "satellite deleted; cert no longer admitted");
        Ok(DeletedSatellite {
            fingerprint,
            mediahost_link_generation,
        })
    }
}

impl Registry {
    pub async fn collection_root_count(&self, host: &str, remote: &str) -> Result<i64> {
        let collection = self
            .catalogue()
            .collections(host)
            .await?
            .into_iter()
            .find(|c| c.remote_id == remote);
        match collection {
            Some(c) => Ok(self.catalogue().roots(&c.id).await?.len() as i64),
            None => Ok(0),
        }
    }
}
