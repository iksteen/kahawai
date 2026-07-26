//! Text subtitle serving (HUB-15/27): enumerate an item's embedded and
//! sidecar text subtitles, extract/convert them to WebVTT lazily, and
//! cache extracted cues on the hub (embedded extraction demuxes the whole
//! source once — never twice).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use kahawai_media::subtitles::{decode_text, is_text_format, parse, to_vtt, Extracted};
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

/// A served ASS script: complete (cache/sidecar) or streamed while the
/// extraction pass runs.
pub enum AssBody {
    Full(String),
    Stream(tokio::sync::mpsc::Receiver<String>),
}

/// Cache key for one subtitle track of one source file — shared by the
/// lazy extractors and the mediahost ingestion path.
fn cache_key(module_id: &str, collection_id: &str, path_rel: &str, key: &str) -> String {
    format!(
        "v2-{:016x}-{key}",
        xxhash_rust::xxh3::xxh3_64(
            format!("{module_id}\n{collection_id}\n{path_rel}").as_bytes()
        )
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

    /// The subtitle tracks we can serve for an item, from the same source
    /// `open_source` would pick (largest, preferring connected hosts).
    pub async fn list(&self, registry: &Registry, item_id: &str) -> Result<Vec<SubtitleEntry>> {
        let (_, _, _, info) = source_row(registry, item_id).await?;
        let mut out = entries(&info);
        // HUB-24: subtitles the user downloaded from external providers.
        let rows = sqlx::query(
            "SELECT id, language, format, release_name FROM downloaded_subtitles
             WHERE item_id = ? ORDER BY id",
        )
        .bind(item_id)
        .fetch_all(registry.db())
        .await?;
        for r in &rows {
            let format: String = r.get("format");
            out.push(SubtitleEntry {
                key: format!("d{}", r.get::<i64, _>("id")),
                kind: "downloaded",
                format: format.clone(),
                language: r.get::<Option<String>, _>("language"),
                flattened: matches!(format.as_str(), "ass" | "ssa"),
                image: false,
            });
        }
        Ok(out)
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
                    tracing::warn!(error = format!("{e:#}"), "streamed subtitle extraction failed")
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
            let sidecar =
                info.external_subtitles.get(idx).context("sidecar index out of range")?;
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
            return requested
                .with_context(|| format!("no cues extracted (track {idx} missing or not a text track)"));
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
    ) -> Result<(Vec<crate::opensubtitles::Candidate>, crate::opensubtitles::Quota)> {
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
            year: (!is_episode).then(|| row.get::<Option<i64>, _>("search_year")).flatten(),
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
    ) -> Result<(String, crate::opensubtitles::Quota)> {
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
            "INSERT INTO downloaded_subtitles
               (item_id, provider, language, format, release_name, downloaded_by)
             VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(item_id)
        .bind(provider.name())
        .bind(&language)
        .bind(&dl.format)
        .bind(&dl.release_name)
        .bind(user_id)
        .fetch_one(registry.db())
        .await?;
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.downloaded_path(id), serde_json::to_vec(&ex)?)?;
        tracing::info!(item = item_id, id, format = %dl.format, "external subtitle downloaded");
        Ok((format!("d{id}"), provider.quota()))
    }

    /// Remove a downloaded subtitle (row + cached body).
    pub async fn delete_downloaded(&self, registry: &Registry, id: i64) -> Result<bool> {
        let n = sqlx::query("DELETE FROM downloaded_subtitles WHERE id = ?")
            .bind(id)
            .execute(registry.db())
            .await?
            .rows_affected();
        let _ = std::fs::remove_file(self.downloaded_path(id));
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
        let path = self.dir.join(format!("{}.json", cache_key(module_id, collection_id, path_rel, key)));
        std::fs::write(&path, serde_json::to_vec(ex)?)?;
        Ok(())
    }

    /// Ladder step 2, urgent: ask the file's mediahost to extract (local
    /// reads, no lease traffic) and wait for the cache to land. Returns
    /// the cached result, or None on timeout/disconnect — the caller
    /// falls back to hub-side lease extraction.
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
        let cache_path =
            self.dir.join(format!("{}.json", cache_key(module_id, collection_id, path_rel, key)));
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
                    tracing::info!(item = item_id, fonts = out.len(),
                        "fonts read from declared ranges");
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
            image: false,
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
    Ok((row.get("module_id"), row.get("collection_id"), row.get("path_rel"), info))
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

