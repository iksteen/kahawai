//! Source-bound subtitle extraction and serving (HUB-15/27/32).
//! Embedded and sidecar streams come from mediadb; acquired/OCR/rendered
//! artifacts follow the captured physical source revision. Extracted payloads
//! remain in the durable on-disk cache because rebuilding demuxes the source.

pub(crate) mod catalogue;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use kahawai_media::subtitles::{Extracted, decode_text, is_text_format, parse, to_vtt};
use serde::Serialize;

use crate::registry::Registry;
use crate::sessions::Sessions;

#[derive(Debug, Clone, Serialize)]
pub struct SubtitleEntry {
    /// "e{n}" for embedded track n, "s{n}" for sidecar n — stable within
    /// a source's stream info.
    pub key: String,
    pub kind: &'static str, // "embedded" | "sidecar"
    pub format: String,
    pub language: Option<String>,
    /// True when the source format is ASS/SSA: serving it as VTT loses
    /// styling, and HUB-32a demands that be a labeled, explicit choice.
    /// Clients with an ASS renderer (the web player, via JASSUB) fetch
    /// the faithful .ass form instead.
    pub flattened: bool,
    /// Bitmap subtitles (PGS/VobSub): rendered from the session tap's
    /// display-set stream on an overlay — no VTT form exists.
    pub image: bool,
}

/// One track plus what it means for the requesting client.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct TrackListing {
    #[serde(flatten)]
    pub track: crate::tracks::Track,
    pub delivery: crate::tracks::Delivery,
    pub note: &'static str,
    /// May the REQUESTING user remove this track? Computed per request
    /// like `delivery`, and never stored: it is a fact about who is
    /// asking, not about the track.
    ///
    /// Only `downloaded` rows qualify at all. The other hub-stored
    /// origins are caches — a raster is rebuilt by the next session
    /// that needs it, an OCR row by the idle sweep — so deleting one
    /// destroys nothing and offering it is noise. A download is
    /// different: it spent a shared, rate-limited provider quota that
    /// re-fetching spends again, which is why it is also restricted to
    /// whoever spent it, or an admin.
    pub deletable: bool,
}

/// A served ASS script: complete (cache/sidecar) or streamed while the
/// extraction pass runs.
pub enum AssBody {
    Full(String),
    Stream(tokio::sync::mpsc::Receiver<String>),
}

/// How long OCR generation waits for the mediahost's display-set walk.
/// Only the sweep generates now — nobody is waiting, and giving up
/// early only wastes the walk: the sets still arrive and get cached,
/// but the track sits in the failed set until the next hub run.
#[cfg(feature = "ocr")]
const SETS_WAIT_IDLE: std::time::Duration = std::time::Duration::from_secs(180);

/// How long between text-prewarm rounds. A round only re-reads mediadb and
/// re-sends what is still cold, so it is cheap; this paces the retry of work
/// a mediahost is still chewing through rather than the work itself.
const TEXT_PREWARM_ROUND: std::time::Duration = std::time::Duration::from_secs(900);

#[cfg(feature = "ocr")]
const OCR_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);

enum ImageSetsState {
    Ready(std::path::PathBuf),
    RetryOnReconnect,
    Unavailable,
}

#[cfg(feature = "ocr")]
enum OcrGeneration {
    Generated,
    NoText,
    RetryOnReconnect { module_id: String },
}

/// Failures in hub-owned OCR work are remembered for this process run so a
/// corrupt track cannot consume ~15 seconds of Tesseract CPU every sweep.
/// Failures whose input depends on one mediahost are different: reconnecting
/// that host is a state change, so only those entries become eligible again.
#[cfg(feature = "ocr")]
#[derive(Default)]
struct OcrSweepFailures(std::collections::HashMap<String, Option<String>>);

#[cfg(feature = "ocr")]
impl OcrSweepFailures {
    fn contains(&self, track_id: impl ToString) -> bool {
        self.0.contains_key(&track_id.to_string())
    }

    fn remember_permanent(&mut self, track_id: impl ToString) {
        self.0.insert(track_id.to_string(), None);
    }

    fn remember_until_reconnect(&mut self, track_id: impl ToString, module_id: String) {
        self.0.insert(track_id.to_string(), Some(module_id));
    }

    fn host_reconnected(&mut self, module_id: &str) {
        self.0
            .retain(|_, retry_host| retry_host.as_deref() != Some(module_id));
    }

    fn reconsider_connected(&mut self, registry: &Registry) {
        self.0.retain(|_, retry_host| {
            retry_host
                .as_deref()
                .is_none_or(|module_id| !registry.is_connected(module_id))
        });
    }
}

#[cfg(feature = "ocr")]
#[derive(Debug, PartialEq, Eq)]
enum OcrSweepWake {
    Periodic,
    MediahostReconnected(String),
    EventsLagged,
}

/// HUB-32d: how long a starting session waits for a rasterisation it
/// needs. Generous next to the measured ~3.5 s an episode takes, and
/// bounded for the same reason the burn path's wait is: a tier that is
/// not ready is a tier to skip, not one to stall on.
pub(crate) const RASTER_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Cache key for one subtitle track of one source file — shared by the
/// lazy extractors and the mediahost ingestion path. Revision-less v2 entries
/// remain on disk but cannot be promoted: their source bytes are unverifiable.
fn cache_key(
    module_id: &str,
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    key: &str,
    revision: &str,
) -> String {
    format!(
        "v3-{:016x}-{key}",
        xxhash_rust::xxh3::xxh3_64(
            format!("{module_id}\n{collection_id}\n{root_token}\n{path_rel}\n{revision}")
                .as_bytes()
        )
    )
}

pub(crate) fn promote_legacy_cache(
    exact: &std::path::Path,
    legacy: &std::path::Path,
) -> std::io::Result<()> {
    if exact.exists() || !legacy.exists() {
        return Ok(());
    }
    if let Some(parent) = exact.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Rename preserves the already-paid artifact without duplicating it. A
    // concurrent winner is harmless: if exact appeared, leave it authoritative.
    if let Err(error) = std::fs::rename(legacy, exact)
        && !exact.exists()
    {
        return Err(error);
    }
    Ok(())
}

/// The item a subtitle request named is not in the catalogue.
///
/// A type because the API cannot otherwise tell it from the provider being
/// down: an `Option::context` here reached the client as 502 "the subtitle
/// provider did not answer", so an admin searching for a typo'd id was told
/// OpenSubtitles was having an outage it was not having.
#[derive(Debug)]
pub struct NoSuchItem;

impl std::fmt::Display for NoSuchItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no such item")
    }
}

impl std::error::Error for NoSuchItem {}

pub struct Subtitles {
    provider: Option<Arc<dyn crate::opensubtitles::SubtitleProvider>>,
    dir: PathBuf,
    /// HUB-21 deployment config (kahawai.toml); wins over settings.
    provider_cfg: crate::opensubtitles::ProviderConfig,
    /// Per-cache-key locks so concurrent requests extract once.
    inflight: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Shared with every other provider caller — the rate limits are
    /// per-IP, so the queues have to be process-wide (gate.rs).
    http: Arc<crate::gate::Http>,
}

impl Subtitles {
    pub(crate) fn cache_dir(&self) -> &std::path::Path {
        &self.dir
    }
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            provider: None,
            provider_cfg: Default::default(),
            inflight: Default::default(),
            http: Arc::new(crate::gate::Http::new().expect("http client")),
        }
    }

    /// Inject a provider implementation, including deterministic integration fixtures.
    pub fn with_provider(
        mut self,
        provider: Arc<dyn crate::opensubtitles::SubtitleProvider>,
    ) -> Self {
        self.provider = Some(provider);
        self
    }
    pub(crate) fn download_lock(&self, key: String) -> Arc<tokio::sync::Mutex<()>> {
        self.inflight
            .lock()
            .unwrap()
            .entry(format!("download:{key}"))
            .or_default()
            .clone()
    }

    /// Attach deployment-level provider config. Without it (tests, and
    /// any deployment that doesn't care) the built-in app key is used.
    pub fn with_provider_config(mut self, cfg: crate::opensubtitles::ProviderConfig) -> Self {
        self.provider_cfg = cfg;
        self
    }

    /// Build the external subtitle provider (HUB-21). The application
    /// key comes from config, else the admin page, else the key we
    /// ship; the optional account always comes from the admin page.
    pub(crate) async fn external_provider(
        &self,
        registry: &Registry,
        user_id: &str,
    ) -> Result<Arc<dyn crate::opensubtitles::SubtitleProvider>> {
        if let Some(provider) = &self.provider {
            return Ok(provider.clone());
        }
        // The application key is ours, overridable only by the config
        // file. The feature is always available.
        let key = if self.provider_cfg.api_key.is_empty() {
            crate::opensubtitles::default_api_key().to_string()
        } else {
            self.provider_cfg.api_key.clone()
        };
        // The account is this USER's, from the credential store: they spend
        // their own download entitlement. Without one they fall back to the
        // deployment-wide anonymous budget, and half an account is no account
        // — `token()` needs both halves before it will log in.
        //
        // A store that will not answer stops the search rather than quietly
        // downgrading it: an account that silently reverted to the shared
        // five-a-day budget looks like OpenSubtitles being stingy.
        let mut account = match registry.credentials() {
            Some(store) => {
                store
                    .get_provider(user_id, crate::opensubtitles::OPENSUBTITLES)
                    .await?
            }
            None => Default::default(),
        };
        let user = account.remove(crate::opensubtitles::USERNAME);
        let pass = account.remove(crate::opensubtitles::PASSWORD);
        Ok(Arc::new(crate::opensubtitles::OpenSubtitles::new(
            self.http.clone(),
            key,
            user,
            pass,
        )))
    }

    /// WebVTT for one subtitle key, cue timestamps shifted by `shift_ms`
    /// (players whose timeline starts mid-file pass a negative shift).
    pub async fn vtt(
        &self,
        registry: &Registry,
        sessions: &Sessions,
        track: &crate::tracks::Track,
        shift_ms: i64,
    ) -> Result<String> {
        let ex = self.load(registry, sessions, track).await?;
        Ok(to_vtt(&ex.cues, shift_ms))
    }

    /// HUB-32a: the script for a SIDECAR ASS track that is about to be
    /// burned. One small read through the same ladder every other
    /// subtitle takes, returning the file's own text — which is what
    /// `assrender` is fed through an appsrc. Embedded tracks never come
    /// this way: they burn from the demuxer's pad, the only path
    /// carrying the release's attached fonts.
    ///
    /// `None` rather than an error: a burn that cannot be materialised
    /// is a tier that has to be withdrawn, not a session that dies.
    pub async fn ass_for_burn(
        &self,
        registry: &Registry,
        sessions: &Sessions,
        track: &crate::tracks::Track,
    ) -> Option<String> {
        match self.load(registry, sessions, track).await {
            Ok(ex) => ex.ass,
            Err(e) => {
                tracing::warn!(
                    track = track.id,
                    error = format!("{e:#}"),
                    "ASS burn: sidecar script unreadable"
                );
                None
            }
        }
    }

    /// The faithful ASS script for an ASS/SSA subtitle (HUB-32) — the
    /// original sidecar bytes, or the reconstructed embedded track.
    /// Times are absolute file times; ASS renderers offset via the
    /// player clock, not the script.
    ///
    /// Embedded tracks not yet cached come back as a STREAM: header first,
    /// Dialogue lines as the demux pass reaches them (≈18× realtime), so
    /// the player renders subtitles seconds after the toggle instead of
    /// waiting out a full-file read. The full extraction still completes
    /// (and is cached) even if the client goes away.
    pub async fn ass_body(
        self: &Arc<Self>,
        registry: &Registry,
        sessions: &Sessions,
        track: &crate::tracks::Track,
    ) -> Result<AssBody> {
        if let Some(body) = &track.acquired {
            return Ok(AssBody::Full(
                body.ass.clone().context("subtitle has no ASS form")?,
            ));
        }
        let internal_key = track.internal_key();
        let key = internal_key.as_str();
        // Downloaded/OCR ASS serves from the stored body — a hole in
        // the old keyspace (only embedded/sidecar could serve .ass).
        if key.starts_with('d') {
            let ex = self.load(registry, sessions, track).await?;
            return Ok(AssBody::Full(ex.ass.context("subtitle has no ASS form")?));
        }
        let FileSource {
            module_id,
            collection_id,
            root_token,
            path_rel,
            size,
            info,
            revision,
            ..
        } = track_source(registry, track).await?;
        let entry = entries(&info)
            .into_iter()
            .find(|e| e.key == key)
            .with_context(|| format!("no subtitle {key} on this item"))?;
        anyhow::ensure!(
            matches!(entry.format.as_str(), "ass" | "ssa"),
            "subtitle has no ASS form"
        );

        // Sidecars are one small read; no streaming needed.
        let Some(n) = key.strip_prefix('e') else {
            let ex = self.load(registry, sessions, track).await?;
            return Ok(AssBody::Full(ex.ass.context("subtitle has no ASS form")?));
        };
        let idx: usize = n.parse().context("bad embedded key")?;

        let cache_key = cache_key(
            &module_id,
            &collection_id,
            &root_token,
            &path_rel,
            key,
            &revision,
        );
        let lock = {
            let mut map = self.inflight.lock().unwrap();
            map.entry(cache_key.clone()).or_default().clone()
        };
        let guard = lock.lock_owned().await;
        let cache_path = self.dir.join(format!("{cache_key}.json"));
        if let Ok(bytes) = std::fs::read(&cache_path) {
            let ex: Extracted = serde_json::from_slice(&bytes)?;
            return Ok(AssBody::Full(ex.ass.context("subtitle has no ASS form")?));
        }

        if let Some(ex) = self
            .request_extraction(
                registry,
                &module_id,
                &collection_id,
                &root_token,
                &path_rel,
                key,
                &revision,
            )
            .await
        {
            return Ok(AssBody::Full(ex.ass.context("subtitle has no ASS form")?));
        }
        let lease = sessions
            .open_lease(
                registry,
                &module_id,
                &collection_id,
                &root_token,
                &path_rel,
                crate::sessions::Reader::Viewer,
            )
            .await?;
        let source = crate::sessions::LeaseSource {
            lease,
            size,
            handle: tokio::runtime::Handle::current(),
            reads: 0,
        };
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(256);
        let this = self.clone();
        let (module_id2, collection_id2, root_token2, path_rel2) = (
            module_id.clone(),
            collection_id.clone(),
            root_token.clone(),
            path_rel.clone(),
        );
        tokio::spawn(async move {
            let (module_id, collection_id, root_token, path_rel) =
                (module_id2, collection_id2, root_token2, path_rel2);
            let _guard = guard; // held until the cache is written
            let extraction = tokio::task::spawn_blocking(move || {
                kahawai_media::subtitles::extract_embedded_stream(
                    Box::new(source),
                    idx,
                    // A gone client never stops the pass: the read is
                    // already paid for, the cache makes it count.
                    |ev| match ev {
                        kahawai_media::subtitles::SubStreamEvent::Header(h)
                        | kahawai_media::subtitles::SubStreamEvent::Dialogue(h) => {
                            let _ = tx.blocking_send(h);
                        }
                    },
                )
            })
            .await;
            match extraction {
                Ok(Ok(tracks)) => {
                    // One pass extracted EVERY text track: cache them all.
                    for (i, ex) in &tracks {
                        if let Err(e) = this.store_extracted(
                            &module_id,
                            &collection_id,
                            &root_token,
                            &path_rel,
                            &format!("e{i}"),
                            &revision,
                            ex,
                        ) {
                            tracing::warn!(error = format!("{e:#}"), "subtitle cache write failed");
                        }
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        error = format!("{e:#}"),
                        "streamed subtitle extraction failed"
                    )
                }
                Err(e) => tracing::warn!(error = %e, "subtitle extraction task panicked"),
            }
        });
        Ok(AssBody::Stream(rx))
    }

    async fn load(
        &self,
        registry: &Registry,
        sessions: &Sessions,
        track: &crate::tracks::Track,
    ) -> Result<Extracted> {
        if let Some(body) = &track.acquired {
            return Ok((**body).clone());
        }
        let internal_key = track.internal_key();
        let key = internal_key.as_str();
        let FileSource {
            module_id,
            collection_id,
            root_token,
            path_rel,
            size,
            info,
            revision,
            sidecar_revision,
        } = track_source(registry, track).await?;
        entries(&info)
            .into_iter()
            .find(|e| e.key == key)
            .with_context(|| format!("no subtitle {key} on this item"))?;

        // Cache parsed cues and the optional faithful ASS script.
        let revision = if key.starts_with('s') {
            sidecar_revision
        } else {
            revision
        };
        let cache_key = cache_key(
            &module_id,
            &collection_id,
            &root_token,
            &path_rel,
            key,
            &revision,
        );
        let lock = {
            let mut map = self.inflight.lock().unwrap();
            map.entry(cache_key.clone()).or_default().clone()
        };
        let _guard = lock.lock().await;

        let cache_path = self.dir.join(format!("{cache_key}.json"));
        if let Ok(bytes) = std::fs::read(&cache_path) {
            return Ok(serde_json::from_slice(&bytes)?);
        }
        let ex: Extracted = if let Some(n) = key.strip_prefix('s') {
            let idx: usize = n.parse().context("bad sidecar key")?;
            let sidecar = info
                .external_subtitles
                .get(idx)
                .context("sidecar index out of range")?;
            let lease = sessions
                .open_lease(
                    registry,
                    &module_id,
                    &collection_id,
                    &root_token,
                    &sidecar.path_rel,
                    crate::sessions::Reader::Viewer,
                )
                .await?;
            let bytes = read_all(lease).await?;
            let text = decode_text(&bytes);
            let cues = parse(&sidecar.format, &text)?;
            let ass = matches!(sidecar.format.as_str(), "ass" | "ssa").then_some(text);
            Extracted { cues, ass }
        } else if let Some(n) = key.strip_prefix('e') {
            let idx: usize = n.parse().context("bad embedded key")?;
            if let Some(ex) = self
                .request_extraction(
                    registry,
                    &module_id,
                    &collection_id,
                    &root_token,
                    &path_rel,
                    key,
                    &revision,
                )
                .await
            {
                return Ok(ex);
            }
            let lease = sessions
                .open_lease(
                    registry,
                    &module_id,
                    &collection_id,
                    &root_token,
                    &path_rel,
                    crate::sessions::Reader::Viewer,
                )
                .await?;
            let source = crate::sessions::LeaseSource {
                lease,
                size,
                handle: tokio::runtime::Handle::current(),
                reads: 0,
            };
            // Last-resort lease pass: extract every text track in the one
            // read and cache them all — a second track request must never
            // pay a second full read.
            let tracks = tokio::task::spawn_blocking(move || {
                kahawai_media::subtitles::extract_embedded_all(Box::new(source))
            })
            .await??;
            let mut requested = None;
            for (i, ex) in tracks {
                if i == idx {
                    requested = Some(ex.clone());
                }
                self.store_extracted(
                    &module_id,
                    &collection_id,
                    &root_token,
                    &path_rel,
                    &format!("e{i}"),
                    &revision,
                    &ex,
                )?;
            }
            return requested.with_context(|| {
                format!("no cues extracted (track {idx} missing or not a text track)")
            });
        } else {
            bail!("bad subtitle key: {key}");
        };
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(&cache_path, serde_json::to_vec(&ex)?)?;
        Ok(ex)
    }

    // Fallbacks describe the assigned library item. Retained provider answers
    // for a previous assignment must not redirect subtitle searches.

    /// The coded size and frame rate to render at — the source's own,
    /// not the script's `PlayRes`. A script authored at 1280x720 for a
    /// 1080p release must rasterise at 1080p or the overlay lands at
    /// the wrong scale.
    async fn raster_geometry(
        &self,
        registry: &Registry,
        parent: &crate::tracks::Track,
    ) -> Result<(u32, u32, (u32, u32))> {
        let info = track_source(registry, parent).await?.info;
        let v = info.video.first().context("source has no video track")?;
        anyhow::ensure!(v.width > 0 && v.height > 0, "source video has no size");
        // Unknown frame rate: 24000/1001 is the anime default and the
        // rate only decides SAMPLING granularity, never the timestamps
        // written — a wrong guess costs a little precision on animated
        // effects, nothing else.
        let fps = v
            .fps
            .filter(|(n, d)| *n > 0 && *d > 0)
            .unwrap_or((24000, 1001));
        Ok((v.width, v.height, fps))
    }

    /// Where the mediahost extraction addresses a track's display sets:
    /// (module, collection, path to walk, index within it, language).
    /// Embedded tracks walk the media container; VobSub sidecars walk
    /// the .idx (the mediahost keys off the extension), addressed by
    /// the in-idx track id from the external_subtitles entry.
    pub(crate) async fn extract_ref(
        &self,
        registry: &Registry,
        track: &crate::tracks::Track,
    ) -> Result<(String, String, String, String, usize, Option<String>)> {
        anyhow::ensure!(
            crate::tracks::is_image_format(&track.format),
            "track {} is {}, not an image subtitle",
            track.id,
            track.format
        );
        let (module_id, collection_id, root_token, media_rel) = (
            track
                .module_id
                .clone()
                .context("hub-stored track has no source to extract")?,
            track.collection_id.clone().unwrap_or_default(),
            track.root_token.clone().unwrap_or_default(),
            track.source_path.clone().unwrap_or_default(),
        );
        let idx = track.stream_index.unwrap_or(0) as usize;
        match track.origin.as_str() {
            "embedded" => Ok((
                module_id,
                collection_id,
                root_token,
                media_rel,
                idx,
                track.language.clone(),
            )),
            "sidecar" => {
                let info = track_source(registry, track).await?.info;
                let ext = info
                    .external_subtitles
                    .get(idx)
                    .with_context(|| format!("sidecar entry {idx} vanished"))?;
                Ok((
                    module_id,
                    collection_id,
                    root_token,
                    ext.path_rel.clone(),
                    ext.track.unwrap_or(0) as usize,
                    ext.language.clone().or_else(|| track.language.clone()),
                ))
            }
            other => bail!("cannot OCR a track of origin {other}"),
        }
    }

    #[cfg(feature = "ocr")]
    async fn wait_for_next_ocr_round(
        registry: &Registry,
        events: &mut tokio::sync::broadcast::Receiver<crate::registry::RegistryEvent>,
        interval: std::time::Duration,
    ) -> OcrSweepWake {
        let deadline = tokio::time::sleep(interval);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return OcrSweepWake::Periodic,
                event = events.recv() => match event {
                    Ok(event) => {
                        if let Some(module_id) = Self::reconnected_mediahost(registry, event) {
                            return OcrSweepWake::MediahostReconnected(module_id);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        return OcrSweepWake::EventsLagged;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return OcrSweepWake::Periodic;
                    }
                }
            }
        }
    }

    #[cfg(feature = "ocr")]
    fn reconnected_mediahost(
        registry: &Registry,
        event: crate::registry::RegistryEvent,
    ) -> Option<String> {
        let crate::registry::RegistryEvent::Satellite {
            module_id,
            connected: true,
            ..
        } = event
        else {
            return None;
        };
        registry
            .snapshot()
            .into_iter()
            .any(|(id, state)| {
                id == module_id && state.connected && state.module_type == "mediahost"
            })
            .then_some(module_id)
    }

    /// Reconnects can arrive during a long library pass. Drain them between
    /// tracks so a track skipped earlier in the same pass is revisited without
    /// waiting for every remaining OCR job to finish first.
    #[cfg(feature = "ocr")]
    fn drain_ocr_reconnects(
        registry: &Registry,
        events: &mut tokio::sync::broadcast::Receiver<crate::registry::RegistryEvent>,
        failed: &mut OcrSweepFailures,
    ) -> bool {
        let mut retry_earlier_candidates = false;
        loop {
            match events.try_recv() {
                Ok(event) => {
                    if let Some(module_id) = Self::reconnected_mediahost(registry, event) {
                        failed.host_reconnected(&module_id);
                        retry_earlier_candidates = true;
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                    failed.reconsider_connected(registry);
                    retry_earlier_candidates = true;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                | Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }
        retry_earlier_candidates
    }

    /// Keep the text-subtitle cache warm, so a first play does not pay for
    /// extraction with the viewer watching a spinner.
    ///
    /// Extraction is a whole-container walk: a live host logs
    /// `tracks=38 elapsed=21.3s` for one episode, and every second of that
    /// used to land on the first person to press Play. Nothing refilled the
    /// cache in the background after the mediadb rewrite removed
    /// `push_subs_worklist` along with the protocol-3 scan handler its two
    /// call sites lived in — the mediahost end survived intact and simply
    /// stopped being asked.
    ///
    /// A `SubsWorklist`, not the `ExtractSubs` the urgent path sends: the
    /// mediahost queues those at `Priority::SubtitlePrewarm`, below every
    /// other background job and interruptible by demand. Sending
    /// `ExtractSubs` here would file idle work as urgent and let a sweep
    /// outrank a viewer.
    ///
    /// That also means no idle gate on this side, unlike the OCR sweep
    /// below: OCR burns hub CPU, so it waits for playback to stop, while
    /// this only publishes work that a scheduler elsewhere already ranks.
    pub fn spawn_text_prewarm(self: &Arc<Self>, registry: Arc<Registry>) {
        let subs = self.clone();
        tokio::spawn(async move {
            // Let links and reconnect scans settle first, as the OCR sweep does.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            loop {
                let pending = match subs.catalogue_text_prewarm(&registry).await {
                    Ok(p) => p,
                    Err(error) => {
                        tracing::warn!(%error, "could not read subtitle prewarm work from mediadb");
                        vec![]
                    }
                };
                // Group per (mediahost, collection): a worklist names one
                // collection, and the mediahost dedupes what it already holds.
                let mut by_collection: std::collections::BTreeMap<
                    (String, String),
                    Vec<kahawai_proto::v1::SourcePath>,
                > = Default::default();
                for (module_id, collection_id, source) in pending {
                    by_collection
                        .entry((module_id, collection_id))
                        .or_default()
                        .push(source);
                }
                let (mut sent, mut skipped) = (0usize, 0usize);
                for ((module_id, collection_id), sources) in by_collection {
                    if !registry.is_connected(&module_id) {
                        skipped += sources.len();
                        continue; // not a failure — the next round retries
                    }
                    sent += sources.len();
                    tracing::info!(%module_id, collection = %collection_id, files = sources.len(),
                        "sending subtitle prewarm worklist");
                    // Chunked like the worklist this replaces: one message
                    // naming every cold file in a large collection is a
                    // needlessly large frame.
                    for chunk in sources.chunks(5000) {
                        let msg = kahawai_proto::v1::HubToHost {
                            msg: Some(kahawai_proto::v1::hub_to_host::Msg::SubsWorklist(
                                kahawai_proto::v1::SubsWorklist {
                                    collection_id: collection_id.clone(),
                                    sources: chunk.to_vec(),
                                },
                            )),
                        };
                        if let Err(error) = registry.send_to_host(&module_id, msg).await {
                            tracing::warn!(%module_id, error = format!("{error:#}"),
                                "subtitle prewarm worklist send failed");
                            break;
                        }
                    }
                }
                // Logged even when there is nothing to do, mirroring the OCR
                // sweep: a quiet cache and a sweep that never ran read the
                // same way in a log otherwise.
                tracing::info!(sent, skipped, "subtitle prewarm round complete");
                tokio::time::sleep(TEXT_PREWARM_ROUND).await;
            }
        });
    }

    /// HUB-32c idle sweep: OCR each physical image subtitle in mediadb
    /// that lacks a cached answer, one at a time, only while nothing is
    /// playing. Retaining the answer avoids repeating extraction and OCR
    /// or making playback wait for them. Reconnects retry only work
    /// blocked on that mediahost.
    #[cfg(feature = "ocr")]
    pub fn spawn_ocr_sweep(
        self: &Arc<Self>,
        registry: Arc<Registry>,
        sessions: Arc<crate::sessions::Sessions>,
    ) {
        let subs = self.clone();
        // Subscribe before spawning so a reconnect racing task startup remains
        // queued for the first post-settle retry decision.
        let mut events = registry.subscribe_events();
        tokio::spawn(async move {
            // Let links and reconnect scans settle first.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            // Tracks that failed stay failed for this hub run — a
            // corrupt track must not become a 15-second crash loop.
            let mut failed = OcrSweepFailures::default();
            loop {
                Self::drain_ocr_reconnects(&registry, &mut events, &mut failed);
                let candidates = match subs.catalogue_ocr_candidates(&registry).await {
                    Ok(tracks) => tracks,
                    Err(error) => {
                        tracing::warn!(%error, "could not read OCR work from mediadb");
                        vec![]
                    }
                };
                let mut generated = 0usize;
                let mut retry_earlier_candidates = false;
                for track in candidates {
                    let id = track.artifact_key.clone().expect("catalogue OCR identity");
                    if Self::drain_ocr_reconnects(&registry, &mut events, &mut failed) {
                        retry_earlier_candidates = true;
                        break;
                    }
                    if failed.contains(&id) {
                        continue;
                    }
                    // Idle means idle: playback outranks the sweep.
                    while !sessions.list().is_empty() {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    }
                    let Ok((
                        module_id,
                        collection_id,
                        root_token,
                        extract_rel,
                        extract_idx,
                        language,
                    )) = subs.extract_ref(&registry, &track).await
                    else {
                        failed.remember_permanent(&id);
                        continue;
                    };
                    if !registry.is_connected(&module_id) {
                        continue; // not a failure — retry when it returns
                    }
                    // No Tesseract model for this track's language: OCR
                    // is off the table, but the display sets are still
                    // warmed into the cache — a later burn-in session
                    // start then reads a file instead of waiting out
                    // the mediahost walk.
                    if crate::ocr::model_for(language.as_deref()).is_none() {
                        let _ = subs
                            .image_sets(
                                &registry,
                                &module_id,
                                &collection_id,
                                &root_token,
                                &extract_rel,
                                extract_idx,
                                track.source_revision().expect("captured source"),
                                SETS_WAIT_IDLE,
                            )
                            .await;
                        failed.remember_permanent(&id);
                        continue;
                    }
                    match subs.catalogue_ocr(&registry, &track).await {
                        Ok(OcrGeneration::Generated) => generated += 1,
                        // No text is an answer and it is now recorded;
                        // the next candidates query no longer offers it.
                        Ok(OcrGeneration::NoText) => {}
                        Ok(OcrGeneration::RetryOnReconnect { module_id }) => {
                            tracing::info!(track = id, item = %track.item_id, %module_id,
                                "idle OCR paused until mediahost reconnects");
                            failed.remember_until_reconnect(&id, module_id);
                        }
                        Err(e) => {
                            tracing::warn!(track = id, item = %track.item_id,
                                error = format!("{e:#}"), "idle OCR failed; skipping this run");
                            failed.remember_permanent(&id);
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                }
                if generated > 0 {
                    tracing::info!(generated, "idle OCR sweep round complete");
                }
                if retry_earlier_candidates {
                    continue;
                }
                match Self::wait_for_next_ocr_round(&registry, &mut events, OCR_SWEEP_INTERVAL)
                    .await
                {
                    OcrSweepWake::MediahostReconnected(module_id) => {
                        failed.host_reconnected(&module_id);
                        tracing::info!(%module_id, "mediahost reconnect woke idle OCR sweep");
                    }
                    // A periodic pass is also the lost-event fallback. Lagging
                    // means at least one state hint was dropped, so reconcile
                    // against authoritative connection state immediately.
                    OcrSweepWake::Periodic | OcrSweepWake::EventsLagged => {
                        failed.reconsider_connected(&registry);
                    }
                }
            }
        });
    }

    /// Ingest a mediahost-extracted track into the cache (ladder step 2).
    #[allow(clippy::too_many_arguments)] // exact source, stream and revision plus payload
    pub fn store_extracted(
        &self,
        module_id: &str,
        collection_id: &str,
        root_token: &str,
        path_rel: &str,
        key: &str,
        revision: &str,
        ex: &Extracted,
    ) -> Result<()> {
        if revision.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!(
            "{}.json",
            cache_key(
                module_id,
                collection_id,
                root_token,
                path_rel,
                key,
                revision
            )
        ));
        std::fs::write(&path, serde_json::to_vec(ex)?)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // exact source/track identity plus wait policy
    pub async fn image_sets(
        &self,
        registry: &Registry,
        module_id: &str,
        collection_id: &str,
        root_token: &str,
        path_rel: &str,
        sub_index: usize,
        revision: &str,
        wait: std::time::Duration,
    ) -> Option<std::path::PathBuf> {
        match self
            .image_sets_state(
                registry,
                module_id,
                collection_id,
                root_token,
                path_rel,
                sub_index,
                revision,
                wait,
            )
            .await
        {
            ImageSetsState::Ready(path) => Some(path),
            ImageSetsState::RetryOnReconnect | ImageSetsState::Unavailable => None,
        }
    }

    #[allow(clippy::too_many_arguments)] // exact source/track identity plus wait policy
    async fn image_sets_state(
        &self,
        registry: &Registry,
        module_id: &str,
        collection_id: &str,
        root_token: &str,
        path_rel: &str,
        sub_index: usize,
        revision: &str,
        wait: std::time::Duration,
    ) -> ImageSetsState {
        let key = format!("i{sub_index}");
        let cache_path = self.dir.join(format!(
            "{}.sets",
            cache_key(
                module_id,
                collection_id,
                root_token,
                path_rel,
                &key,
                revision
            )
        ));
        if tokio::fs::metadata(&cache_path).await.is_ok() {
            return ImageSetsState::Ready(cache_path);
        }
        if !registry.is_connected(module_id) {
            return ImageSetsState::RetryOnReconnect;
        }
        let msg = kahawai_proto::v1::HubToHost {
            msg: Some(kahawai_proto::v1::hub_to_host::Msg::ExtractImageSubs(
                kahawai_proto::v1::ExtractImageSubs {
                    collection_id: collection_id.to_string(),
                    source: Some(kahawai_proto::v1::SourcePath {
                        root_token: root_token.to_string(),
                        path_rel: path_rel.to_string(),
                    }),
                    sub_index: sub_index as u32,
                    source_revision: revision.into(),
                },
            )),
        };
        let Some(link) = registry.host_link(module_id) else {
            return ImageSetsState::RetryOnReconnect;
        };
        if !link.supports_revisioned_subtitles() {
            return ImageSetsState::Unavailable;
        }
        let link_generation = link.generation();
        if link.send(msg).await.is_err() {
            return ImageSetsState::RetryOnReconnect;
        }
        tracing::info!(collection = %collection_id, path = %path_rel, track = sub_index,
            "image display sets requested from mediahost");
        // A viewer is waiting on this one: bounded, unlike the text
        // extraction's patient 10 minutes.
        let deadline = std::time::Instant::now() + wait;
        while std::time::Instant::now() < deadline {
            if tokio::fs::metadata(&cache_path).await.is_ok() {
                return ImageSetsState::Ready(cache_path);
            }
            if !registry.host_link_is_current(module_id, link_generation) {
                return ImageSetsState::RetryOnReconnect;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        tracing::warn!(collection = %collection_id, path = %path_rel, track = sub_index,
            "image display sets did not arrive in time");
        ImageSetsState::Unavailable
    }

    /// Store what the mediahost walked, in the worker's own format.
    pub async fn store_image_sets(
        &self,
        module_id: &str,
        msg: &kahawai_proto::v1::ImageSubtitles,
    ) -> Result<()> {
        if msg.source_revision.is_empty() {
            return Ok(());
        }
        let key = format!("i{}", msg.sub_index);
        let source = msg
            .source
            .as_ref()
            .context("ImageSubtitles missing source")?;
        let path = self.dir.join(format!(
            "{}.sets",
            cache_key(
                module_id,
                &msg.collection_id,
                &source.root_token,
                &source.path_rel,
                &key,
                &msg.source_revision,
            )
        ));
        let blocks: Vec<(u64, Option<u64>, Vec<u8>)> = msg
            .blocks
            .iter()
            .map(|b| {
                (
                    b.start_ms,
                    (b.duration_ms > 0).then_some(b.duration_ms),
                    b.payload.clone(),
                )
            })
            .collect();
        let bytes = kahawai_media::burnin::encode_sets_zstd(
            &msg.codec,
            (!msg.codec_private.is_empty()).then_some(&msg.codec_private[..]),
            &blocks,
        );
        tokio::fs::create_dir_all(&self.dir).await.ok();
        let tmp = path.with_extension("sets.tmp");
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // exact source, stream and captured revision
    async fn request_extraction(
        &self,
        registry: &Registry,
        module_id: &str,
        collection_id: &str,
        root_token: &str,
        path_rel: &str,
        key: &str,
        revision: &str,
    ) -> Option<Extracted> {
        let link = registry.host_link(module_id)?;
        if !link.supports_revisioned_subtitles() {
            return None;
        }
        let msg = kahawai_proto::v1::HubToHost {
            msg: Some(kahawai_proto::v1::hub_to_host::Msg::ExtractSubs(
                kahawai_proto::v1::ExtractSubs {
                    source_revision: revision.into(),
                    collection_id: collection_id.to_string(),
                    source: Some(kahawai_proto::v1::SourcePath {
                        root_token: root_token.to_string(),
                        path_rel: path_rel.to_string(),
                    }),
                },
            )),
        };
        let generation = link.generation();
        link.send(msg).await.ok()?;
        tracing::info!(collection = %collection_id, path = %path_rel,
            "urgent subtitle extraction requested from mediahost");
        let cache_path = self.dir.join(format!(
            "{}.json",
            cache_key(
                module_id,
                collection_id,
                root_token,
                path_rel,
                key,
                revision
            )
        ));
        // The mediahost is never slower than dragging the file over the
        // lease ourselves — wait while its link is alive (10 min sanity
        // cap); the lease fallback is for disconnects, not slowness.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
        while std::time::Instant::now() < deadline {
            if let Ok(bytes) = tokio::fs::read(&cache_path).await {
                return serde_json::from_slice(&bytes).ok();
            }
            if !registry.host_link_is_current(module_id, generation) {
                tracing::warn!(path = %path_rel, "mediahost gone mid-extraction; falling back to lease");
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
        tracing::warn!(path = %path_rel, "mediahost extraction timed out; falling back to lease");
        None
    }

    pub async fn fonts_for_source(
        &self,
        registry: &Registry,
        sessions: &Sessions,
        source: FileSource,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        let FileSource {
            module_id,
            collection_id,
            root_token,
            path_rel,
            size,
            info,
            ..
        } = source;
        let cache_key = format!(
            "fonts-{:016x}",
            xxhash_rust::xxh3::xxh3_64(
                format!("{module_id}\n{collection_id}\n{root_token}\n{path_rel}").as_bytes()
            )
        );
        let lock = {
            let mut map = self.inflight.lock().unwrap();
            map.entry(cache_key.clone()).or_default().clone()
        };
        let _guard = lock.lock().await;
        let dir = self.dir.join(&cache_key);
        let legacy_dir = self.dir.join(format!(
            "fonts-{:016x}",
            xxhash_rust::xxh3::xxh3_64(
                format!("{module_id}\n{collection_id}\n{path_rel}").as_bytes()
            )
        ));
        if registry
            .collection_root_count(&module_id, &collection_id)
            .await
            .unwrap_or(0)
            == 1
        {
            let _ = promote_legacy_cache(&dir, &legacy_dir);
        }
        let index = dir.join("index.json");
        if let Ok(bytes) = std::fs::read(&index) {
            let names: Vec<String> = serde_json::from_slice(&bytes)?;
            let mut out = Vec::new();
            for (i, name) in names.iter().enumerate() {
                out.push((name.clone(), std::fs::read(dir.join(i.to_string()))?));
            }
            return Ok(out);
        }
        // HUB-34 fonts rung. Declarations (MH-4) are authoritative when
        // present: fonts among them → exact ranged lease reads (no
        // demux); none among them → empty, instantly. Only records that
        // were never declared fall back to the gst walk over a lease.
        let fonts = match &info.attachments {
            Some(atts) => {
                let declared: Vec<kahawai_core::media::Attachment> =
                    atts.iter().filter(|a| is_font(a)).cloned().collect();
                if declared.is_empty() {
                    Vec::new()
                } else {
                    use tokio_stream::StreamExt;
                    let lease = sessions
                        .open_lease(
                            registry,
                            &module_id,
                            &collection_id,
                            &root_token,
                            &path_rel,
                            crate::sessions::Reader::Viewer,
                        )
                        .await?;
                    let mut out = Vec::with_capacity(declared.len());
                    for a in declared {
                        let mut stream = lease.read_range(a.offset, a.size);
                        let mut buf = Vec::with_capacity(a.size as usize);
                        while let Some(chunk) = stream.next().await {
                            buf.extend_from_slice(&chunk?);
                        }
                        anyhow::ensure!(
                            buf.len() as u64 == a.size,
                            "short read for declared attachment {}",
                            a.file_name
                        );
                        out.push((a.file_name, buf));
                    }
                    tracing::info!(
                        source = path_rel,
                        fonts = out.len(),
                        "fonts read from declared ranges"
                    );
                    out
                }
            }
            None => {
                let lease = sessions
                    .open_lease(
                        registry,
                        &module_id,
                        &collection_id,
                        &root_token,
                        &path_rel,
                        crate::sessions::Reader::Viewer,
                    )
                    .await?;
                let source = crate::sessions::LeaseSource {
                    lease,
                    size,
                    handle: tokio::runtime::Handle::current(),
                    reads: 0,
                };
                tokio::task::spawn_blocking(move || {
                    kahawai_media::subtitles::extract_fonts(Box::new(source))
                })
                .await??
            }
        };
        std::fs::create_dir_all(&dir)?;
        for (i, (_, bytes)) in fonts.iter().enumerate() {
            std::fs::write(dir.join(i.to_string()), bytes)?;
        }
        let names: Vec<&String> = fonts.iter().map(|(n, _)| n).collect();
        std::fs::write(&index, serde_json::to_vec(&names)?)?;
        Ok(fonts)
    }
}

/// Font-shaped attachment: matroska muxers tag fonts inconsistently
/// (font/ttf, application/x-truetype-font, vnd.ms-opentype, …), so
/// match mime loosely and fall back to the extension.
fn is_font(a: &kahawai_core::media::Attachment) -> bool {
    let m = a.mime_type.to_ascii_lowercase();
    let n = a.file_name.to_ascii_lowercase();
    m.contains("font")
        || m.contains("truetype")
        || m.contains("opentype")
        || n.ends_with(".ttf")
        || n.ends_with(".otf")
        || n.ends_with(".ttc")
}

/// [`ocr_stream_set`] as `negotiate()`'s per-stream flag vec, for the
/// source one plan is judging.
pub fn ocr_flags_for(
    set: &std::collections::HashSet<(String, String, String, String, i64)>,
    module_id: &str,
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    n_subs: usize,
) -> Vec<bool> {
    (0..n_subs)
        .map(|i| {
            set.contains(&(
                module_id.to_string(),
                collection_id.to_string(),
                root_token.to_string(),
                path_rel.to_string(),
                i as i64,
            ))
        })
        .collect()
}

fn entries(info: &kahawai_core::media::MediaInfo) -> Vec<SubtitleEntry> {
    let mut out = Vec::new();
    for (i, s) in info.subtitles.iter().enumerate() {
        let image = matches!(s.format.as_str(), "pgs" | "vobsub" | "dvdsub");
        if is_text_format(&s.format) || image {
            out.push(SubtitleEntry {
                key: format!("e{i}"),
                kind: "embedded",
                format: s.format.clone(),
                language: s.language.clone(),
                flattened: matches!(s.format.as_str(), "ass" | "ssa"),
                image,
            });
        }
    }
    for (i, s) in info.external_subtitles.iter().enumerate() {
        out.push(SubtitleEntry {
            key: format!("s{i}"),
            kind: "sidecar",
            format: s.format.clone(),
            language: s.language.clone(),
            flattened: s.format == "ass",
            // An .idx/.sub pair: image subtitles with no VTT form. No
            // session tap exists for a sidecar, so their serving path
            // is the OCR text tier.
            image: s.format == "vobsub",
        });
    }
    out
}

/// Metadata and byte reads must use the physical file named by the track.
/// A stream index such as e0 is only meaningful within that file; selecting a
/// collection's default source here substitutes another release's timestamps.
#[derive(Debug, Clone)]
pub struct FileSource {
    pub module_id: String,
    pub collection_id: String,
    pub root_token: String,
    pub path_rel: String,
    pub size: u64,
    pub revision: String,
    pub sidecar_revision: String,
    pub info: kahawai_core::media::MediaInfo,
}

async fn track_source(_registry: &Registry, track: &crate::tracks::Track) -> Result<FileSource> {
    track
        .physical
        .clone()
        .context("subtitle track has no captured physical source")
}

/// Drain a whole (small) file through a lease in chunks.
async fn read_all(lease: crate::leases::Lease) -> Result<Vec<u8>> {
    const CHUNK: u64 = 1 << 20;
    const MAX: usize = 16 << 20; // sidecars are text; 16 MiB is generous
    let mut out = Vec::new();
    loop {
        let mut stream = lease.read_range(out.len() as u64, CHUNK).into_inner();
        let mut got = 0u64;
        while let Some(chunk) = stream.recv().await {
            let bytes = chunk.map_err(|e| anyhow::anyhow!("lease read: {e}"))?;
            got += bytes.len() as u64;
            out.extend_from_slice(&bytes);
            if out.len() > MAX {
                bail!("subtitle file too large");
            }
        }
        if got < CHUNK {
            return Ok(out);
        }
    }
}

#[cfg(all(test, feature = "ocr"))]
mod ocr_memory_tests {

    #[test]
    fn reconnect_releases_only_failures_owned_by_that_mediahost() {
        let mut failed = super::OcrSweepFailures::default();
        failed.remember_permanent(1);
        failed.remember_until_reconnect(2, "host-a".into());
        failed.remember_until_reconnect(3, "host-b".into());

        failed.host_reconnected("host-a");

        assert!(failed.contains(1), "a corrupt track remains suppressed");
        assert!(!failed.contains(2), "the returning host's track is retried");
        assert!(
            failed.contains(3),
            "another offline host remains suppressed"
        );
    }

    #[tokio::test]
    async fn mediahost_reconnect_wakes_the_ocr_sweep() {
        let db = crate::db::open_in_memory().await.unwrap();
        let registry = crate::registry::Registry::new(
            db,
            Default::default(),
            kahawai_mediadb::Store::in_memory().await.unwrap(),
        );
        let mut events = registry.subscribe_events();
        registry.connected("tc", "transcoder", "encoder", "fp-tc", "test");
        registry.connected("mh", "mediahost", "storage", "fp-mh", "test");

        let wake = super::Subtitles::wait_for_next_ocr_round(
            &registry,
            &mut events,
            std::time::Duration::from_secs(1),
        )
        .await;

        assert_eq!(
            wake,
            super::OcrSweepWake::MediahostReconnected("mh".into()),
            "transcoder events must not wake mediahost-dependent OCR work"
        );
    }

    #[tokio::test]
    async fn stale_connected_state_with_no_link_is_retryable() {
        let db = crate::db::open_in_memory().await.unwrap();
        let registry = crate::registry::Registry::new(
            db,
            Default::default(),
            kahawai_mediadb::Store::in_memory().await.unwrap(),
        );
        registry.connected("mh", "mediahost", "storage", "fp", "test");
        let subs = super::Subtitles::new(tempfile::tempdir().unwrap().keep());

        let state = subs
            .image_sets_state(
                &registry,
                "mh",
                "series",
                "root",
                "episode.mkv",
                0,
                "revision",
                std::time::Duration::from_secs(1),
            )
            .await;

        assert!(matches!(state, super::ImageSetsState::RetryOnReconnect));
    }

    #[tokio::test]
    async fn replacing_the_extraction_link_makes_the_wait_retryable() {
        let db = crate::db::open_in_memory().await.unwrap();
        let registry = crate::registry::Registry::new(
            db,
            Default::default(),
            kahawai_mediadb::Store::in_memory().await.unwrap(),
        );
        let (old_tx, mut old_rx) = tokio::sync::mpsc::channel(1);
        registry.register_link("mh", old_tx, kahawai_proto::PROTOCOL_MINOR, 0);
        registry.connected("mh", "mediahost", "storage", "fp", "test");
        let subs = super::Subtitles::new(tempfile::tempdir().unwrap().keep());
        let wait = subs.image_sets_state(
            &registry,
            "mh",
            "series",
            "root",
            "episode.mkv",
            0,
            "revision",
            std::time::Duration::from_secs(2),
        );
        tokio::pin!(wait);

        tokio::select! {
            request = old_rx.recv() => assert!(request.is_some(), "extraction was requested"),
            state = &mut wait => panic!("wait ended before link replacement: {}", matches!(state, super::ImageSetsState::RetryOnReconnect)),
        }
        let (new_tx, _new_rx) = tokio::sync::mpsc::channel(1);
        registry.register_link("mh", new_tx, kahawai_proto::PROTOCOL_MINOR, 0);
        registry.connected("mh", "mediahost", "storage", "fp", "test");

        assert!(matches!(
            wait.await,
            super::ImageSetsState::RetryOnReconnect
        ));
    }
}

#[cfg(test)]
mod account_tests {
    use super::*;
    use std::collections::BTreeMap;

    /// `per_account` is the observable end of the account: `OpenSubtitles::new`
    /// sets it only when both halves arrived, and it is what tells a viewer
    /// whose budget a search is spending.
    async fn spends_own_budget(fields: Option<BTreeMap<&str, &str>>) -> bool {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open_in_memory().await.unwrap();
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ('u1','one','x')")
            .execute(&db)
            .await
            .unwrap();
        let credentials = std::sync::Arc::new(
            crate::secrets::Credentials::open(dir.path(), db.clone())
                .await
                .unwrap(),
        );
        if let Some(fields) = fields {
            credentials
                .set_provider("u1", crate::opensubtitles::OPENSUBTITLES, &fields)
                .await
                .unwrap();
        }
        let registry = Registry::new(
            db,
            Default::default(),
            kahawai_mediadb::Store::in_memory().await.unwrap(),
        )
        .with_credentials(credentials);
        let subtitles = Subtitles::new(tempfile::tempdir().unwrap().keep());
        subtitles
            .external_provider(&registry, "u1")
            .await
            .unwrap()
            .quota()
            .per_account
    }

    #[tokio::test]
    async fn an_account_in_the_store_is_the_one_the_search_spends() {
        assert!(
            spends_own_budget(Some(BTreeMap::from([
                (crate::opensubtitles::USERNAME, "someone"),
                (crate::opensubtitles::PASSWORD, "a-secret"),
            ])))
            .await
        );
    }

    #[tokio::test]
    async fn without_one_the_search_falls_back_to_the_shared_budget() {
        assert!(!spends_own_budget(None).await);
        // Half an account cannot log in, so it is not an account. Adoption can
        // leave this shape behind, from a viewer who stored only a username.
        assert!(
            !spends_own_budget(Some(BTreeMap::from([(
                crate::opensubtitles::USERNAME,
                "someone"
            )])))
            .await
        );
    }

    /// The store answers errors as errors. A row that will not open used to be
    /// swallowed into "no account", which reads as OpenSubtitles being stingy
    /// rather than as something being wrong here.
    #[tokio::test]
    async fn an_unreadable_credential_stops_the_search() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open_in_memory().await.unwrap();
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ('u1','one','x')")
            .execute(&db)
            .await
            .unwrap();
        let credentials = std::sync::Arc::new(
            crate::secrets::Credentials::open(dir.path(), db.clone())
                .await
                .unwrap(),
        );
        credentials
            .set_provider(
                "u1",
                crate::opensubtitles::OPENSUBTITLES,
                &BTreeMap::from([
                    (crate::opensubtitles::USERNAME, "someone"),
                    (crate::opensubtitles::PASSWORD, "a-secret"),
                ]),
            )
            .await
            .unwrap();
        sqlx::query("UPDATE credentials SET secret = randomblob(length(secret))")
            .execute(&db)
            .await
            .unwrap();

        let registry = Registry::new(
            db,
            Default::default(),
            kahawai_mediadb::Store::in_memory().await.unwrap(),
        )
        .with_credentials(credentials);
        let subtitles = Subtitles::new(tempfile::tempdir().unwrap().keep());
        assert!(subtitles.external_provider(&registry, "u1").await.is_err());
    }
}

#[cfg(test)]
mod cache_upgrade_tests {
    use super::promote_legacy_cache;

    #[test]
    fn legacy_artifact_is_moved_to_its_exact_key() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("legacy.json");
        let exact = dir.path().join("exact.json");
        std::fs::write(&legacy, b"paid artifact").unwrap();

        promote_legacy_cache(&exact, &legacy).unwrap();

        assert_eq!(std::fs::read(&exact).unwrap(), b"paid artifact");
        assert!(!legacy.exists());
    }

    #[test]
    fn exact_artifact_is_never_replaced_by_a_legacy_one() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("legacy.json");
        let exact = dir.path().join("exact.json");
        std::fs::write(&legacy, b"old").unwrap();
        std::fs::write(&exact, b"exact").unwrap();

        promote_legacy_cache(&exact, &legacy).unwrap();

        assert_eq!(std::fs::read(&exact).unwrap(), b"exact");
        assert_eq!(std::fs::read(&legacy).unwrap(), b"old");
    }
}
