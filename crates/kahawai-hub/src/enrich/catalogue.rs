//! Provider execution for mediadb. No legacy catalogue queries belong here.
//! A worker owns exactly one provider lane. Requests have a bounded lifetime;
//! backoff is persisted and releases the claim instead of sleeping in a job.
use super::*;
use kahawai_mediadb as m;
use std::time::Duration;

const PROVIDERS: &[&str] = &[
    "local",
    "tmdb-artwork",
    "tvdb-artwork",
    "anilist-artwork",
    "local-artwork",
    "coverartarchive",
    "artist-collage",
    "tmdb",
    "tvdb",
    "anidb",
    "anidb-hash",
    "anilist",
    "anime-mappings",
    "musicbrainz",
    "fanart",
    "theaudiodb",
];
// A job may fetch several paced pages. The lease outlives the total attempt,
// allowing process-death recovery without two live owners of an ordinary job.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(120);
const CLAIM_SECONDS: i64 = 180;
pub(super) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
#[derive(Debug, thiserror::Error)]
#[error("provider credentials are not configured")]
struct Unconfigured;

impl Enricher {
    pub(crate) fn catalogue_active(&self) -> bool {
        self.catalogue_started.load(Ordering::SeqCst)
    }
    pub fn start_catalogue(self: &Arc<Self>, registry: Arc<Registry>) {
        if self.catalogue_started.swap(true, Ordering::SeqCst) {
            self.catalogue_wake.notify_waiters();
            return;
        }
        for &provider in PROVIDERS {
            let this = self.clone();
            let registry = registry.clone();
            tokio::spawn(async move {
                loop {
                    let notified = this.catalogue_wake.notified();
                    match this.catalogue_step(&registry, provider).await {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(error) => {
                            tracing::error!(provider,error=%error,"enrichment queue operation failed")
                        }
                    }
                    tokio::select! {_=notified=>{},_=tokio::time::sleep(Duration::from_secs(2))=>{}}
                }
            });
        }
    }
    async fn catalogue_step(
        self: &Arc<Self>,
        registry: &Arc<Registry>,
        provider: &str,
    ) -> Result<bool> {
        self.catalogue_import
            .get_or_try_init(|| seed_hash_cache(registry))
            .await?;
        let store = registry.catalogue();
        let Some(job) = store
            .claim_enrichment(provider, now(), CLAIM_SECONDS)
            .await?
        else {
            return Ok(false);
        };
        let input = store.enrichment_input(&job.item_id).await?;
        let result = tokio::time::timeout(
            ATTEMPT_TIMEOUT,
            self.catalogue_answer(registry, provider, &input, job.force),
        )
        .await
        .unwrap_or_else(|_| Err(AttemptTimedOut.into()));
        match result {
            Ok(answer) => {
                store.finish_enrichment(&job, &answer).await?;
            }
            Err(error) if error.is::<ProviderPaused>() => {
                store.defer_enrichment(&job).await?;
            }
            Err(error) => {
                let blocked = error.is::<Unconfigured>()
                    || is_http_status(&error, reqwest::StatusCode::UNAUTHORIZED)
                    || is_http_status(&error, reqwest::StatusCode::FORBIDDEN);
                let backoff = error.downcast_ref::<crate::gate::ProviderBackoff>();
                let delay = error
                    .downcast_ref::<AniDbPaused>()
                    .map(|b| b.0)
                    .or_else(|| backoff.map(|b| b.retry_after().as_secs() as i64))
                    .unwrap_or_else(|| 60 * (1_i64 << job.attempts.min(6)));
                // Do not persist provider URLs/credentials from reqwest errors.
                let message = if blocked {
                    "Provider credentials are not configured".into()
                } else if let Some(backoff) = backoff {
                    backoff.to_string()
                } else if matches!(provider, "local" | "local-artwork" | "artist-collage") {
                    format!("{provider}: {error:#}; retry scheduled")
                } else {
                    format!("{} request failed; retry scheduled", provider)
                };
                store
                    .fail_enrichment(
                        &job,
                        now() + delay,
                        &message,
                        blocked
                            || backoff.is_some()
                            || error.is::<AniDbPaused>()
                            || error.is::<AttemptTimedOut>()
                            || is_http_transient(&error),
                        blocked,
                    )
                    .await?;
                // Report only structured HTTP facts: error strings and their
                // context may contain provider URLs or credentials.
                let http = error
                    .chain()
                    .find_map(|e| e.downcast_ref::<reqwest::Error>());
                tracing::warn!(
                    provider, item=%job.item_id,
                    http_status = http.and_then(|e| e.status()).map(|s| s.as_u16()),
                    timeout = error.is::<AttemptTimedOut>() || http.is_some_and(|e| e.is_timeout()),
                    connect = http.is_some_and(|e| e.is_connect()),
                    decode = http.is_some_and(|e| e.is_decode()),
                    retry_in_seconds = delay,
                    "{message}"
                );
            }
        }
        Ok(true)
    }
    async fn catalogue_answer(
        self: &Arc<Self>,
        registry: &Arc<Registry>,
        provider: &str,
        input: &m::EnrichmentInput,
        force: bool,
    ) -> Result<m::EnrichmentAnswer> {
        if matches!(
            provider,
            "tmdb-artwork"
                | "tvdb-artwork"
                | "anilist-artwork"
                | "local-artwork"
                | "coverartarchive"
                | "artist-collage"
        ) {
            return self.catalogue_artwork(registry, provider, input).await;
        }
        if provider == "anidb-hash" {
            return self.catalogue_hashes(registry, input).await;
        }
        if provider == "anime-mappings" {
            return self.catalogue_mappings(input).await;
        }
        if provider == "local" {
            return self.catalogue_local(registry, input).await;
        }
        if matches!(provider, "fanart" | "theaudiodb") {
            return self
                .catalogue_artist(registry, provider, input, force)
                .await;
        }
        let target = input
            .selected
            .as_ref()
            .filter(|(_, r)| r.provider == provider)
            .map(|(_, r)| m::RecordLink {
                provider: r.provider.clone(),
                namespace: r.namespace.clone(),
                external_id: r.external_id.clone(),
            })
            .or_else(|| {
                input
                    .links
                    .iter()
                    .find(|l| l.provider == provider && l.namespace != "artist")
                    .cloned()
            });
        // Once identified, a different provider needs a verified bridge. It
        // cannot attach an unrelated same-title search result as a supplement.
        if input.selected.is_some() && target.is_none() && provider != "anidb" {
            return Ok(m::EnrichmentAnswer::default());
        }
        let wanted = registry
            .catalogue()
            .media_entries(&input.item_id)
            .await?
            .into_iter()
            .map(|e| e.data.kind)
            .collect::<Vec<_>>();
        let mut hash_answers = Vec::new();
        if provider == "anidb" {
            for h in input.sources.iter().flat_map(|s| &s.hashes) {
                hash_answers.push(
                    registry
                        .catalogue()
                        .cache_answer("anidb-hash", &format!("ed2k:{}", h.ed2k_hex))
                        .await?,
                );
            }
        }
        let question = serde_json::to_string(&(
            &wanted,
            &hash_answers,
            input.media_type,
            input.title.as_str(),
            input.year,
            &input.artist,
            &target,
            if provider == "anidb" {
                input
                    .sources
                    .iter()
                    .flat_map(|s| s.hashes.iter().map(|h| h.ed2k_hex.as_str()))
                    .collect::<Vec<_>>()
            } else {
                vec![]
            },
        ))?;
        if !force
            && let Some(cached) = registry
                .catalogue()
                .recent_cache_answer(provider, &question, now() - 7 * 24 * 3600)
                .await?
        {
            return Ok(serde_json::from_str(&cached)?);
        }
        provider_ready(registry.catalogue(), provider).await?;
        let answer = match provider {
            "tmdb" | "tvdb" => {
                self.catalogue_video(registry, provider, input, target.as_ref())
                    .await?
            }
            "musicbrainz" => self.catalogue_musicbrainz(input, target.as_ref()).await?,
            "anilist" => self.catalogue_anilist(input, target.as_ref()).await?,
            "anidb" => {
                self.catalogue_anidb(registry, input, target.as_ref())
                    .await?
            }
            _ => anyhow::bail!("unknown provider"),
        };
        registry
            .catalogue()
            .put_cache_answer(provider, &question, &serde_json::to_string(&answer)?, now())
            .await?;
        Ok(answer)
    }
    async fn catalogue_video(
        &self,
        registry: &Registry,
        provider: &str,
        input: &m::EnrichmentInput,
        target: Option<&m::RecordLink>,
    ) -> Result<m::EnrichmentAnswer> {
        let kind = if input.media_type == m::MediaType::Movies
            || target.is_some_and(|t| t.namespace == "movie")
        {
            "movie"
        } else {
            "show"
        };
        let mut candidates;
        let mut episodes = Vec::new();
        let mut rich: Option<serde_json::Value> = None;
        let mut linked = Vec::new();
        if provider == "tmdb" {
            let key = tmdb_key(registry).await?.ok_or(Unconfigured)?;
            let lease = self.provider_lease(TMDB);
            candidates = if let Some(t) = target {
                {
                    // TMDB documents append_to_response and TV external IDs:
                    // https://developer.themoviedb.org/docs/append-to-response
                    // https://developer.themoviedb.org/reference/tv-series-external-ids
                    let path = if kind == "movie" { "movie" } else { "tv" };
                    let mut request = self
                        .http
                        .get(format!(
                            "https://api.themoviedb.org/3/{path}/{}",
                            t.external_id
                        ))
                        .query(&[("append_to_response", "credits,external_ids")]);
                    if is_v4_token(&key) {
                        request = request.bearer_auth(&key);
                    } else {
                        request = request.query(&[("api_key", key.as_str())]);
                    }
                    let value: serde_json::Value = self
                        .http
                        .send_current(request, lease.clone())
                        .await?
                        .status_checked()?
                        .json()
                        .await?;
                    if let Some(id) = value["external_ids"]["tvdb_id"].as_u64() {
                        linked.push(m::RecordLink {
                            provider: "tvdb".into(),
                            namespace: "show".into(),
                            external_id: id.to_string(),
                        });
                    }
                    let candidate = serde_json::from_value(value.clone())?;
                    rich = Some(value);
                    vec![candidate]
                }
            } else {
                self.search(&key, kind, &input.title, input.year.map(i64::from), &lease)
                    .await?
            };
            if let Some(t) = target
                && kind == "show"
            {
                let mut absolute = 0;
                for (season, count) in self.tmdb_seasons(&key, &t.external_id, &lease).await? {
                    let question = format!("season:{}:{season}", t.external_id);
                    let mut children: Vec<EpisodeData> = if let Some(cached) = registry
                        .catalogue()
                        .recent_cache_answer("tmdb-pages", &question, now() - 7 * 24 * 3600)
                        .await?
                    {
                        serde_json::from_str(&cached)?
                    } else {
                        let children = self
                            .tmdb_season(&key, &t.external_id, season, &lease)
                            .await?;
                        registry
                            .catalogue()
                            .put_cache_answer(
                                "tmdb-pages",
                                &question,
                                &serde_json::to_string(&children)?,
                                now(),
                            )
                            .await?;
                        children
                    };
                    for child in &mut children {
                        child.absolute = Some(absolute + child.episode);
                    }
                    absolute += count;
                    episodes.extend(children);
                }
            }
        } else {
            let creds = tvdb_creds(registry).await?.ok_or(Unconfigured)?;
            let lease = self.provider_lease(TVDB);
            let token = self.tvdb_token(&creds, &lease).await?;
            candidates = if let Some(t) = target {
                vec![
                    self.tvdb_details(&token, kind, t.external_id.parse()?, &lease)
                        .await?,
                ]
            } else {
                self.tvdb_search(&token, kind, &input.title, &lease).await?
            };
            if let Some(t) = target
                && kind == "show"
            {
                episodes = self
                    .tvdb_episodes_english_cached(
                        &token,
                        &t.external_id,
                        "default",
                        &lease,
                        Some(registry.catalogue()),
                    )
                    .await?;
            }
        }
        let best = pick_candidate(&candidates, &input.title, input.year.map(i64::from))
            .map(|(c, confidence)| (c.id, confidence));
        candidates.truncate(10);
        let mut answer = m::EnrichmentAnswer::default();
        for c in candidates {
            let strength = if target.is_some()
                || best.is_some_and(|(id, confidence)| id == c.id && confidence == "auto")
            {
                10
            } else {
                0
            };
            let mut record = candidate_record(provider, kind, &c, input.media_type);
            if let Some(value) = &rich {
                record.description.genres = value["genres"].as_array().map(|v| {
                    v.iter()
                        .filter_map(|g| g["name"].as_str().map(str::to_owned))
                        .collect()
                });
                record.description.cast = value["credits"]["cast"].as_array().map(|v| {
                    v.iter()
                        .filter_map(|c| {
                            Some(m::Credit {
                                name: c["name"].as_str()?.into(),
                                role: c["character"].as_str().map(str::to_owned),
                            })
                        })
                        .collect()
                });
            }
            if !episodes.is_empty() {
                record.description.children = Some(episodes.iter().map(child).collect());
            }
            answer.candidates.push(m::EnrichmentCandidate {
                complete: target.is_some(),
                record,
                strength,
                links: linked.clone(),
            });
        }
        if target.is_some() && kind == "show" {
            let entries = registry.catalogue().media_entries(&input.item_id).await?;
            if entries.iter().any(|entry| match &entry.data.kind {
                m::EntryKind::Episode { episodes: spans } => spans.iter().any(|span| {
                    let end = span.episode_end.unwrap_or(span.episode);
                    let count = episodes
                        .iter()
                        .filter(|e| {
                            let number = if let Some(season) = span.season {
                                if e.season != Some(i64::from(season)) {
                                    return false;
                                }
                                Some(e.episode)
                            } else {
                                e.absolute
                            };
                            number.is_some_and(|n| {
                                n >= i64::from(span.episode) && n <= i64::from(end)
                            })
                        })
                        .count() as u64;
                    count < u64::from(end) - u64::from(span.episode) + 1
                }),
                _ => false,
            }) {
                answer.refresh_at = Some(now() + 7 * 24 * 3600);
            }
        }
        Ok(answer)
    }
    async fn catalogue_anilist(
        &self,
        input: &m::EnrichmentInput,
        target: Option<&m::RecordLink>,
    ) -> Result<m::EnrichmentAnswer> {
        let found = if let Some(t) = target {
            self.anilist
                .media_by_id(t.external_id.parse()?)
                .await?
                .into_iter()
                .collect()
        } else {
            self.anilist.search(&input.title).await?
        };
        let mut answer = m::EnrichmentAnswer::default();
        let exact: Vec<_> = found
            .iter()
            .filter(|v| {
                v.title.english.as_deref().map(fold) == Some(fold(&input.title))
                    || v.title.romaji.as_deref().map(fold) == Some(fold(&input.title))
            })
            .filter(|v| {
                input
                    .year
                    .is_none_or(|y| v.start_date.as_ref().and_then(|d| d.year) == Some(y))
            })
            .map(|v| v.id)
            .collect();
        for media in found.into_iter().take(10) {
            let title = media.display_title().unwrap_or_else(|| input.title.clone());
            let mut record = base_record(
                "anilist",
                "anime",
                &media.id.to_string(),
                &title,
                m::MediaType::Anime,
            );
            record.year = media.start_date.as_ref().and_then(|d| d.year);
            record.description = m::Description {
                overview: media.plain_description(),
                original_title: media.title.romaji.clone(),
                original_language: media.original_language().map(str::to_owned),
                release_date: media.premiered(),
                rating: media.average_score.map(|s| s / 10.0),
                genres: media.genres.clone(),
                artwork: media
                    .cover_image
                    .as_ref()
                    .and_then(|c| c.extra_large.clone().or(c.large.clone()))
                    .map(|u| vec![u]),
                ..Default::default()
            };
            let strength = if target.is_some() || exact.len() == 1 && exact[0] == media.id {
                10
            } else {
                0
            };
            answer.candidates.push(m::EnrichmentCandidate {
                complete: true,
                record,
                strength,
                links: vec![],
            });
        }
        Ok(answer)
    }
    async fn catalogue_anidb(
        self: &Arc<Self>,
        registry: &Registry,
        input: &m::EnrichmentInput,
        target: Option<&m::RecordLink>,
    ) -> Result<m::EnrichmentAnswer> {
        if let Some(remaining) = crate::anidb::ban_remaining(&self.data_dir)? {
            return Err(AniDbPaused(remaining).into());
        }
        let mut aids = Vec::new();
        let mut exact = false;
        for hash in input
            .sources
            .iter()
            .flat_map(|s| &s.hashes)
            .filter(|h| !h.ed2k_hex.is_empty() && h.error.is_empty())
        {
            let question = format!("ed2k:{}", hash.ed2k_hex);
            let hit: Option<m::ContentIdentification> = registry
                .catalogue()
                .cache_answer("anidb-hash", &question)
                .await?
                .map(|s| serde_json::from_str(&s))
                .transpose()?
                .flatten();
            if let Some(hit) = hit {
                aids.push(hit.aid);
                exact = true;
            }
        }
        aids.sort_unstable();
        aids.dedup();
        if aids.is_empty() {
            if let Some(t) = target {
                aids.push(t.external_id.parse()?)
            } else {
                aids = crate::anime::AnidbTitles::load(&self.http, &self.data_dir)
                    .await?
                    .candidates(&input.title);
            }
        }
        let unique = aids.len() == 1;
        let mut answer = m::EnrichmentAnswer::default();
        for aid in aids.into_iter().take(10) {
            let info = crate::anime::anidb_anime_info(&self.http, &self.data_dir, aid).await?;
            let mut record = base_record(
                "anidb",
                "anime",
                &aid.to_string(),
                &info.title,
                m::MediaType::Anime,
            );
            record.year = info.year.and_then(|y| i32::try_from(y).ok());
            let children =
                crate::anime::anidb_episode_titles(&self.http, &self.data_dir, aid, &[]).await?;
            record.description.children = Some(
                children
                    .into_iter()
                    .map(|(episode, title)| m::ChildMetadata {
                        title,
                        episode: u32::try_from(episode).ok(),
                        ..Default::default()
                    })
                    .collect(),
            );
            let strength = if unique && exact {
                20
            } else if unique && input.year.is_none_or(|y| record.year == Some(y)) {
                10
            } else {
                0
            };
            answer.candidates.push(m::EnrichmentCandidate {
                complete: true,
                record,
                strength,
                links: vec![],
            });
        }
        Ok(answer)
    }
    async fn catalogue_local(
        &self,
        registry: &Registry,
        input: &m::EnrichmentInput,
    ) -> Result<m::EnrichmentAnswer> {
        let mut answer = m::EnrichmentAnswer::default();
        let mut record = base_record(
            "local",
            &format!("{}:{}", input.mediahost_id, input.collection_id),
            &format!("{}:art", input.item_id),
            &input.title,
            input.media_type,
        );
        record.year = input.year;
        let mut seen = std::collections::HashSet::new();
        let mut children = Vec::new();
        for entry in registry.catalogue().media_entries(&input.item_id).await? {
            if let m::EntryKind::Track { disc, track } = entry.data.kind {
                children.push(m::ChildMetadata {
                    title: entry.data.title,
                    disc,
                    track,
                    ..Default::default()
                });
            }
        }
        for source in &input.sources {
            let Some(media) = &source.media else { continue };
            if let Some(art) = &media.artwork {
                record.description.artwork =
                    Some(vec![format!("local://{}/{}", source.root_token, art)]);
            }
            if let Some(nfo) = &media.nfo {
                if !seen.insert((source.root_token.clone(), nfo.clone())) {
                    continue;
                }
                let lease = self
                    .sessions
                    .get()
                    .context("metadata byte reader unavailable")?
                    .open_lease(
                        registry,
                        &input.mediahost_id,
                        &input.remote_id,
                        &source.root_token,
                        nfo,
                        crate::sessions::Reader::Sweep,
                    )
                    .await?;
                let bytes = read_nfo(lease).await?;
                let text = String::from_utf8_lossy(&bytes);
                if let Ok(doc) = roxmltree::Document::parse(&text)
                    && doc.root_element().has_tag_name("episodedetails")
                {
                    let value = |name: &str| {
                        doc.root_element()
                            .children()
                            .find(|n| n.has_tag_name(name))
                            .and_then(|n| n.text())
                    };
                    children.push(m::ChildMetadata {
                        title: value("title").unwrap_or_default().into(),
                        season: value("season").and_then(|n| n.parse().ok()),
                        episode: value("episode").and_then(|n| n.parse().ok()),
                        overview: value("plot").map(str::to_owned),
                        ..Default::default()
                    });
                    continue;
                }
                if let Some((fields, claim)) = parse_nfo(&text) {
                    record.external_id =
                        format!("{}:nfo:{}", input.item_id, claim.as_deref().unwrap_or(nfo));
                    if let Some(title) = fields.title {
                        record.title = title;
                    }
                    record.year = fields.premiered.as_deref().and_then(year).or(input.year);
                    record.description.overview = fields.overview;
                    record.description.release_date = fields.premiered;
                    record.description.rating = fields.rating;
                    record.description.genres = fields
                        .genres
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()?;
                    answer.candidates = vec![m::EnrichmentCandidate {
                        complete: true,
                        record: record.clone(),
                        strength: 30,
                        links: vec![],
                    }];
                }
            }
        }
        if !children.is_empty() {
            record.description.children = Some(children);
        }
        if !answer.candidates.is_empty()
            || record.description.artwork.is_some()
            || record.description.children.is_some()
        {
            answer.local = Some(record);
        }
        Ok(answer)
    }
    async fn catalogue_artist(
        &self,
        registry: &Registry,
        provider: &str,
        input: &m::EnrichmentInput,
        force: bool,
    ) -> Result<m::EnrichmentAnswer> {
        let identity = registry
            .catalogue()
            .artist_identity(&input.item_id)
            .await?
            .or_else(|| {
                input
                    .links
                    .iter()
                    .find(|l| l.provider == "musicbrainz" && l.namespace == "artist")
                    .map(|l| m::ArtistIdentity {
                        id: l.external_id.clone(),
                        name: input.artist.clone().unwrap_or_default(),
                    })
            });
        let Some(identity) = identity else {
            return Ok(Default::default());
        };
        let artist = m::RecordLink {
            provider: "musicbrainz".into(),
            namespace: "artist".into(),
            external_id: identity.id,
        };
        let cache_question = format!("artist:{}", artist.external_id);
        if !force
            && let Some(cached) = registry
                .catalogue()
                .cache_answer(provider, &cache_question)
                .await?
        {
            let url: Option<String> = serde_json::from_str(&cached)?;
            if url.as_ref().is_none_or(|url| {
                self.artwork
                    .get()
                    .and_then(std::sync::Weak::upgrade)
                    .is_some_and(|a| a.remote_cache_complete(url))
            }) {
                registry
                    .catalogue()
                    .put_artist_artwork(&artist.external_id, provider, url.as_deref(), now())
                    .await?;
                return Ok(Default::default());
            }
        }
        provider_ready(registry.catalogue(), provider).await?;
        let url = if provider == "fanart" {
            let key = fanart_client_key(registry).await?.ok_or(Unconfigured)?;
            self.fanart_artist(&key, &artist.external_id, self.provider_lease(FANART))
                .await?
                .and_then(|mut a| a.best_image().map(|i| i.url.clone()))
        } else {
            let (key, premium, lease) = self.theaudiodb_credential(registry).await?;
            match self
                .theaudiodb_artist(&key, premium, &artist.external_id, &identity.name, lease)
                .await?
            {
                TheAudioDbAnswer::Artist(a) => a.best_image().map(|(_, url)| url.to_owned()),
                _ => None,
            }
        };
        if let Some(url) = &url {
            let artwork = self
                .artwork
                .get()
                .and_then(std::sync::Weak::upgrade)
                .context("artwork store unavailable")?;
            if !artwork.prefetch_remote(url).await? {
                return Ok(Default::default());
            }
        }
        registry
            .catalogue()
            .put_cache_answer(
                provider,
                &cache_question,
                &serde_json::to_string(&url)?,
                now(),
            )
            .await?;
        registry
            .catalogue()
            .put_artist_artwork(&artist.external_id, provider, url.as_deref(), now())
            .await?;
        Ok(m::EnrichmentAnswer::default())
    }
}
fn year(date: &str) -> Option<i32> {
    date.get(..4)?.parse().ok()
}
fn base_record(
    provider: &str,
    namespace: &str,
    id: &str,
    title: &str,
    media_type: m::MediaType,
) -> m::ProviderRecord {
    m::ProviderRecord {
        provider: provider.into(),
        namespace: namespace.into(),
        external_id: id.into(),
        language: "en".into(),
        media_type,
        title: title.into(),
        year: None,
        description: Default::default(),
    }
}
fn candidate_record(
    provider: &str,
    kind: &str,
    c: &Candidate,
    media_type: m::MediaType,
) -> m::ProviderRecord {
    let _ = media_type;
    let mut r = base_record(
        provider,
        kind,
        &c.id.to_string(),
        &c.title,
        if kind == "movie" {
            m::MediaType::Movies
        } else {
            m::MediaType::Series
        },
    );
    r.year = c.release_date.as_deref().and_then(year);
    r.description = m::Description {
        overview: c.overview.clone(),
        original_title: c.original_title.clone(),
        original_language: c.original_language.clone(),
        release_date: c.release_date.clone(),
        rating: c.vote_average,
        artwork: c.poster_path.clone().map(|u| {
            vec![if u.starts_with('/') {
                format!("https://image.tmdb.org/t/p/w500{u}")
            } else {
                u
            }]
        }),
        ..Default::default()
    };
    r
}
fn child(e: &EpisodeData) -> m::ChildMetadata {
    m::ChildMetadata {
        provider_id: Some(e.provider_id.clone()),
        absolute: e.absolute.and_then(|n| u32::try_from(n).ok()),
        artwork: e.image.clone(),
        release_date: e.aired.clone(),
        rating: e.rating,
        title: e.title.clone().unwrap_or_default(),
        season: e.season.and_then(|s| u32::try_from(s).ok()),
        episode: u32::try_from(e.episode).ok(),
        overview: e.overview.clone(),
        ..Default::default()
    }
}
impl Enricher {
    pub(crate) async fn catalogue_search(
        self: &Arc<Self>,
        registry: &Arc<Registry>,
        mut input: m::EnrichmentInput,
        provider: &str,
        query: &str,
    ) -> Result<()> {
        anyhow::ensure!(
            matches!(
                provider,
                "tmdb" | "tvdb" | "anidb" | "anilist" | "musicbrainz"
            ),
            "not an identity provider"
        );
        input.selected = None;
        input.links.clear();
        input.title = query.to_owned();
        input.year = None;
        for s in &mut input.sources {
            s.hashes.clear();
        }
        let answer = tokio::time::timeout(
            ATTEMPT_TIMEOUT,
            self.catalogue_answer(registry, provider, &input, false),
        )
        .await??;
        registry
            .catalogue()
            .offer_candidates(&input.item_id, input.revision, &answer.candidates)
            .await
    }
}
impl Enricher {
    async fn catalogue_mappings(&self, input: &m::EnrichmentInput) -> Result<m::EnrichmentAnswer> {
        let Some((anchor, selected)) = &input.selected else {
            return Ok(Default::default());
        };
        let lists = crate::anime::AnimeLists::load(&self.http, &self.data_dir).await?;
        let aids = if selected.provider == "anidb" {
            vec![selected.external_id.parse()?]
        } else {
            lists.reverse(&selected.provider, &selected.external_id)
        };
        let mut answer = m::EnrichmentAnswer {
            refresh_at: Some(now() + 7 * 24 * 3600),
            ..Default::default()
        };
        if aids.len() != 1 {
            return Ok(answer);
        }
        let aid = aids[0];
        if let Some(mapping) = lists.by_anidb(aid) {
            for (provider, namespace, id) in [
                ("anidb", "anime", Some(aid)),
                ("anilist", "anime", mapping.anilist_id),
                ("tvdb", "show", mapping.tvdb_id),
                ("tmdb", "movie", mapping.tmdb.movie),
                ("tmdb", "show", mapping.tmdb.tv),
            ] {
                if provider == selected.provider {
                    continue;
                }
                if let Some(id) = id {
                    answer.links.push((
                        anchor.clone(),
                        m::RecordLink {
                            provider: provider.into(),
                            namespace: namespace.into(),
                            external_id: id.to_string(),
                        },
                    ));
                }
            }
        }
        Ok(answer)
    }
}
#[derive(Debug, thiserror::Error)]
#[error("AniDB remains paused for {0} seconds")]
struct AniDbPaused(i64);
async fn seed_hash_cache(registry: &Registry) -> Result<()> {
    let store = registry.catalogue();
    if store
        .cache_answer("import", "legacy-content-hashes")
        .await?
        .is_some()
    {
        return Ok(());
    }
    // The only legacy read: content identities, never old copies or assignments.
    for row in sqlx::query("SELECT * FROM ed2k_aid")
        .fetch_all(registry.db())
        .await?
    {
        let hit = row
            .get::<Option<i64>, _>("aid")
            .map(|aid| m::ContentIdentification {
                aid: aid as u32,
                eid: row.get::<Option<i64>, _>("eid").map(|n| n as u32),
                epno: row.get("epno"),
                gid: row.get::<Option<i64>, _>("gid").map(|n| n as u32),
                group_name: row.get("group_name"),
            });
        store
            .seed_cache_answer(
                "anidb-hash",
                &format!("ed2k:{}", row.get::<String, _>("ed2k")),
                &serde_json::to_string(&hit)?,
                now(),
            )
            .await?;
    }
    store
        .seed_cache_answer("import", "legacy-content-hashes", "true", now())
        .await?;
    Ok(())
}
impl Enricher {
    async fn catalogue_artwork(
        &self,
        registry: &Registry,
        provider: &str,
        input: &m::EnrichmentInput,
    ) -> Result<m::EnrichmentAnswer> {
        let artwork = self
            .artwork
            .get()
            .and_then(std::sync::Weak::upgrade)
            .context("artwork store unavailable")?;
        let sessions = self.sessions.get().context("metadata reader unavailable")?;
        if provider == "artist-collage" {
            for library in registry.catalogue().copy_libraries(&input.item_id).await? {
                artwork
                    .prefetch_catalogue_collage(registry, sessions, &input.item_id, &library)
                    .await?;
            }
        } else {
            let metadata = registry
                .catalogue()
                .resolve_metadata(&input.item_id)
                .await?;
            for poster in metadata.description.artwork.unwrap_or_default() {
                let host = reqwest::Url::parse(&poster)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_owned))
                    .unwrap_or_default();
                let owns = match provider {
                    "local-artwork" => poster.starts_with("local://"),
                    "tmdb-artwork" => host == "image.tmdb.org",
                    "coverartarchive" => host == "coverartarchive.org",
                    "tvdb-artwork" => host == "thetvdb.com" || host.ends_with(".thetvdb.com"),
                    "anilist-artwork" => host.ends_with(".anilist.co"),
                    _ => false,
                };
                if !owns {
                    continue;
                }
                if provider == "local-artwork" {
                    for (size, _) in crate::artwork::SIZES {
                        artwork
                            .catalogue_at(registry, sessions, input, &poster, Some(size))
                            .await?;
                    }
                } else {
                    artwork.prefetch_remote(&poster).await?;
                }
            }
        }
        Ok(Default::default())
    }
}
impl Enricher {
    async fn catalogue_musicbrainz(
        &self,
        input: &m::EnrichmentInput,
        target: Option<&m::RecordLink>,
    ) -> Result<m::EnrichmentAnswer> {
        let mut answer = m::EnrichmentAnswer::default();
        let group = if let Some(target) = target {
            // MusicBrainz documents lookup /<ENTITY_TYPE>/<MBID>?inc=<INC>.
            // https://musicbrainz.org/doc/MusicBrainz_API (read 2026-09-12).
            let value: serde_json::Value = self
                .http
                .send(
                    self.http
                        .get(format!(
                            "https://musicbrainz.org/ws/2/release-group/{}",
                            target.external_id
                        ))
                        .query(&[("inc", "artist-credits+genres"), ("fmt", "json")]),
                )
                .await?
                .status_checked()?
                .json()
                .await?;
            let credit = value["artist-credit"].as_array();
            let named = credit.and_then(|c| {
                if c.len() == 1 {
                    Some(&c[0]["artist"])
                } else {
                    None
                }
            });
            let artist = named.and_then(|a| {
                Some(m::ArtistIdentity {
                    id: a["id"].as_str()?.into(),
                    name: a["name"].as_str()?.into(),
                })
            });
            let artist_id = artist.as_ref().map(|a| a.id.clone()).unwrap_or_default();
            answer.artist = artist;
            Some(MbReleaseGroup {
                id: target.external_id.clone(),
                artist_id,
                title: value["title"]
                    .as_str()
                    .context("release group has no title")?
                    .into(),
                first_release_date: value["first-release-date"].as_str().map(str::to_owned),
                genres: value["genres"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|g| g["name"].as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        } else if let Some(artist) = &input.artist {
            self.musicbrainz_album(&input.title, artist).await?
        } else {
            None
        };
        if let Some(group) = group {
            if answer.artist.is_none() && !group.artist_id.is_empty() {
                answer.artist = Some(m::ArtistIdentity {
                    id: group.artist_id.clone(),
                    name: input.artist.clone().unwrap_or_default(),
                });
            }
            let mut record = base_record(
                "musicbrainz",
                "release-group",
                &group.id,
                &group.title,
                m::MediaType::Music,
            );
            record.year = group.first_release_date.as_deref().and_then(year);
            record.description.release_date = group.first_release_date;
            record.description.genres = Some(group.genres);
            record.description.artwork = Some(vec![format!(
                "https://coverartarchive.org/release-group/{}/front",
                group.id
            )]);
            let links = answer
                .artist
                .as_ref()
                .map(|a| {
                    vec![m::RecordLink {
                        provider: "musicbrainz".into(),
                        namespace: "artist".into(),
                        external_id: a.id.clone(),
                    }]
                })
                .unwrap_or_default();
            answer.candidates.push(m::EnrichmentCandidate {
                record,
                strength: 10,
                complete: target.is_some(),
                links,
            });
        }
        if answer.artist.is_none()
            && let Some(artist) = &input.artist
            && let Some(id) = self.musicbrainz_artist(artist).await?
        {
            answer.artist = Some(m::ArtistIdentity {
                id,
                name: artist.clone(),
            });
        }
        Ok(answer)
    }
}
#[derive(Debug, thiserror::Error)]
#[error("provider has no runnable network requests")]
struct ProviderPaused;
#[derive(Debug, thiserror::Error)]
#[error("provider attempt timed out")]
struct AttemptTimedOut;
async fn provider_ready(store: &m::Store, provider: &str) -> Result<()> {
    if let Some((until, blocked)) = store.provider_pause(provider).await?
        && (blocked || until > now())
    {
        return Err(ProviderPaused.into());
    }
    Ok(())
}

impl Enricher {
    async fn catalogue_hashes(
        &self,
        registry: &Registry,
        input: &m::EnrichmentInput,
    ) -> Result<m::EnrichmentAnswer> {
        for hash in input
            .sources
            .iter()
            .flat_map(|s| &s.hashes)
            .filter(|h| !h.ed2k_hex.is_empty() && h.error.is_empty())
        {
            let question = format!("ed2k:{}", hash.ed2k_hex);
            let _hit: Option<m::ContentIdentification> = if let Some(answer) = registry
                .catalogue()
                .cache_answer("anidb-hash", &question)
                .await?
            {
                serde_json::from_str(&answer)?
            } else {
                provider_ready(registry.catalogue(), "anidb-hash").await?;
                if let Some(remaining) = crate::anidb::ban_remaining(&self.data_dir)? {
                    return Err(AniDbPaused(remaining).into());
                }
                let (mut fields, lease) = self
                    .credential_snapshot(registry, crate::anidb::ANIDB)
                    .await?;
                if self.anidb_stale.swap(false, Ordering::AcqRel) {
                    *lease.wait(self.anidb.lock()).await? = None;
                }
                let mut client = lease.wait(self.anidb.lock()).await?;
                if client.is_none() {
                    let user = fields
                        .remove(crate::anidb::USERNAME)
                        .filter(|s| !s.is_empty())
                        .ok_or(Unconfigured)?;
                    let pass = fields
                        .remove(crate::anidb::PASSWORD)
                        .filter(|s| !s.is_empty())
                        .ok_or(Unconfigured)?;
                    let key = fields
                        .remove(crate::anidb::UDP_API_KEY)
                        .filter(|s| !s.is_empty());
                    *client = Some(
                        crate::anidb::Anidb::login_current(
                            &self.data_dir,
                            &user,
                            &pass,
                            key.as_deref(),
                            lease,
                        )
                        .await?,
                    );
                }
                let hit = client
                    .as_mut()
                    .unwrap()
                    .file_by_ed2k(hash.size, &hash.ed2k_hex)
                    .await?;
                let aid = hit.map(|h| m::ContentIdentification {
                    aid: h.aid,
                    eid: Some(h.eid),
                    epno: Some(h.epno),
                    gid: Some(h.gid),
                    group_name: Some(h.group_name),
                });
                registry
                    .catalogue()
                    .put_cache_answer(
                        "anidb-hash",
                        &question,
                        &serde_json::to_string(&aid)?,
                        now(),
                    )
                    .await?;
                aid
            };
        }
        Ok(Default::default())
    }
}
