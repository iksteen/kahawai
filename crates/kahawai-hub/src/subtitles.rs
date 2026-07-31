//! Text subtitle serving (HUB-15/27): enumerate an item's embedded and
//! sidecar text subtitles, extract/convert them to WebVTT lazily, and
//! cache extracted cues on the hub (embedded extraction demuxes the whole
//! source once — never twice).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use kahawai_media::subtitles::{Extracted, decode_text, is_text_format, parse, to_vtt};
use serde::Serialize;
use sqlx::Row;

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
#[derive(serde::Serialize)]
pub struct TrackListing {
    #[serde(flatten)]
    pub track: crate::tracks::Track,
    pub delivery: crate::tracks::Delivery,
    pub note: &'static str,
}

/// A served ASS script: complete (cache/sidecar) or streamed while the
/// extraction pass runs.
pub enum AssBody {
    Full(String),
    Stream(tokio::sync::mpsc::Receiver<String>),
}

/// How long OCR generation waits for the mediahost's display-set walk.
/// Urgent (a human pressed the button): bounded like the burn path.
/// Idle (the sweep): nobody is waiting, and giving up early only wastes
/// the walk — the sets still arrive and get cached, but the track sits
/// in the failed set until the next hub run.
#[cfg(feature = "ocr")]
const SETS_WAIT_URGENT: std::time::Duration = std::time::Duration::from_secs(20);
#[cfg(feature = "ocr")]
const SETS_WAIT_IDLE: std::time::Duration = std::time::Duration::from_secs(180);

/// Cache key for one subtitle track of one source file — shared by the
/// lazy extractors and the mediahost ingestion path.
fn cache_key(module_id: &str, collection_id: &str, path_rel: &str, key: &str) -> String {
    format!(
        "v2-{:016x}-{key}",
        xxhash_rust::xxh3::xxh3_64(format!("{module_id}\n{collection_id}\n{path_rel}").as_bytes())
    )
}

pub struct Subtitles {
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
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            provider_cfg: Default::default(),
            inflight: Default::default(),
            http: Arc::new(crate::gate::Http::new().expect("http client")),
        }
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
    async fn external_provider(
        &self,
        registry: &Registry,
        user_id: &str,
    ) -> Result<Box<dyn crate::opensubtitles::SubtitleProvider>> {
        // The application key is ours, overridable only by the config
        // file. The feature is always available.
        let key = if self.provider_cfg.api_key.is_empty() {
            crate::opensubtitles::default_api_key().to_string()
        } else {
            self.provider_cfg.api_key.clone()
        };
        // The account is this USER's, from their own settings: they
        // spend their own download entitlement. Without one they fall
        // back to the deployment-wide anonymous budget.
        let pref = |key: &'static str| async move {
            sqlx::query_scalar::<_, String>(
                "SELECT value FROM user_prefs WHERE user_id = ? AND scope = '' AND key = ?",
            )
            .bind(user_id)
            .bind(key)
            .fetch_optional(registry.db())
            .await
            .ok()
            .flatten()
            .filter(|v| !v.is_empty())
        };
        let user = pref(crate::opensubtitles::USER_PREF_USERNAME).await;
        let pass = pref(crate::opensubtitles::USER_PREF_PASSWORD).await;
        Ok(Box::new(crate::opensubtitles::OpenSubtitles::new(
            self.http.clone(),
            key,
            user,
            pass,
        )))
    }

    /// Every track of the item, from the unified table — bound to the
    /// same source `open_source` would pick, plus the hub-stored rows.
    /// Capability adjusts each track's DELIVERY, never its existence
    /// (the UI disables `none`; the API always lists).
    pub async fn list(
        &self,
        registry: &Registry,
        item_id: &str,
        ass_render: bool,
        graphics_overlay: bool,
    ) -> Result<Vec<TrackListing>> {
        let (module_id, collection_id, path_rel, _info) = source_row(registry, item_id).await?;
        let tracks = crate::tracks::for_item_source(
            registry.db(),
            item_id,
            &module_id,
            &collection_id,
            &path_rel,
        )
        .await?;
        // Burn needs the display sets readable where the encode runs —
        // a connected mediahost extracts them (HUB-32b).
        let burn_capable = registry.is_connected(&module_id);
        Ok(tracks
            .into_iter()
            .map(|t| {
                let (delivery, note) =
                    crate::tracks::delivery(&t, ass_render, graphics_overlay, burn_capable);
                TrackListing {
                    track: t,
                    delivery,
                    note,
                }
            })
            .collect())
    }

    /// The internal key (`e{n}`/`s{n}`/`d{id}`) a track id resolves to —
    /// the notation the caches and the pipeline still speak. Public API
    /// surfaces speak track ids only.
    pub async fn internal_key(&self, registry: &Registry, id: i64) -> Result<crate::tracks::Track> {
        crate::tracks::get(registry.db(), id)
            .await?
            .with_context(|| format!("no subtitle track {id}"))
    }

    /// WebVTT for one subtitle key, cue timestamps shifted by `shift_ms`
    /// (players whose timeline starts mid-file pass a negative shift).
    pub async fn vtt(
        &self,
        registry: &Registry,
        sessions: &Sessions,
        item_id: &str,
        key: &str,
        shift_ms: i64,
    ) -> Result<String> {
        let ex = self.load(registry, sessions, item_id, key).await?;
        Ok(to_vtt(&ex.cues, shift_ms))
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
        item_id: &str,
        key: &str,
    ) -> Result<AssBody> {
        // Downloaded/OCR ASS serves from the stored body — a hole in
        // the old keyspace (only embedded/sidecar could serve .ass).
        if key.starts_with('d') {
            let ex = self.load(registry, sessions, item_id, key).await?;
            return Ok(AssBody::Full(ex.ass.context("subtitle has no ASS form")?));
        }
        let (module_id, collection_id, path_rel, info) = source_row(registry, item_id).await?;
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
            let ex = self.load(registry, sessions, item_id, key).await?;
            return Ok(AssBody::Full(ex.ass.context("subtitle has no ASS form")?));
        };
        let idx: usize = n.parse().context("bad embedded key")?;

        let cache_key = cache_key(&module_id, &collection_id, &path_rel, key);
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
            .request_extraction(registry, &module_id, &collection_id, &path_rel, key)
            .await
        {
            return Ok(AssBody::Full(ex.ass.context("subtitle has no ASS form")?));
        }
        let (_, _, size, _, lease) = sessions.open_source(registry, item_id).await?;
        let source = crate::sessions::LeaseSource {
            lease,
            size,
            handle: tokio::runtime::Handle::current(),
        };
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(256);
        let this = self.clone();
        let (module_id2, collection_id2, path_rel2) =
            (module_id.clone(), collection_id.clone(), path_rel.clone());
        tokio::spawn(async move {
            let (module_id, collection_id, path_rel) = (module_id2, collection_id2, path_rel2);
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
                            &path_rel,
                            &format!("e{i}"),
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
        item_id: &str,
        key: &str,
    ) -> Result<Extracted> {
        // HUB-24: downloaded subtitles live in the cache keyed by their
        // row id — independent of which source file the item resolves
        // to, so a re-scan or a second copy never orphans them.
        if let Some(id) = key.strip_prefix('d') {
            let id: i64 = id.parse().context("bad downloaded-subtitle key")?;
            let bytes = std::fs::read(self.downloaded_path(id))
                .context("downloaded subtitle missing from cache — download it again")?;
            return Ok(serde_json::from_slice(&bytes)?);
        }
        let (module_id, collection_id, path_rel, info) = source_row(registry, item_id).await?;
        entries(&info)
            .into_iter()
            .find(|e| e.key == key)
            .with_context(|| format!("no subtitle {key} on this item"))?;

        // v2: the cache holds cues + optional faithful ASS.
        let cache_key = cache_key(&module_id, &collection_id, &path_rel, key);
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
                .open_lease(registry, &module_id, &collection_id, &sidecar.path_rel)
                .await?;
            let bytes = read_all(lease).await?;
            let text = decode_text(&bytes);
            let cues = parse(&sidecar.format, &text)?;
            let ass = matches!(sidecar.format.as_str(), "ass" | "ssa").then_some(text);
            Extracted { cues, ass }
        } else if let Some(n) = key.strip_prefix('e') {
            let idx: usize = n.parse().context("bad embedded key")?;
            if let Some(ex) = self
                .request_extraction(registry, &module_id, &collection_id, &path_rel, key)
                .await
            {
                return Ok(ex);
            }
            let (_, _, size, _, lease) = sessions.open_source(registry, item_id).await?;
            let source = crate::sessions::LeaseSource {
                lease,
                size,
                handle: tokio::runtime::Handle::current(),
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
                self.store_extracted(&module_id, &collection_id, &path_rel, &format!("e{i}"), &ex)?;
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

    fn downloaded_path(&self, id: i64) -> PathBuf {
        self.dir.join(format!("downloaded-{id}.json"))
    }

    /// Search results plus the entitlement state the UI must show.
    /// HUB-21/22/24: search external providers for one item. Hash first
    /// (exact file), title/year as the fallback the provider needs when
    /// it doesn't know the hash.
    pub async fn search_external(
        &self,
        registry: &Registry,
        item_id: &str,
        languages: Vec<String>,
        user_id: &str,
    ) -> Result<(
        Vec<crate::opensubtitles::Candidate>,
        crate::opensubtitles::Quota,
    )> {
        let provider = self.external_provider(registry, user_id).await?;
        provider.refresh_quota().await;
        let row = sqlx::query(
            "SELECT i.kind, i.season, i.episode,
                    md.proj_season, md.proj_episode,
                    CASE WHEN COALESCE(pm.provider, md.provider) = 'tmdb'
                         THEN COALESCE(pm.provider_id, md.provider_id) END AS tmdb_provider_id,
                    COALESCE(pai.mapped_tmdb, ai.mapped_tmdb) AS mapped_tmdb,
                    COALESCE(pm.title, p.title, md.title, i.title) AS search_title,
                    COALESCE(i.year, p.year,
                             CAST(substr(COALESCE(md.premiered, pmd.premiered), 1, 4) AS INTEGER))
                        AS search_year
             FROM items i
             LEFT JOIN items p ON p.id = i.parent_id
             LEFT JOIN resolved_metadata md ON md.item_id = i.id
             LEFT JOIN resolved_metadata pmd ON pmd.item_id = i.parent_id
             LEFT JOIN resolved_metadata pm ON pm.item_id = i.parent_id
             LEFT JOIN anime_ids ai ON ai.item_id = i.id
             LEFT JOIN anime_ids pai ON pai.item_id = i.parent_id
             WHERE i.id = ?",
        )
        .bind(item_id)
        .fetch_optional(registry.db())
        .await?
        .context("no such item")?;

        // The mediahost's oshash IS the OpenSubtitles moviehash (HUB-22).
        let hash: Option<i64> = sqlx::query_scalar(
            "SELECT f.oshash FROM item_sources s
             JOIN files f ON (f.module_id, f.collection_id, f.path_rel)
                           = (s.module_id, s.collection_id, s.path_rel)
             WHERE s.item_id = ? ORDER BY f.size DESC LIMIT 1",
        )
        .bind(item_id)
        .fetch_optional(registry.db())
        .await?;

        // HUB-22, two phases: the hash is an EXACT file identifier, so
        // ask for it alone first (the API ANDs parameters — pairing it
        // with a title query returns nothing when the hash is unknown).
        // Only if that comes up empty do we fall back to title search.
        if let Some(h) = hash {
            let hits = provider
                .search(&crate::opensubtitles::SearchQuery {
                    moviehash: Some(h as u64),
                    tmdb_id: None,
                    imdb_id: None,
                    title: None,
                    year: None,
                    season: None,
                    episode: None,
                    languages: languages.clone(),
                })
                .await?;
            if !hits.is_empty() {
                return Ok((hits, provider.quota()));
            }
        }

        // For episodes OpenSubtitles wants season + episode;
        // absolute-numbered anime has no native season, so use the
        // HUB-31 projection — without it, "episode 11" alone matches
        // episode 11 of anything.
        let is_episode = row.get::<String, _>("kind") == "episode";
        let (mut season, mut episode) = (
            row.get::<Option<i64>, _>("season"),
            row.get::<Option<i64>, _>("episode"),
        );
        if is_episode && season.is_none() {
            season = row.get::<Option<i64>, _>("proj_season");
            episode = row.get::<Option<i64>, _>("proj_episode").or(episode);
        }
        if !is_episode {
            (season, episode) = (None, None);
        } else if season.is_none() {
            // Unprojected absolute numbering: an episode filter would be
            // meaningless, so search the series and let the user choose.
            episode = None;
        }
        // HUB-22 middle rung: enrichment's external ids beat a title
        // string — for episodes the show's TMDB id plus season/episode
        // is unambiguous where a title match is a guess.
        let tmdb_id: Option<i64> = row
            .get::<Option<String>, _>("tmdb_provider_id")
            .and_then(|s| s.parse().ok())
            .or_else(|| row.get::<Option<i64>, _>("mapped_tmdb"));
        if tmdb_id.is_some() {
            let hits = provider
                .search(&crate::opensubtitles::SearchQuery {
                    moviehash: None,
                    tmdb_id,
                    imdb_id: None,
                    title: None,
                    year: None,
                    season: if is_episode { season } else { None },
                    episode: if is_episode { episode } else { None },
                    languages: languages.clone(),
                })
                .await
                .unwrap_or_default();
            if !hits.is_empty() {
                return Ok((hits, provider.quota()));
            }
        }

        let q = crate::opensubtitles::SearchQuery {
            moviehash: None,
            tmdb_id: None,
            imdb_id: None,
            title: row.get::<Option<String>, _>("search_title"),
            // Year is the SHOW's start year for episodes, which the API
            // ANDs against the episode's air year — precise enough
            // without it once season+episode are set.
            year: (!is_episode)
                .then(|| row.get::<Option<i64>, _>("search_year"))
                .flatten(),
            season,
            episode,
            languages,
        };
        let hits = provider.search(&q).await?;
        Ok((hits, provider.quota()))
    }

    /// Download a chosen candidate, parse it, and register it for the
    /// item. Returns the new subtitle key ("d{id}").
    pub async fn download_external(
        &self,
        registry: &Registry,
        item_id: &str,
        file_id: &str,
        language: Option<String>,
        user_id: &str,
    ) -> Result<(i64, crate::opensubtitles::Quota)> {
        let provider = self.external_provider(registry, user_id).await?;
        let dl = provider.download(file_id).await?;
        let text = decode_text(&dl.bytes);
        let cues = parse(&dl.format, &text)?;
        anyhow::ensure!(!cues.is_empty(), "downloaded subtitle had no cues");
        let ex = Extracted {
            cues,
            ass: matches!(dl.format.as_str(), "ass" | "ssa").then_some(text),
        };
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO subtitle_tracks
               (item_id, origin, format, language, label, provider, created_by)
             VALUES (?, 'downloaded', ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(item_id)
        .bind(&dl.format)
        .bind(&language)
        .bind(&dl.release_name)
        .bind(provider.name())
        .bind(user_id)
        .fetch_one(registry.db())
        .await?;
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.downloaded_path(id), serde_json::to_vec(&ex)?)?;
        tracing::info!(item = item_id, id, format = %dl.format, "external subtitle downloaded");
        Ok((id, provider.quota()))
    }

    /// HUB-32c: OCR an image subtitle track (embedded or VobSub
    /// sidecar) into a new text track. The result is a first-class
    /// `ocr` row whose `derived_from` points at the image track — that
    /// linkage is what regeneration replaces by and what the sweep and
    /// negotiation dedupe on. Returns the new track id.
    #[cfg(feature = "ocr")]
    pub async fn ocr_generate(
        &self,
        registry: &Registry,
        parent_id: i64,
        user_id: &str,
    ) -> Result<i64> {
        // A human pressed the button: bounded like the burn path's wait.
        self.ocr_generate_within(registry, parent_id, user_id, SETS_WAIT_URGENT)
            .await
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
    ) -> Result<(String, String, String, usize, Option<String>)> {
        anyhow::ensure!(
            crate::tracks::is_image_format(&track.format),
            "track {} is {}, not an image subtitle",
            track.id,
            track.format
        );
        let (module_id, collection_id, media_rel) = (
            track
                .module_id
                .clone()
                .context("hub-stored track has no source to extract")?,
            track.collection_id.clone().unwrap_or_default(),
            track.path_rel.clone().unwrap_or_default(),
        );
        let idx = track.stream_index.unwrap_or(0) as usize;
        match track.origin.as_str() {
            "embedded" => Ok((
                module_id,
                collection_id,
                media_rel,
                idx,
                track.language.clone(),
            )),
            "sidecar" => {
                let streams: String = sqlx::query_scalar(
                    "SELECT streams_json FROM files
                     WHERE (module_id, collection_id, path_rel) = (?, ?, ?)",
                )
                .bind(&module_id)
                .bind(&collection_id)
                .bind(&media_rel)
                .fetch_one(registry.db())
                .await?;
                let info: kahawai_core::media::MediaInfo = serde_json::from_str(&streams)?;
                let ext = info
                    .external_subtitles
                    .get(idx)
                    .with_context(|| format!("sidecar entry {idx} vanished"))?;
                Ok((
                    module_id,
                    collection_id,
                    ext.path_rel.clone(),
                    ext.track.unwrap_or(0) as usize,
                    ext.language.clone().or_else(|| track.language.clone()),
                ))
            }
            other => bail!("cannot OCR a track of origin {other}"),
        }
    }

    #[cfg(feature = "ocr")]
    async fn ocr_generate_within(
        &self,
        registry: &Registry,
        parent_id: i64,
        user_id: &str,
        sets_wait: std::time::Duration,
    ) -> Result<i64> {
        // One generation per parent at a time: the idle sweep and the
        // button race here, and losing the race means the work is
        // already done — return the winner's row instead of redoing it.
        let lock = {
            let mut map = self.inflight.lock().unwrap();
            map.entry(format!("ocr:{parent_id}")).or_default().clone()
        };
        let _guard = lock.lock_owned().await;
        if let Some(id) = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM subtitle_tracks WHERE derived_from = ? AND origin = 'ocr'",
        )
        .bind(parent_id)
        .fetch_optional(registry.db())
        .await?
        {
            return Ok(id);
        }
        let parent = crate::tracks::get(registry.db(), parent_id)
            .await?
            .with_context(|| format!("no subtitle track {parent_id}"))?;
        let (module_id, collection_id, extract_rel, extract_idx, language) =
            self.extract_ref(registry, &parent).await?;
        let model = crate::ocr::model_for(language.as_deref()).with_context(|| {
            format!(
                "no Tesseract model for language {:?} — install its traineddata",
                language.as_deref().unwrap_or("(untagged)")
            )
        })?;
        // The display sets: cached from any earlier burn/overlay use, or
        // walked by the mediahost now. A viewer may be waiting (the
        // urgent case), so the wait is bounded like the burn path's.
        let sets = self
            .image_sets(
                registry,
                &module_id,
                &collection_id,
                &extract_rel,
                extract_idx,
                sets_wait,
            )
            .await
            .context("display sets unavailable (mediahost offline or unindexed track)")?;
        let cues = tokio::task::spawn_blocking({
            let sets = sets.clone();
            let model = model.clone();
            move || crate::ocr::ocr_sets_file(&sets, &model)
        })
        .await??;
        let n_cues = cues.len();
        let ex = Extracted { cues, ass: None };

        let id: i64 = sqlx::query_scalar(
            "INSERT INTO subtitle_tracks
               (item_id, origin, format, language, label, provider, machine,
                created_by, derived_from)
             VALUES (?, 'ocr', 'srt', ?, ?, 'ocr', 1, ?, ?) RETURNING id",
        )
        .bind(&parent.item_id)
        .bind(&language)
        .bind(&model)
        .bind(user_id)
        .bind(parent_id)
        .fetch_one(registry.db())
        .await?;
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.downloaded_path(id), serde_json::to_vec(&ex)?)?;
        tracing::info!(item = %parent.item_id, parent = parent_id, track = id,
            %model, cues = n_cues, "image subtitle OCRed to text");
        Ok(id)
    }

    /// HUB-32c idle sweep: OCR every image subtitle track in the library
    /// that lacks a text row, one at a time, only while nothing is
    /// playing. The cost model that makes this defensible: the sets
    /// extraction is a sparse index walk on the mediahost (kilobytes,
    /// idle-tier there) and ~15 s of hub CPU per track, once ever —
    /// peanuts next to the ED2K pass that reads every byte of every
    /// file. The per-track button stays as the urgent path.
    #[cfg(feature = "ocr")]
    pub fn spawn_ocr_sweep(
        self: &Arc<Self>,
        registry: Arc<Registry>,
        sessions: Arc<crate::sessions::Sessions>,
    ) {
        let subs = self.clone();
        tokio::spawn(async move {
            // Let links and reconnect scans settle first.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            // Tracks that failed stay failed for this hub run — a
            // corrupt track must not become a 15-second crash loop.
            let mut failed: std::collections::HashSet<i64> = Default::default();
            loop {
                let candidates = subs.ocr_candidates(&registry).await;
                let mut generated = 0usize;
                for id in candidates {
                    if failed.contains(&id) {
                        continue;
                    }
                    // Idle means idle: playback outranks the sweep.
                    while !sessions.list().is_empty() {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    }
                    // The row may have vanished under a rescan since the
                    // candidate query ran; only try where the model and
                    // the mediahost exist.
                    let Ok(Some(track)) = crate::tracks::get(registry.db(), id).await else {
                        failed.insert(id);
                        continue;
                    };
                    let Ok((module_id, collection_id, extract_rel, extract_idx, language)) =
                        subs.extract_ref(&registry, &track).await
                    else {
                        failed.insert(id);
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
                                &extract_rel,
                                extract_idx,
                                SETS_WAIT_IDLE,
                            )
                            .await;
                        failed.insert(id);
                        continue;
                    }
                    match subs
                        .ocr_generate_within(&registry, id, "idle-sweep", SETS_WAIT_IDLE)
                        .await
                    {
                        Ok(_) => generated += 1,
                        Err(e) => {
                            tracing::warn!(track = id, item = %track.item_id,
                                error = format!("{e:#}"), "idle OCR failed; skipping this run");
                            failed.insert(id);
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                }
                if generated > 0 {
                    tracing::info!(generated, "idle OCR sweep round complete");
                }
                tokio::time::sleep(std::time::Duration::from_secs(600)).await;
            }
        });
    }

    /// Image subtitle tracks with no OCR text row derived from them
    /// yet — the sweep's work list, ordered so one item finishes before
    /// the next begins.
    #[cfg(feature = "ocr")]
    async fn ocr_candidates(&self, registry: &Registry) -> Vec<i64> {
        sqlx::query_scalar(
            "SELECT t.id FROM subtitle_tracks t
             WHERE t.origin IN ('embedded', 'sidecar')
               AND t.format IN ('pgs', 'vobsub', 'dvdsub')
               AND NOT EXISTS (
                     SELECT 1 FROM subtitle_tracks d
                     WHERE d.derived_from = t.id AND d.origin = 'ocr')
             ORDER BY t.item_id, t.id",
        )
        .fetch_all(registry.db())
        .await
        .unwrap_or_default()
    }

    /// Remove a hub-stored track (downloaded/OCR: row + cached body).
    /// Scan-owned rows refuse — deleting one would only last until the
    /// next rescan re-materializes it.
    pub async fn delete_track(&self, registry: &Registry, id: i64) -> Result<bool> {
        let n = sqlx::query(
            "DELETE FROM subtitle_tracks
             WHERE id = ? AND origin IN ('downloaded', 'ocr')",
        )
        .bind(id)
        .execute(registry.db())
        .await?
        .rows_affected();
        if n > 0 {
            let _ = std::fs::remove_file(self.downloaded_path(id));
        }
        Ok(n > 0)
    }

    /// Ingest a mediahost-extracted track into the cache (ladder step 2).
    pub fn store_extracted(
        &self,
        module_id: &str,
        collection_id: &str,
        path_rel: &str,
        key: &str,
        ex: &Extracted,
    ) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!(
            "{}.json",
            cache_key(module_id, collection_id, path_rel, key)
        ));
        std::fs::write(&path, serde_json::to_vec(ex)?)?;
        Ok(())
    }

    /// Ladder step 2, urgent: ask the file's mediahost to extract (local
    /// reads, no lease traffic) and wait for the cache to land. Returns
    /// the cached result, or None on timeout/disconnect — the caller
    /// falls back to hub-side lease extraction.
    /// HUB-32b: the display-set file for one image subtitle track,
    /// asked of the mediahost (whose disk makes the index walk free)
    /// and cached like any other extraction. `None` when the host is
    /// gone or the track has no usable index — the caller then plans
    /// without a burn instead of promising one.
    pub async fn image_sets(
        &self,
        registry: &Registry,
        module_id: &str,
        collection_id: &str,
        path_rel: &str,
        sub_index: usize,
        wait: std::time::Duration,
    ) -> Option<std::path::PathBuf> {
        let key = format!("i{sub_index}");
        let cache_path = self.dir.join(format!(
            "{}.sets",
            cache_key(module_id, collection_id, path_rel, &key)
        ));
        if tokio::fs::metadata(&cache_path).await.is_ok() {
            return Some(cache_path);
        }
        if !registry.is_connected(module_id) {
            return None;
        }
        let msg = kahawai_proto::v1::HubToHost {
            msg: Some(kahawai_proto::v1::hub_to_host::Msg::ExtractImageSubs(
                kahawai_proto::v1::ExtractImageSubs {
                    collection_id: collection_id.to_string(),
                    path_rel: path_rel.to_string(),
                    sub_index: sub_index as u32,
                },
            )),
        };
        registry.send_to_host(module_id, msg).await.ok()?;
        tracing::info!(collection = %collection_id, path = %path_rel, track = sub_index,
            "image display sets requested from mediahost");
        // A viewer is waiting on this one: bounded, unlike the text
        // extraction's patient 10 minutes.
        let deadline = std::time::Instant::now() + wait;
        while std::time::Instant::now() < deadline {
            if tokio::fs::metadata(&cache_path).await.is_ok() {
                return Some(cache_path);
            }
            if !registry.is_connected(module_id) {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        tracing::warn!(collection = %collection_id, path = %path_rel, track = sub_index,
            "image display sets did not arrive in time");
        None
    }

    /// Store what the mediahost walked, in the worker's own format.
    pub async fn store_image_sets(
        &self,
        module_id: &str,
        msg: &kahawai_proto::v1::ImageSubtitles,
    ) -> Result<()> {
        let key = format!("i{}", msg.sub_index);
        let path = self.dir.join(format!(
            "{}.sets",
            cache_key(module_id, &msg.collection_id, &msg.path_rel, &key)
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

    async fn request_extraction(
        &self,
        registry: &Registry,
        module_id: &str,
        collection_id: &str,
        path_rel: &str,
        key: &str,
    ) -> Option<Extracted> {
        if !registry.is_connected(module_id) {
            return None;
        }
        let msg = kahawai_proto::v1::HubToHost {
            msg: Some(kahawai_proto::v1::hub_to_host::Msg::ExtractSubs(
                kahawai_proto::v1::ExtractSubs {
                    collection_id: collection_id.to_string(),
                    path_rel: path_rel.to_string(),
                },
            )),
        };
        registry.send_to_host(module_id, msg).await.ok()?;
        tracing::info!(collection = %collection_id, path = %path_rel,
            "urgent subtitle extraction requested from mediahost");
        let cache_path = self.dir.join(format!(
            "{}.json",
            cache_key(module_id, collection_id, path_rel, key)
        ));
        // The mediahost is never slower than dragging the file over the
        // lease ourselves — wait while its link is alive (10 min sanity
        // cap); the lease fallback is for disconnects, not slowness.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
        while std::time::Instant::now() < deadline {
            if let Ok(bytes) = tokio::fs::read(&cache_path).await {
                return serde_json::from_slice(&bytes).ok();
            }
            if !registry.is_connected(module_id) {
                tracing::warn!(path = %path_rel, "mediahost gone mid-extraction; falling back to lease");
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
        tracing::warn!(path = %path_rel, "mediahost extraction timed out; falling back to lease");
        None
    }

    /// Font attachments of the item's source (HUB-32), extracted once
    /// and cached: returns (name, bytes) pairs.
    pub async fn fonts(
        &self,
        registry: &Registry,
        sessions: &Sessions,
        item_id: &str,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        let (module_id, collection_id, path_rel, info) = source_row(registry, item_id).await?;
        let cache_key = format!(
            "fonts-{:016x}",
            xxhash_rust::xxh3::xxh3_64(
                format!("{module_id}\n{collection_id}\n{path_rel}").as_bytes()
            )
        );
        let lock = {
            let mut map = self.inflight.lock().unwrap();
            map.entry(cache_key.clone()).or_default().clone()
        };
        let _guard = lock.lock().await;
        let dir = self.dir.join(&cache_key);
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
                    let (_, _, _, _, lease) = sessions.open_source(registry, item_id).await?;
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
                        item = item_id,
                        fonts = out.len(),
                        "fonts read from declared ranges"
                    );
                    out
                }
            }
            None => {
                let (_, _, size, _, lease) = sessions.open_source(registry, item_id).await?;
                let source = crate::sessions::LeaseSource {
                    lease,
                    size,
                    handle: tokio::runtime::Handle::current(),
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

/// HUB-32c: which embedded streams of an item already have an OCR text
/// track derived from them — negotiation prefers text over burn for
/// those. Keyed (module, collection, path, stream index) because rows
/// bind to their source: multi-source items get each source's own
/// answer, which the old item-wide release_name parse got wrong.
pub async fn ocr_stream_set(
    db: &sqlx::SqlitePool,
    item_id: &str,
) -> std::collections::HashSet<(String, String, String, i64)> {
    sqlx::query_as(
        "SELECT t.module_id, t.collection_id, t.path_rel, t.stream_index
         FROM subtitle_tracks t
         WHERE t.item_id = ? AND t.origin = 'embedded'
           AND EXISTS (
                 SELECT 1 FROM subtitle_tracks d
                 WHERE d.derived_from = t.id AND d.origin = 'ocr')",
    )
    .bind(item_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .collect()
}

/// [`ocr_stream_set`] as `negotiate()`'s per-stream flag vec, for the
/// source one plan is judging.
pub fn ocr_flags_for(
    set: &std::collections::HashSet<(String, String, String, i64)>,
    module_id: &str,
    collection_id: &str,
    path_rel: &str,
    n_subs: usize,
) -> Vec<bool> {
    (0..n_subs)
        .map(|i| {
            set.contains(&(
                module_id.to_string(),
                collection_id.to_string(),
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

/// The item's source the way `Sessions::open_source` picks it, without
/// opening a lease: (module, collection, path, streams info).
pub(crate) async fn source_row(
    registry: &Registry,
    item_id: &str,
) -> Result<(String, String, String, kahawai_core::media::MediaInfo)> {
    let rows = sqlx::query(
        "SELECT s.module_id, s.collection_id, s.path_rel, f.streams_json
         FROM item_sources s
         JOIN files f ON (f.module_id, f.collection_id, f.path_rel)
                       = (s.module_id, s.collection_id, s.path_rel)
         WHERE s.item_id = ? ORDER BY f.size DESC",
    )
    .bind(item_id)
    .fetch_all(registry.db())
    .await?;
    let row = rows
        .iter()
        .find(|r| registry.is_connected(&r.get::<String, _>("module_id")))
        .or(rows.first())
        .context("no sources for item")?;
    let info: kahawai_core::media::MediaInfo =
        serde_json::from_str(row.get::<String, _>("streams_json").as_str()).unwrap_or_default();
    Ok((
        row.get("module_id"),
        row.get("collection_id"),
        row.get("path_rel"),
        info,
    ))
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
