//! Metadata enrichment (M4/HUB-7): match movies and shows against TMDB,
//! store overview/poster/rating per item. Matching is conservative —
//! normalized-title equality (plus year within ±1 when known) is an
//! `auto` match; a lone plausible result is `weak`; anything else is a
//! recorded `miss` so the next run doesn't re-search it. The admin can
//! re-run after fixing titles; a review queue (HUB-8) comes later.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use sqlx::Row;

use crate::registry::Registry;

pub const TMDB_KEY_SETTING: &str = "tmdb_api_key";
pub const TVDB_KEY_SETTING: &str = "tvdb_api_key";
pub const TVDB_PIN_SETTING: &str = "tvdb_pin";

struct MbReleaseGroup {
    id: String,
    title: String,
    first_release_date: Option<String>,
    genres: Vec<String>,
}

pub struct Enricher {
    http: reqwest::Client,
    data_dir: std::path::PathBuf,
    anilist: crate::anime::Anilist,
    running: AtomicBool,
    /// (matched, weak, missed) of the current/last run.
    progress: (AtomicUsize, AtomicUsize, AtomicUsize),
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    results: Vec<Candidate>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct Candidate {
    id: u64,
    #[serde(alias = "name")]
    title: String,
    #[serde(default, alias = "original_name")]
    original_title: Option<String>,
    #[serde(default)]
    overview: Option<String>,
    #[serde(default)]
    poster_path: Option<String>,
    #[serde(default)]
    vote_average: Option<f64>,
    #[serde(default, alias = "first_air_date")]
    release_date: Option<String>,
}

impl Candidate {
    fn year(&self) -> Option<i64> {
        self.release_date.as_deref()?.get(..4)?.parse().ok()
    }
}

/// Conservative pick: normalized-title equality (title or original),
/// year within ±1 when both sides know it → auto. A single result that
/// at least contains the words → weak. Otherwise none.
pub fn pick_candidate<'c>(
    candidates: &'c [Candidate],
    title: &str,
    year: Option<i64>,
) -> Option<(&'c Candidate, &'static str)> {
    let norm = fold(title);
    // Spaceless tier: acronym titles compare as "s h i e l d" vs
    // "shield" depending on where the dots got normalized away.
    let squash = |s: &str| s.replace(' ', "");
    let norm_sq = squash(&norm);
    let title_eq = |c: &Candidate| {
        fold(&c.title) == norm
            || c.original_title.as_deref().is_some_and(|t| fold(t) == norm)
            || squash(&fold(&c.title)) == norm_sq
    };
    let year_ok = |c: &Candidate| match (year, c.year()) {
        (Some(w), Some(h)) => (w - h).abs() <= 1,
        _ => true,
    };
    if let Some(c) = candidates.iter().find(|c| title_eq(c) && year_ok(c)) {
        return Some((c, "auto"));
    }
    // Franchise-prefixed rips: "Indiana Jones and the Raiders of the
    // Lost Ark" vs TMDB's "Raiders of the Lost Ark" — the local title
    // ends with the candidate's (or vice versa). Weak, first hit wins
    // (TMDB relevance order).
    if let Some(c) = candidates.iter().find(|c| {
        let ct = fold(&c.title);
        ct.len() >= 10 && (norm.ends_with(&ct) || ct.ends_with(&norm)) && year_ok(c)
    }) {
        return Some((c, "weak"));
    }
    // Single plausible hit: accept weakly (release-name noise, subtitles
    // in titles). Multiple hits without a title match = too ambiguous.
    match candidates {
        [only] if year_ok(only) => Some((only, "weak")),
        _ => None,
    }
}

/// Diacritic + number-word folding on top of kahawai's normalization:
/// rips say "Leon" and "12 Monkeys", TMDB says "Léon" and "Twelve
/// Monkeys" — same films.
pub(crate) fn fold(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    const WORDS: &[(&str, &str)] = &[
        ("zero", "0"), ("one", "1"), ("two", "2"), ("three", "3"), ("four", "4"),
        ("five", "5"), ("six", "6"), ("seven", "7"), ("eight", "8"), ("nine", "9"),
        ("ten", "10"), ("eleven", "11"), ("twelve", "12"), ("thirteen", "13"),
        ("fourteen", "14"), ("fifteen", "15"), ("sixteen", "16"), ("seventeen", "17"),
        ("eighteen", "18"), ("nineteen", "19"), ("twenty", "20"),
    ];
    let s = s.replace(['&', '+'], " and ");
    let base: String = kahawai_core::names::normalize_title(&s)
        .nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect();
    const ROMAN: &[(&str, &str)] = &[
        ("ii", "2"), ("iii", "3"), ("iv", "4"), ("vi", "6"), ("vii", "7"),
        ("viii", "8"), ("ix", "9"), ("xi", "11"), ("xii", "12"), ("xiii", "13"),
    ];
    base.split_whitespace()
        .map(|w| {
            WORDS
                .iter()
                .chain(ROMAN.iter())
                .find(|(word, _)| *word == w)
                .map(|(_, d)| *d)
                .unwrap_or(w)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Enricher {
    pub fn new(data_dir: std::path::PathBuf) -> Self {
        let http = reqwest::Client::builder()
            .user_agent("kahawai")
            .build()
            .expect("http client");
        Self {
            anilist: crate::anime::Anilist::new(http.clone()),
            data_dir,
            http,
            running: AtomicBool::new(false),
            progress: Default::default(),
        }
    }

    pub fn status(&self) -> serde_json::Value {
        serde_json::json!({
            "running": self.running.load(Ordering::SeqCst),
            "matched": self.progress.0.load(Ordering::SeqCst),
            "weak": self.progress.1.load(Ordering::SeqCst),
            "missed": self.progress.2.load(Ordering::SeqCst),
        })
    }

    async fn search(&self, key: &str, kind: &str, title: &str, year: Option<i64>) -> Result<Vec<Candidate>> {
        let endpoint = if kind == "movie" { "movie" } else { "tv" };
        let mut req = self
            .http
            .get(format!("https://api.themoviedb.org/3/search/{endpoint}"))
            .query(&[("query", title), ("include_adult", "false")]);
        // Both TMDB credential styles work: the v3 "API Key" rides the
        // api_key query param, the v4 "API Read Access Token" (a JWT,
        // starts with "eyJ") rides a Bearer header.
        if key.starts_with("eyJ") {
            req = req.bearer_auth(key);
        } else {
            req = req.query(&[("api_key", key)]);
        }
        if let (Some(y), "movie") = (year, endpoint) {
            req = req.query(&[("year", y.to_string())]);
        }
        let resp = req.send().await.context("tmdb request")?;
        anyhow::ensure!(
            resp.status() != reqwest::StatusCode::UNAUTHORIZED,
            "TMDB rejected the API key"
        );
        let resp = resp.error_for_status().context("tmdb response")?;
        Ok(resp.json::<SearchResponse>().await.context("tmdb json")?.results)
    }

    /// TheTVDB v4: login yields a bearer token (valid for weeks; we
    /// fetch one per run).
    async fn tvdb_login(&self, key: &str, pin: Option<&str>) -> Result<String> {
        #[derive(Deserialize)]
        struct LoginData {
            token: String,
        }
        #[derive(Deserialize)]
        struct LoginResp {
            data: LoginData,
        }
        let mut body = serde_json::json!({ "apikey": key });
        if let Some(pin) = pin {
            body["pin"] = serde_json::json!(pin);
        }
        let resp = self
            .http
            .post("https://api4.thetvdb.com/v4/login")
            .json(&body)
            .send()
            .await
            .context("tvdb login request")?
            .error_for_status()
            .context("tvdb login rejected (key/pin?)")?;
        Ok(resp.json::<LoginResp>().await.context("tvdb login json")?.data.token)
    }

    async fn tvdb_search(
        &self,
        token: &str,
        kind: &str,
        title: &str,
    ) -> Result<Vec<Candidate>> {
        #[derive(Deserialize)]
        struct SearchResult {
            #[serde(default)]
            tvdb_id: Option<String>,
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            year: Option<String>,
            #[serde(default)]
            image_url: Option<String>,
            #[serde(default)]
            overview: Option<String>,
        }
        #[derive(Deserialize)]
        struct SearchResp {
            #[serde(default)]
            data: Vec<SearchResult>,
        }
        let media_type = if kind == "movie" { "movie" } else { "series" };
        let resp = self
            .http
            .get("https://api4.thetvdb.com/v4/search")
            .bearer_auth(token)
            .query(&[("query", title), ("type", media_type), ("limit", "10")])
            .send()
            .await
            .context("tvdb search")?
            .error_for_status()?
            .json::<SearchResp>()
            .await
            .context("tvdb search json")?;
        Ok(resp
            .data
            .into_iter()
            .filter_map(|r| {
                Some(Candidate {
                    id: r.tvdb_id.as_deref()?.parse().ok()?,
                    title: r.name?,
                    original_title: None,
                    overview: r.overview,
                    // Absolute URL: the artwork store fetches it as-is.
                    poster_path: r.image_url,
                    vote_average: None,
                    release_date: r.year.map(|y| format!("{y}-01-01")),
                })
            })
            .collect())
    }

    /// One episode's provider data, normalized across TMDB/TVDB.
    async fn tmdb_season(
        &self,
        key: &str,
        show_id: &str,
        season: i64,
    ) -> Result<Vec<EpisodeData>> {
        #[derive(Deserialize)]
        struct Ep {
            episode_number: i64,
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            overview: Option<String>,
            #[serde(default)]
            still_path: Option<String>,
            #[serde(default)]
            air_date: Option<String>,
            #[serde(default)]
            vote_average: Option<f64>,
            id: u64,
        }
        #[derive(Deserialize)]
        struct Season {
            #[serde(default)]
            episodes: Vec<Ep>,
        }
        let mut req = self
            .http
            .get(format!("https://api.themoviedb.org/3/tv/{show_id}/season/{season}"));
        if key.starts_with("eyJ") {
            req = req.bearer_auth(key);
        } else {
            req = req.query(&[("api_key", key)]);
        }
        let s: Season = req.send().await?.error_for_status()?.json().await?;
        Ok(s.episodes
            .into_iter()
            .map(|e| EpisodeData {
                provider_id: e.id.to_string(),
                season: Some(season),
                episode: e.episode_number,
                absolute: None,
                title: e.name,
                overview: e.overview,
                image: e.still_path, // relative: poster pipeline prefixes
                aired: e.air_date,
                rating: e.vote_average,
            })
            .collect())
    }

    /// TMDB show's season list: (season_number, episode_count) — used to
    /// map absolute numbering onto seasons cumulatively.
    async fn tmdb_seasons(&self, key: &str, show_id: &str) -> Result<Vec<(i64, i64)>> {
        #[derive(Deserialize)]
        struct S {
            season_number: i64,
            #[serde(default)]
            episode_count: i64,
        }
        #[derive(Deserialize)]
        struct Show {
            #[serde(default)]
            seasons: Vec<S>,
        }
        let mut req = self.http.get(format!("https://api.themoviedb.org/3/tv/{show_id}"));
        if key.starts_with("eyJ") {
            req = req.bearer_auth(key);
        } else {
            req = req.query(&[("api_key", key)]);
        }
        let s: Show = req.send().await?.error_for_status()?.json().await?;
        Ok(s.seasons
            .into_iter()
            .filter(|s| s.season_number > 0)
            .map(|s| (s.season_number, s.episode_count))
            .collect())
    }

    /// TVDB episodes in a given order ("default" or "absolute"), all pages.
    async fn tvdb_episodes(
        &self,
        token: &str,
        series_id: &str,
        order: &str,
    ) -> Result<Vec<EpisodeData>> {
        #[derive(Deserialize)]
        struct Ep {
            id: u64,
            #[serde(default)]
            #[serde(rename = "seasonNumber")]
            season_number: Option<i64>,
            #[serde(default)]
            number: Option<i64>,
            #[serde(default)]
            #[serde(rename = "absoluteNumber")]
            absolute_number: Option<i64>,
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            overview: Option<String>,
            #[serde(default)]
            image: Option<String>,
            #[serde(default)]
            aired: Option<String>,
        }
        #[derive(Deserialize)]
        struct Data {
            #[serde(default)]
            episodes: Vec<Ep>,
        }
        #[derive(Deserialize)]
        struct Resp {
            data: Data,
        }
        let mut out = Vec::new();
        for page in 0..20 {
            let resp = self
                .http
                .get(format!(
                    "https://api4.thetvdb.com/v4/series/{series_id}/episodes/{order}"
                ))
                .bearer_auth(token)
                .query(&[("page", page.to_string())])
                .send()
                .await?;
            if !resp.status().is_success() {
                break;
            }
            let r: Resp = resp.json().await?;
            if r.data.episodes.is_empty() {
                break;
            }
            out.extend(r.data.episodes.into_iter().map(|e| EpisodeData {
                provider_id: e.id.to_string(),
                season: e.season_number,
                episode: e.number.unwrap_or(0),
                absolute: e.absolute_number,
                title: e.name,
                overview: e.overview,
                image: e.image, // absolute URL
                aired: e.aired,
                rating: None,
            }));
        }
        Ok(out)
    }

    /// Enrich every movie/show item without metadata. Returns
    /// (matched, weak, missed); a second concurrent call is a no-op.
    pub async fn run_once(self: &Arc<Self>, registry: &Registry) -> Result<(usize, usize, usize)> {
        if self.running.swap(true, Ordering::SeqCst) {
            anyhow::bail!("enrichment already running");
        }
        let result = self.run_inner(registry).await;
        self.running.store(false, Ordering::SeqCst);
        result
    }

    async fn run_inner(self: &Arc<Self>, registry: &Registry) -> Result<(usize, usize, usize)> {
        let Some(key) = registry.get_setting(TMDB_KEY_SETTING).await? else {
            anyhow::bail!("no TMDB API key configured");
        };
        // TVDB is the backup resolver: only consulted when the TMDB
        // ladder comes up empty, same strict verifier.
        let tvdb_token = match registry.get_setting(TVDB_KEY_SETTING).await? {
            Some(tk) => {
                let pin = registry.get_setting(TVDB_PIN_SETTING).await?;
                match self.tvdb_login(&tk, pin.as_deref()).await {
                    Ok(tok) => Some(std::sync::Arc::new(tok)),
                    Err(e) => {
                        tracing::warn!(error = format!("{e:#}"), "TVDB login failed; skipping");
                        None
                    }
                }
            }
            None => None,
        };
        for c in [&self.progress.0, &self.progress.1, &self.progress.2] {
            c.store(0, Ordering::SeqCst);
        }
        let items = sqlx::query(
            "SELECT i.id, i.kind, i.title, i.year,
                    (SELECT s.path_rel FROM item_sources s
                     WHERE s.item_id = i.id LIMIT 1) AS src_path
             FROM items i
             LEFT JOIN item_metadata m ON m.item_id = i.id
             WHERE i.kind IN ('movie', 'show')
               AND (m.item_id IS NULL OR m.confidence = 'miss')
             ORDER BY i.title",
        )
        .fetch_all(registry.db())
        .await?;
        if items.is_empty() {
            return Ok((0, 0, 0));
        }
        tracing::info!(items = items.len(), "enrichment run starting");

        let sem = Arc::new(tokio::sync::Semaphore::new(4));
        let mut tasks = tokio::task::JoinSet::new();
        for row in items {
            let (id, kind, title, year) = (
                row.get::<String, _>("id"),
                row.get::<String, _>("kind"),
                row.get::<String, _>("title"),
                row.get::<Option<i64>, _>("year"),
            );
            // A movie in its own subdirectory carries a second identity:
            // the directory name, often cleaner than the release-junk
            // filename ("Hellraiser - Revelations (2011)/Hellraiser.
            // VIIII.Revelations…"). Used as an alternative match key.
            let alt = (kind == "movie")
                .then(|| row.get::<Option<String>, _>("src_path"))
                .flatten()
                .and_then(|p| {
                    let (dirs, _) = p.rsplit_once('/')?;
                    let dir = dirs.rsplit('/').next()?;
                    let g = kahawai_core::names::parse_movie(dir);
                    (!g.title.is_empty() && fold(&g.title) != fold(&title)).then_some(g)
                });
            let this = self.clone();
            let key = key.clone();
            let alt = alt.clone();
            let tvdb_token = tvdb_token.clone();
            let db = registry.db().clone();
            let sem = sem.clone();
            tasks.spawn(async move {
                let _permit = sem.acquire().await;
                // Query ladder: TMDB's search has holes (a literal "And"
                // finds nothing where "&" or a shortened query hits), so
                // retry with variants — the strict verifier still judges
                // every candidate against the FULL local title.
                let mut variants = vec![title.clone()];
                if title.contains(" And ") || title.contains(" and ") {
                    variants.push(title.replace(" And ", " & ").replace(" and ", " & "));
                }
                if title.contains('&') {
                    variants.push(title.replace('&', "and"));
                }
                let words: Vec<&str> = title.split_whitespace().collect();
                if words.len() > 3 {
                    variants.push(words[..words.len() - 1].join(" "));
                    variants.push(words[..words.len() - 2].join(" "));
                }
                let mut picked_owned: Option<(Candidate, &'static str)> = None;
                for (vi, q) in variants.iter().enumerate() {
                    match this.search(&key, &kind, q, year).await {
                        Ok(cands) => {
                            if let Some((c, conf)) = pick_candidate(&cands, &title, year) {
                                picked_owned = Some((c.clone(), conf));
                                if vi > 0 {
                                    tracing::debug!(title, variant = %q, "matched via query variant");
                                }
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(title, error = format!("{e:#}"), "tmdb search failed");
                            return;
                        }
                    }
                }
                let mut picked = picked_owned;
                let mut provider = "tmdb";
                if picked.is_none()
                    && let Some(alt) = &alt
                {
                    let alt_year = alt.year.map(|y| y as i64).or(year);
                    match this.search(&key, &kind, &alt.title, alt_year).await {
                        Ok(cands) => {
                            if let Some((c, conf)) = pick_candidate(&cands, &alt.title, alt_year)
                            {
                                picked = Some((c.clone(), conf));
                                tracing::debug!(title, alt = %alt.title, "matched via directory name");
                            }
                        }
                        Err(e) => {
                            tracing::warn!(title, error = format!("{e:#}"), "tmdb alt search failed")
                        }
                    }
                }
                if picked.is_none()
                    && let Some(token) = &tvdb_token
                {
                    match this.tvdb_search(token, &kind, &title, ).await {
                        Ok(cands) => {
                            if let Some((c, conf)) = pick_candidate(&cands, &title, year) {
                                picked = Some((c.clone(), conf));
                                provider = "tvdb";
                                tracing::debug!(title, "matched via TVDB fallback");
                            }
                        }
                        Err(e) => {
                            tracing::warn!(title, error = format!("{e:#}"), "tvdb search failed")
                        }
                    }
                }
                let (provider_id, confidence, c) = match &picked {
                    Some((c, conf)) => (c.id.to_string(), *conf, Some(c)),
                    None => (String::new(), "miss", None),
                };
                match confidence {
                    "auto" => this.progress.0.fetch_add(1, Ordering::SeqCst),
                    "weak" => this.progress.1.fetch_add(1, Ordering::SeqCst),
                    _ => this.progress.2.fetch_add(1, Ordering::SeqCst),
                };
                let r = sqlx::query(
                    "INSERT INTO item_metadata
                       (item_id, provider, provider_id, title, overview, poster_path,
                        rating, premiered, genres, confidence, updated_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, unixepoch())
                     ON CONFLICT (item_id) DO UPDATE SET
                       provider = excluded.provider,
                       provider_id = excluded.provider_id,
                       title = excluded.title,
                       overview = excluded.overview,
                       poster_path = excluded.poster_path,
                       rating = excluded.rating,
                       premiered = excluded.premiered,
                       confidence = excluded.confidence,
                       updated_at = excluded.updated_at",
                )
                .bind(&id)
                .bind(provider)
                .bind(&provider_id)
                .bind(c.map(|c| c.title.clone()))
                .bind(c.and_then(|c| c.overview.clone()))
                .bind(c.and_then(|c| c.poster_path.clone()))
                .bind(c.and_then(|c| c.vote_average))
                .bind(c.and_then(|c| c.release_date.clone()))
                .bind(confidence)
                .execute(&db)
                .await;
                if let Err(e) = r {
                    tracing::warn!(title, error = %e, "metadata upsert failed");
                }
            });
        }
        while tasks.join_next().await.is_some() {}
        let (m, w, x) = (
            self.progress.0.load(Ordering::SeqCst),
            self.progress.1.load(Ordering::SeqCst),
            self.progress.2.load(Ordering::SeqCst),
        );
        tracing::info!(matched = m, weak = w, missed = x, "enrichment run complete");
        // Anime pass (HUB-29): native identity/metadata for items in
        // anime collections, run before episodes so the mapped TVDB ids
        // feed episode enrichment.
        if let Err(e) = self.enrich_anime(registry).await {
            tracing::warn!(error = format!("{e:#}"), "anime enrichment failed");
        }
        if let Err(e) = self.enrich_episodes(registry, &key, tvdb_token.as_ref()).await {
            tracing::warn!(error = format!("{e:#}"), "episode enrichment failed");
        }
        if let Err(e) = self.enrich_music(registry).await {
            tracing::warn!(error = format!("{e:#}"), "music enrichment failed");
        }
        Ok((m, w, x))
    }

    /// MusicBrainz release-group enrichment for albums: strict fold-
    /// exact title + artist verification, 1 req/s (MB's hard limit),
    /// Cover Art Archive front cover as the poster fallback (local
    /// folder art still wins in the artwork chain).
    async fn enrich_music(self: &Arc<Self>, registry: &Registry) -> Result<()> {
        let albums = sqlx::query(
            "SELECT i.id, i.title, i.artist FROM items i
             LEFT JOIN item_metadata m ON m.item_id = i.id
             WHERE i.kind = 'album' AND i.artist IS NOT NULL
               AND (m.item_id IS NULL OR (m.confidence = 'miss' AND m.updated_at < unixepoch() - 7 * 86400))
             ORDER BY i.title",
        )
        .fetch_all(registry.db())
        .await?;
        if albums.is_empty() {
            return Ok(());
        }
        tracing::info!(albums = albums.len(), "music enrichment starting");
        let (mut matched, mut missed) = (0usize, 0usize);
        for (n, row) in albums.iter().enumerate() {
            let (id, title, artist) = (
                row.get::<String, _>("id"),
                row.get::<String, _>("title"),
                row.get::<String, _>("artist"),
            );
            // MB hard rate limit: one request per second.
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
            match self.musicbrainz_album(&title, &artist).await {
                Ok(Some(rg)) => {
                    matched += 1;
                    sqlx::query(
                        "INSERT OR REPLACE INTO item_metadata
                           (item_id, provider, provider_id, title, overview, poster_path,
                            rating, premiered, genres, confidence, updated_at)
                         VALUES (?, 'musicbrainz', ?, ?, NULL, ?, NULL, ?, ?, 'auto', unixepoch())",
                    )
                    .bind(&id)
                    .bind(&rg.id)
                    .bind(&rg.title)
                    .bind(format!("https://coverartarchive.org/release-group/{}/front-500", rg.id))
                    .bind(&rg.first_release_date)
                    .bind(serde_json::to_string(&rg.genres)?)
                    .execute(registry.db())
                    .await?;
                }
                Ok(None) => {
                    missed += 1;
                    sqlx::query(
                        "INSERT OR REPLACE INTO item_metadata
                           (item_id, provider, provider_id, confidence, updated_at)
                         VALUES (?, 'musicbrainz', '', 'miss', unixepoch())",
                    )
                    .bind(&id)
                    .execute(registry.db())
                    .await?;
                }
                Err(e) => {
                    tracing::warn!(title, error = format!("{e:#}"), "musicbrainz lookup failed")
                }
            }
            if (n + 1) % 100 == 0 {
                tracing::info!(done = n + 1, total = albums.len(), matched, "music enrichment progress");
            }
        }
        tracing::info!(matched, missed, "music enrichment complete");
        Ok(())
    }

    /// Strictly verified release-group search: the fold of title AND
    /// artist must match a candidate exactly — never guess.
    async fn musicbrainz_album(&self, title: &str, artist: &str) -> Result<Option<MbReleaseGroup>> {
        let query = format!("releasegroup:\"{}\" AND artist:\"{}\"",
            title.replace('"', ""), artist.replace('"', ""));
        let encoded: String = query
            .bytes()
            .flat_map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    vec![b as char]
                }
                _ => format!("%{b:02X}").chars().collect(),
            })
            .collect();
        let url = format!(
            "https://musicbrainz.org/ws/2/release-group?query={encoded}&fmt=json&limit=8"
        );
        let resp: serde_json::Value = self
            .http
            .get(&url)
            // MB requires an identifying UA with contact info.
            .header("user-agent", "kahawai/0.1 (https://github.com/iksteen/kahawai)")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let groups = resp["release-groups"].as_array().cloned().unwrap_or_default();
        let want_title = fold(title);
        let want_artist = fold(artist);
        for g in &groups {
            let gtitle = g["title"].as_str().unwrap_or_default();
            if fold(gtitle) != want_title {
                continue;
            }
            let artist_ok = g["artist-credit"]
                .as_array()
                .map(|credits| {
                    credits.iter().any(|c| {
                        c["name"].as_str().map(|n| fold(n) == want_artist).unwrap_or(false)
                            || c["artist"]["name"]
                                .as_str()
                                .map(|n| fold(n) == want_artist)
                                .unwrap_or(false)
                    })
                })
                .unwrap_or(false);
            if !artist_ok {
                continue;
            }
            let genres: Vec<String> = g["tags"]
                .as_array()
                .map(|t| {
                    t.iter()
                        .filter_map(|x| x["name"].as_str().map(str::to_string))
                        .take(5)
                        .collect()
                })
                .unwrap_or_default();
            return Ok(Some(MbReleaseGroup {
                id: g["id"].as_str().unwrap_or_default().to_string(),
                title: gtitle.to_string(),
                first_release_date: g["first-release-date"].as_str().map(str::to_string),
                genres,
            }));
        }
        Ok(None)
    }

    /// HUB-29: AniDB titles dump → aid (identity), anime-lists mapping
    /// → AniList/TVDB/TMDB ids, AniList → description/cover/relations.
    /// Conservative like everything else: only fold-exact identities
    /// are accepted; anything else keeps its generic-provider metadata.
    async fn enrich_anime(self: &Arc<Self>, registry: &Registry) -> Result<()> {
        let items = sqlx::query(
            "SELECT DISTINCT i.id, i.kind, i.title, i.year,
                    m.provider, m.provider_id, m.confidence FROM items i
             JOIN item_sources s ON s.item_id = i.id
                OR s.item_id IN (SELECT id FROM items WHERE parent_id = i.id)
             JOIN collections c ON (c.module_id, c.collection_id)
                                 = (s.module_id, s.collection_id)
             LEFT JOIN item_metadata m ON m.item_id = i.id
             WHERE c.media_type = 'anime' AND i.kind IN ('movie', 'show')
               AND (m.item_id IS NULL OR m.anilist_id IS NULL)
               AND (m.confidence IS NULL OR m.confidence != 'rejected')
             ORDER BY i.title",
        )
        .fetch_all(registry.db())
        .await?;
        if items.is_empty() {
            return Ok(());
        }
        tracing::info!(items = items.len(), "anime enrichment starting");
        let titles = crate::anime::AnidbTitles::load(&self.http, &self.data_dir).await?;
        let lists = crate::anime::AnimeLists::load(&self.http, &self.data_dir).await?;
        // Gold path (HUB-30): FILE-by-ED2K when the admin configured an
        // AniDB account. One session for the whole pass.
        let mut anidb = match (
            registry.get_setting(crate::anidb::USER_SETTING).await?,
            registry.get_setting(crate::anidb::PASS_SETTING).await?,
        ) {
            (Some(user), Some(pass)) if !user.is_empty() && !pass.is_empty() => {
                let key = registry
                    .get_setting(crate::anidb::APIKEY_SETTING)
                    .await?
                    .filter(|k| !k.is_empty());
                match crate::anidb::Anidb::login(&user, &pass, key.as_deref()).await {
                    Ok(c) => Some(c),
                    Err(e) => {
                        tracing::warn!(error = format!("{e:#}"), "anidb login failed; title matching only");
                        None
                    }
                }
            }
            _ => None,
        };

        let mut done = 0usize;
        for row in items {
            let (id, kind, title, year) = (
                row.get::<String, _>("id"),
                row.get::<String, _>("kind"),
                row.get::<String, _>("title"),
                row.get::<Option<i64>, _>("year"),
            );
            let existing = row
                .get::<Option<String>, _>("provider")
                .zip(row.get::<Option<String>, _>("provider_id"));
            let manual = row.get::<Option<String>, _>("confidence").as_deref() == Some("manual");
            // ED2K-exact identity first: the file IS the identity, no
            // name heuristics involved. Cached in ed2k_aid so a file is
            // never asked about twice across runs.
            let mut exact_aid: Option<u32> = None;
            if let Some(client) = anidb.as_mut() {
                match self.anidb_identify(registry, client, &id).await {
                    Ok(aid) => exact_aid = aid,
                    Err(e) => {
                        tracing::warn!(error = format!("{e:#}"), "anidb lookup failed; disabling for this run");
                        anidb = None;
                    }
                }
            }
            match self
                .anime_one(
                    registry, &titles, &lists, &id, &kind, &title, year, existing, manual,
                    exact_aid,
                )
                .await
            {
                Ok(true) => done += 1,
                Ok(false) => tracing::debug!(title, "no anime identity; keeping generic metadata"),
                Err(e) => tracing::warn!(title, error = format!("{e:#}"), "anime enrichment error"),
            }
        }
        if let Some(client) = anidb {
            client.logout().await;
        }
        tracing::info!(matched = done, "anime enrichment complete");
        Ok(())
    }

    /// Resolve an item's AniDB id from a representative file's ED2K
    /// hash. Results (hits AND misses) are persisted per content in the
    /// ed2k_aid table — AniDB is never asked twice for the same hash.
    async fn anidb_identify(
        &self,
        registry: &Registry,
        client: &mut crate::anidb::Anidb,
        item_id: &str,
    ) -> Result<Option<u32>> {
        let Some(row) = sqlx::query(
            "SELECT f.ed2k, f.size FROM files f
             JOIN item_sources s ON (s.module_id, s.collection_id, s.path_rel)
                                  = (f.module_id, f.collection_id, f.path_rel)
             WHERE f.ed2k IS NOT NULL
               AND (s.item_id = ?1
                    OR s.item_id IN (SELECT id FROM items WHERE parent_id = ?1))
             ORDER BY s.path_rel LIMIT 1",
        )
        .bind(item_id)
        .fetch_optional(registry.db())
        .await?
        else {
            return Ok(None);
        };
        let (ed2k, size) = (row.get::<String, _>("ed2k"), row.get::<i64, _>("size") as u64);

        if let Some(cached) = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT aid FROM ed2k_aid WHERE ed2k = ?",
        )
        .bind(&ed2k)
        .fetch_optional(registry.db())
        .await?
        {
            return Ok(cached.map(|a| a as u32));
        }
        let hit = client.file_by_ed2k(size, &ed2k).await?;
        let aid = hit.as_ref().map(|h| h.aid);
        if let Some(h) = &hit {
            tracing::info!(aid = h.aid, epno = %h.epno, group = %h.group_name,
                "anidb exact file identification");
        }
        sqlx::query("INSERT OR REPLACE INTO ed2k_aid (ed2k, aid, updated_at) VALUES (?, ?, unixepoch())")
            .bind(&ed2k)
            .bind(aid)
            .execute(registry.db())
            .await?;
        Ok(aid)
    }

    #[allow(clippy::too_many_arguments)]
    async fn anime_one(
        self: &Arc<Self>,
        registry: &Registry,
        titles: &crate::anime::AnidbTitles,
        lists: &crate::anime::AnimeLists,
        item_id: &str,
        kind: &str,
        title: &str,
        year: Option<i64>,
        existing: Option<(String, String)>,
        manual: bool,
        exact_aid: Option<u32>,
    ) -> Result<bool> {
        // ED2K-exact identification outranks every heuristic — the
        // hash is the file. Then: an already-verified generic match,
        // reverse-mapped — adopts anime ids without re-deciding WHO the
        // item is. Falls back to the titles dump; manual matches never
        // fall back (the admin's identity choice is pinned).
        if let Some(aid) = exact_aid
            && let Some(m) = lists.by_anidb(aid)
            && let Some(anilist_id) = m.anilist_id
            && let Some(media) = self.anilist.media_by_id(anilist_id).await?
        {
            self.store_anime(registry, item_id, kind, &media, Some(aid), Some(m)).await?;
            tracing::info!(title, anilist = media.id, anidb = aid, "anime matched (ed2k exact)");
            return Ok(true);
        }
        let reverse: Vec<u32> = existing
            .as_ref()
            .map(|(p, pid)| lists.reverse(p, pid))
            .unwrap_or_default();
        let mut picks: Vec<(u32, &crate::anime::Mapping)> = reverse
            .iter()
            .filter_map(|aid| lists.by_anidb(*aid).map(|m| (*aid, m)))
            .filter(|(_, m)| crate::anime::format_fits(kind, m.kind.as_deref()))
            .filter(|(_, m)| m.anilist_id.is_some())
            .collect();
        if picks.is_empty() && manual {
            return Ok(false);
        }
        if picks.is_empty() {
            // Identity: AniDB titles dump, format-checked via the mapping.
            picks = titles
                .candidates(title)
                .into_iter()
                .filter_map(|aid| lists.by_anidb(aid).map(|m| (aid, m)))
                .filter(|(_, m)| crate::anime::format_fits(kind, m.kind.as_deref()))
                .filter(|(_, m)| m.anilist_id.is_some())
                .collect();
        }

        let (media, anidb_id) = if picks.is_empty() {
            // Fallback: AniList search, accepted only on a fold-exact
            // title (romaji or english) and a fitting format.
            let found = self.anilist.search(title).await?;
            let media = found.into_iter().find(|m| {
                crate::anime::format_fits(kind, m.format.as_deref())
                    && (m.title.romaji.as_deref().map(fold) == Some(fold(title))
                        || m.title.english.as_deref().map(fold) == Some(fold(title)))
            });
            let Some(media) = media else { return Ok(false) };
            (media, None)
        } else {
            // Ambiguity: same title as TV + movie etc — use the year
            // when we have one, else take the dump's best-ranked pick.
            let mut chosen: Option<(u32, crate::anime::AnilistMedia)> = None;
            for (aid, m) in picks.drain(..).take(3) {
                let Some(media) = self.anilist.media_by_id(m.anilist_id.unwrap()).await? else {
                    continue;
                };
                let fits_year = match (year, media.start_date.as_ref().and_then(|d| d.year)) {
                    (Some(y), Some(my)) => (y - i64::from(my)).abs() <= 1,
                    _ => true,
                };
                if fits_year {
                    chosen = Some((aid, media));
                    break;
                }
                if chosen.is_none() {
                    chosen = Some((aid, media)); // fallback: best-ranked
                }
            }
            let Some((aid, media)) = chosen else { return Ok(false) };
            (media, Some(aid))
        };

        let mapping = anidb_id.and_then(|aid| lists.by_anidb(aid));
        self.store_anime(registry, item_id, kind, &media, anidb_id, mapping).await?;
        tracing::info!(title, anilist = media.id, anidb = anidb_id, "anime matched");
        Ok(true)
    }

    /// Persist an AniList match: metadata upsert + relations graph.
    async fn store_anime(
        &self,
        registry: &Registry,
        item_id: &str,
        kind: &str,
        media: &crate::anime::AnilistMedia,
        anidb_id: Option<u32>,
        mapping: Option<&crate::anime::Mapping>,
    ) -> Result<()> {
        let poster = media
            .cover_image
            .as_ref()
            .and_then(|c| c.extra_large.clone().or_else(|| c.large.clone()));
        let genres = media.genres.clone().unwrap_or_default();
        sqlx::query(
            "INSERT OR REPLACE INTO item_metadata
               (item_id, provider, provider_id, title, overview, poster_path, rating,
                premiered, genres, confidence, updated_at,
                anidb_id, anilist_id, mapped_tvdb, mapped_tmdb)
             VALUES (?, 'anilist', ?, ?, ?, ?, ?, ?, ?, 'auto', unixepoch(), ?, ?, ?, ?)",
        )
        .bind(item_id)
        .bind(media.id.to_string())
        .bind(media.display_title())
        .bind(media.plain_description())
        .bind(&poster)
        .bind(media.average_score.map(|s| s / 10.0))
        .bind(media.premiered())
        .bind(serde_json::to_string(&genres)?)
        .bind(anidb_id)
        .bind(media.id)
        .bind(mapping.and_then(|m| m.tvdb_id))
        .bind(mapping.and_then(|m| m.tmdb_for(kind)))
        .execute(registry.db())
        .await?;

        // Relations graph → watch-order building blocks. Watchable
        // relation kinds only; adaptations point at manga.
        sqlx::query("DELETE FROM item_relations WHERE from_item = ?")
            .bind(item_id)
            .execute(registry.db())
            .await?;
        if let Some(rel) = &media.relations {
            for edge in &rel.edges {
                let (Some(kind_raw), Some(node)) = (&edge.relation_type, &edge.node) else {
                    continue;
                };
                let keep = matches!(
                    kind_raw.as_str(),
                    "SEQUEL" | "PREQUEL" | "SIDE_STORY" | "ALTERNATIVE" | "PARENT"
                        | "FULL_STORY" | "SUMMARY" | "SPIN_OFF"
                );
                if !keep || node.format.as_deref() == Some("MUSIC") {
                    continue;
                }
                sqlx::query(
                    "INSERT OR IGNORE INTO item_relations
                       (from_item, kind, target_anilist, target_title)
                     VALUES (?, ?, ?, ?)",
                )
                .bind(item_id)
                .bind(kind_raw.to_lowercase())
                .bind(node.id)
                .bind(node.title.english.clone().or_else(|| node.title.romaji.clone()))
                .execute(registry.db())
                .await?;
            }
        }
        Ok(())
    }

    /// Phase two: episode-level metadata for every matched show that
    /// still has metadata-less episodes. Seasoned shows map by
    /// (season, episode); absolute-numbered shows (anime) use TVDB's
    /// absolute order when the show matched there, else TMDB seasons
    /// concatenated cumulatively.
    async fn enrich_episodes(
        self: &Arc<Self>,
        registry: &Registry,
        tmdb_key: &str,
        tvdb_token: Option<&std::sync::Arc<String>>,
    ) -> Result<()> {
        let shows = sqlx::query(
            "SELECT i.id, m.provider, m.provider_id, m.mapped_tvdb, m.mapped_tmdb
             FROM items i
             JOIN item_metadata m ON m.item_id = i.id
             WHERE i.kind = 'show' AND m.provider_id != ''
               AND m.confidence != 'rejected'
               AND EXISTS (
                 SELECT 1 FROM items e
                 LEFT JOIN item_metadata em ON em.item_id = e.id
                 WHERE e.parent_id = i.id AND em.item_id IS NULL)",
        )
        .fetch_all(registry.db())
        .await?;
        if shows.is_empty() {
            return Ok(());
        }
        tracing::info!(shows = shows.len(), "episode enrichment starting");
        let sem = Arc::new(tokio::sync::Semaphore::new(3));
        let mut tasks = tokio::task::JoinSet::new();
        for row in shows {
            let (show_id, mut provider, mut pid) = (
                row.get::<String, _>("id"),
                row.get::<String, _>("provider"),
                row.get::<String, _>("provider_id"),
            );
            // Anime shows carry mapped TVDB/TMDB ids (HUB-29): episode
            // data still comes from those providers.
            if provider == "anilist" {
                if let Some(tvdb) = row.get::<Option<i64>, _>("mapped_tvdb") {
                    provider = "tvdb".into();
                    pid = tvdb.to_string();
                } else if let Some(tmdb) = row.get::<Option<i64>, _>("mapped_tmdb") {
                    provider = "tmdb".into();
                    pid = tmdb.to_string();
                } else {
                    continue; // no episode source for this one
                }
            }
            let this = self.clone();
            let key = tmdb_key.to_string();
            let token = tvdb_token.cloned();
            let db = registry.db().clone();
            let sem = sem.clone();
            tasks.spawn(async move {
                let _permit = sem.acquire().await;
                if let Err(e) =
                    this.enrich_show_episodes(&db, &show_id, &provider, &pid, &key, token.as_deref())
                        .await
                {
                    tracing::warn!(show = %show_id, error = format!("{e:#}"), "episode fetch failed");
                }
            });
        }
        while tasks.join_next().await.is_some() {}
        tracing::info!("episode enrichment complete");
        Ok(())
    }

    async fn enrich_show_episodes(
        &self,
        db: &sqlx::SqlitePool,
        show_id: &str,
        provider: &str,
        pid: &str,
        tmdb_key: &str,
        tvdb_token: Option<&String>,
    ) -> Result<()> {
        // Our episode items: (item_id, season, episode). season NULL =
        // absolute numbering.
        let eps = sqlx::query(
            "SELECT id, season, episode FROM items WHERE parent_id = ? AND kind = 'episode'",
        )
        .bind(show_id)
        .fetch_all(db)
        .await?;
        let absolute = eps.iter().any(|r| r.get::<Option<i64>, _>("season").is_none());

        // Provider episodes, keyed however our items are keyed.
        let mut by_key: std::collections::HashMap<(Option<i64>, i64), EpisodeData> =
            Default::default();
        match (provider, absolute) {
            ("tvdb", false) => {
                let token = tvdb_token.context("tvdb-matched show but no tvdb token")?;
                for e in self.tvdb_episodes(token, pid, "default").await? {
                    if let (Some(s), n) = (e.season, e.episode) {
                        by_key.insert((Some(s), n), e);
                    }
                }
            }
            ("tvdb", true) => {
                let token = tvdb_token.context("tvdb-matched show but no tvdb token")?;
                let eps_abs = self.tvdb_episodes(token, pid, "absolute").await?;
                for (i, e) in eps_abs.into_iter().enumerate() {
                    let n = e.absolute.unwrap_or(i as i64 + 1);
                    by_key.insert((None, n), e);
                }
            }
            (_, false) => {
                let seasons: Vec<i64> = eps
                    .iter()
                    .filter_map(|r| r.get::<Option<i64>, _>("season"))
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                for s in seasons {
                    match self.tmdb_season(tmdb_key, pid, s).await {
                        Ok(list) => {
                            for e in list {
                                by_key.insert((Some(s), e.episode), e);
                            }
                        }
                        Err(e) => tracing::debug!(show = pid, season = s, error = %e, "season fetch failed"),
                    }
                }
            }
            (_, true) => {
                // Absolute over TMDB: concatenate seasons in order.
                let seasons = self.tmdb_seasons(tmdb_key, pid).await?;
                let max_abs = eps
                    .iter()
                    .map(|r| r.get::<i64, _>("episode"))
                    .max()
                    .unwrap_or(0);
                let mut fetched: std::collections::HashMap<i64, Vec<EpisodeData>> =
                    Default::default();
                for abs in 1..=max_abs {
                    if let Some((s, n)) = absolute_to_seasoned(&seasons, abs) {
                        if !fetched.contains_key(&s) {
                            let list =
                                self.tmdb_season(tmdb_key, pid, s).await.unwrap_or_default();
                            fetched.insert(s, list);
                        }
                        if let Some(e) =
                            fetched[&s].iter().find(|e| e.episode == n).cloned()
                        {
                            by_key.insert((None, abs), e);
                        }
                    }
                }
            }
        }
        if by_key.is_empty() {
            return Ok(());
        }

        let mut wrote = 0;
        for r in &eps {
            let key = (r.get::<Option<i64>, _>("season"), r.get::<i64, _>("episode"));
            let Some(e) = by_key.get(&key) else { continue };
            let item_id: String = r.get("id");
            sqlx::query(
                "INSERT INTO item_metadata
                   (item_id, provider, provider_id, title, overview, poster_path,
                    rating, premiered, genres, confidence, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, 'auto', unixepoch())
                 ON CONFLICT (item_id) DO UPDATE SET
                   provider = excluded.provider,
                   provider_id = excluded.provider_id,
                   title = excluded.title,
                   overview = excluded.overview,
                   poster_path = excluded.poster_path,
                   rating = excluded.rating,
                   premiered = excluded.premiered,
                   updated_at = excluded.updated_at",
            )
            .bind(&item_id)
            .bind(provider)
            .bind(&e.provider_id)
            .bind(e.title.as_deref())
            .bind(e.overview.as_deref())
            .bind(e.image.as_deref())
            .bind(e.rating)
            .bind(e.aired.as_deref())
            .execute(db)
            .await?;
            wrote += 1;
        }
        tracing::info!(show = show_id, episodes = wrote, "episode metadata stored");
        Ok(())
    }

    /// Provider search for the review queue (HUB-8): TMDB first, TVDB
    /// appended when configured. Raw candidates, human judges.
    pub async fn search_candidates(
        &self,
        registry: &Registry,
        kind: &str,
        query: &str,
        year: Option<i64>,
    ) -> Result<serde_json::Value> {
        let mut out: Vec<serde_json::Value> = Vec::new();
        if let Some(key) = registry.get_setting(TMDB_KEY_SETTING).await? {
            match self.search(&key, kind, query, year).await {
                Ok(cands) => out.extend(cands.iter().map(|c| {
                    let mut v = serde_json::to_value(c).unwrap();
                    v["provider"] = serde_json::json!("tmdb");
                    // Absolute preview URL for the admin UI.
                    if let Some(p) = c.poster_path.as_deref() {
                        v["poster_url"] =
                            serde_json::json!(format!("https://image.tmdb.org/t/p/w154{p}"));
                    }
                    v
                })),
                Err(e) => tracing::warn!(error = format!("{e:#}"), "review tmdb search failed"),
            }
        }
        if let Some(tk) = registry.get_setting(TVDB_KEY_SETTING).await? {
            let pin = registry.get_setting(TVDB_PIN_SETTING).await?;
            if let Ok(token) = self.tvdb_login(&tk, pin.as_deref()).await {
                match self.tvdb_search(&token, kind, query).await {
                    Ok(cands) => out.extend(cands.iter().map(|c| {
                        let mut v = serde_json::to_value(c).unwrap();
                        v["provider"] = serde_json::json!("tvdb");
                        if let Some(p) = c.poster_path.as_deref() {
                            v["poster_url"] = serde_json::json!(p);
                        }
                        v
                    })),
                    Err(e) => {
                        tracing::warn!(error = format!("{e:#}"), "review tvdb search failed")
                    }
                }
            }
        }
        Ok(serde_json::json!(out))
    }

    /// Fetch a TMDB poster (used by the artwork store when an item has
    /// no local artwork).
    pub async fn fetch_poster(&self, poster_path: &str) -> Result<Vec<u8>> {
        // TMDB stores relative paths; TVDB image URLs are absolute.
        let url = if poster_path.starts_with("http") {
            poster_path.to_string()
        } else {
            format!("https://image.tmdb.org/t/p/w500{poster_path}")
        };
        let resp = self.http.get(&url).send().await?.error_for_status()?;
        Ok(resp.bytes().await?.to_vec())
    }
}

#[derive(Debug, Clone)]
pub struct EpisodeData {
    pub provider_id: String,
    pub season: Option<i64>,
    pub episode: i64,
    pub absolute: Option<i64>,
    pub title: Option<String>,
    pub overview: Option<String>,
    pub image: Option<String>,
    pub aired: Option<String>,
    pub rating: Option<f64>,
}

/// Map an absolute episode number onto (season, episode) given ordered
/// (season, episode_count) pairs — sequential-numbered shows only, which
/// is exactly the absolute-numbered-fansub convention.
pub fn absolute_to_seasoned(seasons: &[(i64, i64)], absolute: i64) -> Option<(i64, i64)> {
    let mut remaining = absolute;
    for &(season, count) in seasons {
        if count <= 0 {
            continue;
        }
        if remaining <= count {
            return Some((season, remaining));
        }
        remaining -= count;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: u64, title: &str, date: &str) -> Candidate {
        Candidate {
            id,
            title: title.into(),
            original_title: None,
            overview: None,
            poster_path: None,
            vote_average: None,
            release_date: Some(date.into()),
        }
    }

    #[test]
    fn maps_absolute_numbering() {
        let seasons = [(1i64, 12i64), (2, 13), (3, 12)];
        assert_eq!(absolute_to_seasoned(&seasons, 1), Some((1, 1)));
        assert_eq!(absolute_to_seasoned(&seasons, 12), Some((1, 12)));
        assert_eq!(absolute_to_seasoned(&seasons, 13), Some((2, 1)));
        assert_eq!(absolute_to_seasoned(&seasons, 25), Some((2, 13)));
        assert_eq!(absolute_to_seasoned(&seasons, 26), Some((3, 1)));
        assert_eq!(absolute_to_seasoned(&seasons, 38), None); // beyond the end
    }

    #[test]
    fn picks_conservatively() {
        // Exact title + year → auto, even when it's not the first result.
        let cands = vec![cand(1, "Heat Wave", "1995-01-01"), cand(2, "Heat", "1995-12-15")];
        let (c, conf) = pick_candidate(&cands, "Heat", Some(1995)).unwrap();
        assert_eq!((c.id, conf), (2, "auto"));
        // Year mismatch beyond ±1 disqualifies the title match.
        assert!(pick_candidate(&cands[1..], "Heat", Some(2006)).is_none());
        // Single plausible result without title equality → weak.
        let one = vec![cand(3, "Léon: The Professional", "1994-09-14")];
        let (c, conf) = pick_candidate(&one, "Leon", Some(1994)).unwrap();
        assert_eq!((c.id, conf), (3, "weak"));
        // Multiple results, none matching → miss.
        let many = vec![cand(4, "A", "2000-01-01"), cand(5, "B", "2000-01-01")];
        assert!(pick_candidate(&many, "C", None).is_none());
        // Normalized equality: punctuation/case don't matter.
        let lp = vec![cand(6, "Léon: The Professional", "1994-09-14")];
        let (_, conf) = pick_candidate(&lp, "Leon The Professional", None).unwrap();
        assert_eq!(conf, "auto");
        // Number words fold: "12 Monkeys" == "Twelve Monkeys".
        let tm = vec![cand(7, "Twelve Monkeys", "1995-12-29"), cand(8, "12 Rounds", "2009-03-19")];
        let (c, conf) = pick_candidate(&tm, "12 Monkeys", None).unwrap();
        assert_eq!((c.id, conf), (7, "auto"));
        // Roman numerals fold (2+ chars only — "I" and "V" are words).
        let mib = vec![cand(13, "Men in Black II", "2002-07-03")];
        let (c, conf) = pick_candidate(&mib, "Men in Black 2", None).unwrap();
        assert_eq!((c.id, conf), (13, "auto"));
        let vfv = vec![cand(14, "V for Vendetta", "2006-03-15"), cand(15, "5 for Vendetta", "2000-01-01")];
        let (c, _) = pick_candidate(&vfv, "V for Vendetta", None).unwrap();
        assert_eq!(c.id, 14);
        // Acronym spacing: "S H I E L D" == "S.H.I.E.L.D.".
        let sh = vec![cand(12, "Marvel's Agents of S.H.I.E.L.D.", "2013-09-24")];
        let (c, conf) = pick_candidate(&sh, "Marvels Agents of S H I E L D", None).unwrap();
        assert_eq!((c.id, conf), (12, "auto"));
        // '&' and 'and' are the same word.
        let oo = vec![cand(11, "Iliza Shlesinger: Over & Over", "2019-07-02")];
        let (c, conf) = pick_candidate(&oo, "Iliza Shlesinger Over And Over", None).unwrap();
        assert_eq!((c.id, conf), (11, "auto"));
        // Franchise prefix: local title ends with the candidate's → weak.
        let rd = vec![
            cand(9, "Raiders of the Lost Ark", "1981-06-12"),
            cand(10, "The Lost Ark", "2000-01-01"),
        ];
        let (c, conf) =
            pick_candidate(&rd, "Indiana Jones and the Raiders of the Lost Ark", None).unwrap();
        assert_eq!((c.id, conf), (9, "weak"));
    }
}
