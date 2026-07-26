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
    /// Every provider call goes out through this: pacing and
    /// rate-limit backoff live in `gate.rs`, not at the call sites.
    http: std::sync::Arc<crate::gate::Http>,
    data_dir: std::path::PathBuf,
    anilist: crate::anime::Anilist,
    last_nudge: std::sync::atomic::AtomicU64,
    running: AtomicBool,
    /// (matched, weak, missed) of the current/last run.
    progress: (AtomicUsize, AtomicUsize, AtomicUsize),
    /// The UDP session, kept for the PROCESS lifetime — not per run.
    /// A login per enrichment run is what got this client banned twice
    /// in one evening; sessions are cheap to hold and expensive to
    /// re-establish.
    anidb: tokio::sync::Mutex<Option<crate::anidb::Anidb>>,
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
    /// ISO 639-1 from TMDB search; TVDB maps primary_language into it.
    #[serde(default)]
    original_language: Option<String>,
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
        let http = std::sync::Arc::new(crate::gate::Http::new().expect("http client"));
        Self {
            anilist: crate::anime::Anilist::new(http.clone()),
            data_dir,
            http,
            running: AtomicBool::new(false),
            last_nudge: std::sync::atomic::AtomicU64::new(0),
            progress: Default::default(),
            anidb: Default::default(),
        }
    }

    /// Where anime/AniDB state lives (the api's verify path needs it).
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
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
        let resp = self.http.send(req).await.context("tmdb request")?;
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
            .send(self.http.post("https://api4.thetvdb.com/v4/login").json(&body))
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
            #[serde(default)]
            primary_language: Option<String>,
        }
        #[derive(Deserialize)]
        struct SearchResp {
            #[serde(default)]
            data: Vec<SearchResult>,
        }
        let media_type = if kind == "movie" { "movie" } else { "series" };
        let resp = self
            .http
            .send(
                self.http
                    .get("https://api4.thetvdb.com/v4/search")
                    .bearer_auth(token)
                    .query(&[("query", title), ("type", media_type), ("limit", "10")]),
            )
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
                    original_language: r.primary_language,
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
        let s: Season = self.http.send(req).await?.error_for_status()?.json().await?;
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
        let s: Show = self.http.send(req).await?.error_for_status()?.json().await?;
        Ok(s.seasons
            .into_iter()
            .filter(|s| s.season_number > 0)
            .map(|s| (s.season_number, s.episode_count))
            .collect())
    }

    /// TVDB episodes in a given order ("default" or "absolute"), all
    /// pages, with English names/overviews merged over the original-
    /// language base where TVDB has the translation (the lang-scoped
    /// endpoint returns null names for untranslated episodes).
    async fn tvdb_episodes_english(
        &self,
        token: &str,
        series_id: &str,
        order: &str,
    ) -> Result<Vec<EpisodeData>> {
        let mut out = self.tvdb_episodes(token, series_id, order, None).await?;
        let eng = self
            .tvdb_episodes(token, series_id, order, Some("eng"))
            .await
            .unwrap_or_default();
        let by_id: std::collections::HashMap<String, EpisodeData> =
            eng.into_iter().map(|e| (e.provider_id.clone(), e)).collect();
        for e in &mut out {
            if let Some(t) = by_id.get(&e.provider_id) {
                if t.title.is_some() {
                    e.title = t.title.clone();
                }
                if t.overview.is_some() {
                    e.overview = t.overview.clone();
                }
            }
        }
        Ok(out)
    }

    async fn tvdb_episodes(
        &self,
        token: &str,
        series_id: &str,
        order: &str,
        lang: Option<&str>,
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
                .send(
                    self.http
                        .get(match lang {
                            Some(l) => format!(
                                "https://api4.thetvdb.com/v4/series/{series_id}/episodes/{order}/{l}"
                            ),
                            None => format!(
                                "https://api4.thetvdb.com/v4/series/{series_id}/episodes/{order}"
                            ),
                        })
                        .bearer_auth(token)
                        .query(&[("page", page.to_string())]),
                )
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
    /// Debounced auto-run: at most one spawned enrichment per 10 min,
    /// used by hooks like "new ED2K hashes landed".
    pub fn nudge(self: &Arc<Self>, registry: Arc<Registry>) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let last = self.last_nudge.load(Ordering::SeqCst);
        if now.saturating_sub(last) < 600
            || self
                .last_nudge
                .compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            if let Err(e) = this.run_once(&registry).await {
                tracing::debug!(error = format!("{e:#}"), "nudged enrichment skipped");
            }
        });
    }

    pub async fn run_once(self: &Arc<Self>, registry: &Registry) -> Result<(usize, usize, usize)> {
        if self.running.swap(true, Ordering::SeqCst) {
            anyhow::bail!("enrichment already running");
        }
        let result = self.run_inner(registry).await;
        self.running.store(false, Ordering::SeqCst);
        result
    }

    /// Provider chains, per media type (ordered; first identity wins):
    ///   anime : ED2K→AniDB exact > reverse-mapped verified match >
    ///           AniDB titles dump > AniList search > (fallback) TMDB > TVDB
    ///   movies/series : TMDB (ladder + dir alt-key) > TVDB
    ///   music : embedded tags (identity, at resolution) > MusicBrainz
    /// The anime pass runs FIRST so anime items never spend a wasted
    /// TMDB match that would immediately be overwritten; items the anime
    /// chain declines fall through to the generic pass unmatched.
    async fn run_inner(self: &Arc<Self>, registry: &Registry) -> Result<(usize, usize, usize)> {
        registry.emit(serde_json::json!({ "kind": "enrich", "running": true }));
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
        // HUB-5: instantiate the run's providers; chains are declared in
        // providers::chain_for. Unconfigured providers stay absent.
        let mut set = crate::providers::ProviderSet::default();
        set.add(Box::new(TmdbProvider { enricher: self.clone(), key: key.clone() }));
        if let Some(token) = tvdb_token.clone() {
            set.add(Box::new(TvdbProvider { enricher: self.clone(), token }));
        }
        let anime_items = self.select_anime_items(registry).await.unwrap_or_default();
        if !anime_items.is_empty() {
            match self.build_anime_provider(registry).await {
                Ok(p) => set.add(Box::new(p)),
                Err(e) => {
                    tracing::warn!(error = format!("{e:#}"), "anime provider unavailable this run")
                }
            }
        }
        set.add(Box::new(MusicbrainzProvider { enricher: self.clone() }));
        let providers = Arc::new(set);

        // Anime chain first (HUB-29): sequential — its providers pace
        // themselves against AniDB/AniList.
        if let Err(e) = self.enrich_anime(registry, &providers, anime_items).await {
            tracing::warn!(error = format!("{e:#}"), "anime enrichment failed");
        }
        let items = sqlx::query(
            "SELECT i.id, i.kind, i.title, i.year,
                    (SELECT s.path_rel FROM item_sources s
                     WHERE s.item_id = i.id LIMIT 1) AS src_path,
                    -- Movies and series have separate chains (HUB-5), so
                    -- the walk needs each item's OWN media type.
                    c0.media_type AS media_type
             FROM items i
             LEFT JOIN merged_metadata m ON m.item_id = i.id
             LEFT JOIN collections c0 ON (c0.module_id, c0.collection_id) = (
                 SELECT s3.module_id, s3.collection_id FROM item_sources s3
                 WHERE s3.item_id = i.id
                    OR s3.item_id IN (SELECT id FROM items WHERE parent_id = i.id)
                 LIMIT 1)
             WHERE i.kind IN ('movie', 'show')
               AND (m.item_id IS NULL OR m.confidence = 'miss'
                    -- HUB-5: every provider in the chain answers once,
                    -- whatever the order — so an item needs work while
                    -- any of them has never been asked.
                    OR EXISTS (
                      SELECT 1 FROM provider_ranks r
                      WHERE r.media_type = CASE
                              WHEN c0.media_type IN ('movies','series','anime','music')
                              THEN c0.media_type ELSE 'movies' END
                        AND NOT EXISTS (
                          SELECT 1 FROM provider_metadata pm
                          WHERE pm.item_id = i.id
                            AND (pm.provider = r.provider
                                 OR (r.provider = 'anime' AND pm.provider = 'anilist'))))
                    -- or a provider refused and is due again (bans and
                    -- rate limits reschedule, they never drop work).
                    OR EXISTS (
                      SELECT 1 FROM enrichment_queue q
                      WHERE q.item_id = i.id AND q.due_at <= unixepoch()))
               AND NOT EXISTS (
                 SELECT 1 FROM item_sources s2
                 JOIN collections c2 ON (c2.module_id, c2.collection_id)
                                      = (s2.module_id, s2.collection_id)
                 WHERE c2.media_type = 'anime'
                   AND (s2.item_id = i.id
                        OR s2.item_id IN (SELECT id FROM items WHERE parent_id = i.id)))
             ORDER BY i.title",
        )
        .fetch_all(registry.db())
        .await?;
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
            let media_type = crate::providers::media_type_key(
                row.get::<Option<String>, _>("media_type").as_deref().unwrap_or_default(),
            );
            // A movie in its own subdirectory carries a second identity:
            // the directory name, often cleaner than the release-junk
            // filename. Used as an alternative match key.
            let alt = (kind == "movie")
                .then(|| row.get::<Option<String>, _>("src_path"))
                .flatten()
                .and_then(|p| {
                    let (dirs, _) = p.rsplit_once('/')?;
                    let dir = dirs.rsplit('/').next()?;
                    let g = kahawai_core::names::parse_movie(dir);
                    (!g.title.is_empty() && fold(&g.title) != fold(&title)).then_some(g)
                });
            let item = crate::providers::ItemRef {
                id,
                kind,
                title,
                year,
                artist: None,
                alt,
                existing: None,
                manual: false,
                known_aid: None,
                identified: false,
                owner: None,
            };
            let this = self.clone();
            let set = providers.clone();
            let db = registry.db().clone();
            let sem = sem.clone();
            tasks.spawn(async move {
                let _permit = sem.acquire().await;
                match set.run_chain(media_type, &db, &item).await {
                    Some("auto") => {
                        this.progress.0.fetch_add(1, Ordering::SeqCst);
                    }
                    Some("weak") => {
                        this.progress.1.fetch_add(1, Ordering::SeqCst);
                    }
                    Some(_) => {}
                    None => {
                        this.progress.2.fetch_add(1, Ordering::SeqCst);
                        if let Err(e) = this.store_generic(&db, &item.id, "tmdb", None).await {
                            tracing::warn!(title = %item.title, error = %e, "miss upsert failed");
                        }
                    }
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
        registry.emit(serde_json::json!({ "kind": "enrich", "running": false }));
        if let Err(e) = self.enrich_episodes(registry, &key, tvdb_token.as_ref()).await {
            tracing::warn!(error = format!("{e:#}"), "episode enrichment failed");
        }
        if let Err(e) = self.backfill_original_language(registry, &key).await {
            tracing::warn!(error = format!("{e:#}"), "original-language backfill failed");
        }
        if let Err(e) = self.enrich_music(registry, &providers).await {
            tracing::warn!(error = format!("{e:#}"), "music enrichment failed");
        }
        providers.finish().await;
        Ok((m, w, x))
    }

    /// Album enrichment via the music chain (MusicBrainz today).
    async fn enrich_music(
        self: &Arc<Self>,
        registry: &Registry,
        providers: &Arc<crate::providers::ProviderSet>,
    ) -> Result<()> {
        let albums = sqlx::query(
            "SELECT i.id, i.title, i.artist FROM items i
             LEFT JOIN merged_metadata m ON m.item_id = i.id
             WHERE i.kind = 'album' AND i.artist IS NOT NULL
               AND (m.item_id IS NULL
                    OR (m.confidence = 'miss' AND m.updated_at < unixepoch() - 7 * 86400)
                    -- Work the chain still owes: a provider that refused
                    -- and is due again. Without this a rescheduled album
                    -- would sit in the queue forever (HUB-5).
                    OR EXISTS (
                      SELECT 1 FROM enrichment_queue q
                      WHERE q.item_id = i.id AND q.due_at <= unixepoch()))
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
            let item = crate::providers::ItemRef {
                id: row.get("id"),
                kind: "album".into(),
                title: row.get("title"),
                year: None,
                artist: row.get("artist"),
                alt: None,
                existing: None,
                manual: false,
                known_aid: None,
                identified: false,
                owner: None,
            };
            match providers.run_chain("music", registry.db(), &item).await {
                Some(_) => matched += 1,
                None => {
                    missed += 1;
                    crate::providers::store_answer(
                        registry.db(),
                        &item.id,
                        "musicbrainz",
                        "",
                        "miss",
                        Default::default(),
                        &crate::providers::chain_in_force(registry.db(), "music").await,
                    )
                    .await?;
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
        // MB allows one request per second per IP and answers 503 to
        // everything above it; the gate holds us to that, and carries
        // the identifying UA it also requires.
        let resp: serde_json::Value =
            self.http.send(self.http.get(&url)).await?.error_for_status()?.json().await?;
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

    /// Anime items needing the chain: unidentified ones, plus matched
    /// items whose files gained ED2K hashes AniDB hasn't been asked
    /// about — a late hash is canonical and re-verifies a name match.
    async fn select_anime_items(
        &self,
        registry: &Registry,
    ) -> Result<Vec<crate::providers::ItemRef>> {
        let rows = sqlx::query(
            "SELECT DISTINCT i.id, i.kind, i.title, i.year,
                    m.provider, m.provider_id, m.confidence, m.anidb_id, m.anilist_id
             FROM items i
             JOIN item_sources s ON s.item_id = i.id
                OR s.item_id IN (SELECT id FROM items WHERE parent_id = i.id)
             JOIN collections c ON (c.module_id, c.collection_id)
                                 = (s.module_id, s.collection_id)
             LEFT JOIN merged_metadata m ON m.item_id = i.id
             WHERE c.media_type = 'anime' AND i.kind IN ('movie', 'show')
               AND (m.confidence IS NULL OR m.confidence != 'rejected')
               AND (
                 m.item_id IS NULL OR m.anilist_id IS NULL
                 -- HUB-5: the tail answers too, whatever the order —
                 -- so this item needs work while any chain provider has
                 -- never been asked, or one is due again after refusing.
                 OR EXISTS (
                   SELECT 1 FROM provider_ranks r
                   WHERE r.media_type = 'anime'
                     AND NOT EXISTS (
                       SELECT 1 FROM provider_metadata pm
                       WHERE pm.item_id = i.id
                         AND (pm.provider = r.provider
                              OR (r.provider = 'anime' AND pm.provider = 'anilist'))))
                 OR EXISTS (
                   SELECT 1 FROM enrichment_queue q
                   WHERE q.item_id = i.id AND q.due_at <= unixepoch())
                 OR EXISTS (
                   SELECT 1 FROM files f
                   JOIN item_sources s2 ON (s2.module_id, s2.collection_id, s2.path_rel)
                                         = (f.module_id, f.collection_id, f.path_rel)
                   WHERE f.ed2k IS NOT NULL
                     AND (s2.item_id = i.id
                          OR s2.item_id IN (SELECT id FROM items WHERE parent_id = i.id))
                     AND f.ed2k NOT IN (SELECT ed2k FROM ed2k_aid)))
             ORDER BY i.title",
        )
        .fetch_all(registry.db())
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| crate::providers::ItemRef {
                id: row.get("id"),
                kind: row.get("kind"),
                title: row.get("title"),
                year: row.get("year"),
                artist: None,
                alt: None,
                existing: row
                    .get::<Option<String>, _>("provider")
                    .zip(row.get::<Option<String>, _>("provider_id")),
                manual: row.get::<Option<String>, _>("confidence").as_deref() == Some("manual"),
                known_aid: row.get::<Option<i64>, _>("anidb_id").map(|a| a as u32),
                identified: row.get::<Option<i64>, _>("anilist_id").is_some(),
                owner: None,
            })
            .collect())
    }

    /// The anime chain's composite provider: AniDB identity (ED2K, then
    /// reverse mapping, then titles dump) + AniList description.
    async fn build_anime_provider(self: &Arc<Self>, registry: &Registry) -> Result<AnimeProvider> {
        let titles = crate::anime::AnidbTitles::load(&self.http, &self.data_dir).await?;
        let lists = crate::anime::AnimeLists::load(&self.http, &self.data_dir).await?;
        // Reuse the process's session; only authenticate when there
        // isn't one (first run, or the last one went stale).
        if self.anidb.lock().await.is_some() {
            return Ok(AnimeProvider { enricher: self.clone(), titles, lists });
        }
        let anidb = match (
            registry.get_setting(crate::anidb::USER_SETTING).await?,
            registry.get_setting(crate::anidb::PASS_SETTING).await?,
        ) {
            (Some(user), Some(pass)) if !user.is_empty() && !pass.is_empty() => {
                let key = registry
                    .get_setting(crate::anidb::APIKEY_SETTING)
                    .await?
                    .filter(|k| !k.is_empty());
                match crate::anidb::Anidb::login(&self.data_dir, &user, &pass, key.as_deref())
                    .await
                {
                    Ok(c) => Some(c),
                    Err(e) => {
                        tracing::warn!(error = format!("{e:#}"), "anidb login failed; title matching only");
                        None
                    }
                }
            }
            _ => None,
        };
        // Heal rows identified before mapped-id storage existed (or
        // before the mapping knew them): Settled items never re-store,
        // so adopt bridge ids directly from the mapping. Idempotent,
        // no API calls — and it feeds HUB-31's projection backfill.
        let stale = sqlx::query(
            "SELECT item_id, anidb_id, i.kind FROM merged_metadata m
             JOIN items i ON i.id = m.item_id
             WHERE m.anidb_id IS NOT NULL
               AND m.mapped_tvdb IS NULL AND m.mapped_tmdb IS NULL",
        )
        .fetch_all(registry.db())
        .await?;
        for row in stale {
            let aid = row.get::<i64, _>("anidb_id") as u32;
            let Some(m) = lists.by_anidb(aid) else { continue };
            let tmdb = m.tmdb_for(&row.get::<String, _>("kind"));
            if m.tvdb_id.is_none() && tmdb.is_none() {
                continue;
            }
            sqlx::query(
                "UPDATE merged_metadata SET mapped_tvdb = ?, mapped_tmdb = ? WHERE item_id = ?",
            )
            .bind(m.tvdb_id)
            .bind(tmdb)
            .bind(row.get::<String, _>("item_id"))
            .execute(registry.db())
            .await?;
            tracing::info!(aid, tvdb = ?m.tvdb_id, tmdb = ?tmdb, "mapped ids backfilled");
        }
        *self.anidb.lock().await = anidb;
        Ok(AnimeProvider { enricher: self.clone(), titles, lists })
    }

    async fn enrich_anime(
        self: &Arc<Self>,
        registry: &Registry,
        providers: &Arc<crate::providers::ProviderSet>,
        items: Vec<crate::providers::ItemRef>,
    ) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        tracing::info!(items = items.len(), "anime enrichment starting");
        let mut done = 0usize;
        for item in &items {
            match providers.run_chain("anime", registry.db(), item).await {
                Some("settled") => {}
                Some(_) => done += 1,
                None => {
                    tracing::debug!(title = %item.title, "no anime identity; recording miss");
                    if !item.identified {
                        self.store_generic(registry.db(), &item.id, "anime", None).await?;
                    }
                }
            }
        }
        tracing::info!(matched = done, "anime enrichment complete");
        Ok(())
    }

    /// Resolve an item's AniDB id from a representative file's ED2K
    /// hash. Results (hits AND misses) are persisted per content in the
    /// ed2k_aid table — AniDB is never asked twice for the same hash.
    pub(crate) async fn anidb_identify(
        &self,
        db: &sqlx::SqlitePool,
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
        .fetch_optional(db)
        .await?
        else {
            return Ok(None);
        };
        let (ed2k, size) = (row.get::<String, _>("ed2k"), row.get::<i64, _>("size") as u64);

        if let Some(cached) = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT aid FROM ed2k_aid WHERE ed2k = ?",
        )
        .bind(&ed2k)
        .fetch_optional(db)
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
            .execute(db)
            .await?;
        Ok(aid)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn anime_one(
        self: &Arc<Self>,
        db: &sqlx::SqlitePool,
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
            self.store_anime(db, item_id, kind, &media, Some(aid), Some(m)).await?;
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
        self.store_anime(db, item_id, kind, &media, anidb_id, mapping).await?;
        tracing::info!(title, anilist = media.id, anidb = anidb_id, "anime matched");
        Ok(true)
    }

    /// Persist an AniList match: metadata upsert + relations graph.
    async fn store_anime(
        &self,
        db: &sqlx::SqlitePool,
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
        // The anime composite's answer (HUB-5). Recorded under
        // 'anilist', which ranks wherever 'anime' sits in the chain, so
        // the TMDB/TVDB tail can fill what AniList leaves empty —
        // cover art and synopsis, most often — without ever being able
        // to overwrite what AniDB/AniList did supply.
        crate::providers::store_answer(
            db,
            item_id,
            "anilist",
            &media.id.to_string(),
            "auto",
            crate::providers::Fields {
                title: media.display_title(),
                overview: media.plain_description(),
                poster_path: poster.clone(),
                rating: media.average_score.map(|s| s / 10.0),
                premiered: media.premiered(),
                original_language: media.original_language().map(str::to_string),
                genres: Some(serde_json::to_string(&genres)?),
            },
            &crate::providers::chain_in_force(db, "anime").await,
        )
        .await?;
        // Identity columns the merge never touches: they say what this
        // anime IS and how it bridges to the other services.
        sqlx::query(
            "UPDATE merged_metadata SET anidb_id = ?, anilist_id = ?,
                mapped_tvdb = ?, mapped_tmdb = ? WHERE item_id = ?",
        )
        .bind(anidb_id)
        .bind(media.id)
        .bind(mapping.and_then(|m| m.tvdb_id))
        .bind(mapping.and_then(|m| m.tmdb_for(kind)))
        .bind(item_id)
        .execute(db)
        .await?;

        // Relations graph → watch-order building blocks. Watchable
        // relation kinds only; adaptations point at manga.
        sqlx::query("DELETE FROM item_relations WHERE from_item = ?")
            .bind(item_id)
            .execute(db)
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
                .execute(db)
                .await?;
            }
        }
        Ok(())
    }

    /// One-time healing for rows matched before original_language was
    /// captured at search time: fetch TMDB details (movie/tv by matched
    /// or mapped id). '' marks asked-but-absent so nothing re-asks.
    /// TVDB-only rows are left NULL (their language arrives if ever
    /// re-matched); the sweep goes quiet once everything is stamped.
    async fn backfill_original_language(
        self: &Arc<Self>,
        registry: &Registry,
        tmdb_key: &str,
    ) -> Result<()> {
        let rows = sqlx::query(
            "SELECT m.item_id, i.kind,
                    CASE WHEN m.provider = 'tmdb' THEN m.provider_id
                         ELSE CAST(m.mapped_tmdb AS TEXT) END AS tmdb_id
             FROM merged_metadata m JOIN items i ON i.id = m.item_id
             WHERE m.original_language IS NULL AND m.provider_id != ''
               AND i.kind IN ('movie', 'show')
               AND (m.provider = 'tmdb' OR m.mapped_tmdb IS NOT NULL)",
        )
        .fetch_all(registry.db())
        .await?;
        if rows.is_empty() {
            return Ok(());
        }
        tracing::info!(items = rows.len(), "original-language backfill starting");
        let sem = Arc::new(tokio::sync::Semaphore::new(4));
        let mut tasks = tokio::task::JoinSet::new();
        for row in rows {
            let (item_id, kind, tmdb_id) = (
                row.get::<String, _>("item_id"),
                row.get::<String, _>("kind"),
                row.get::<Option<String>, _>("tmdb_id"),
            );
            let Some(tmdb_id) = tmdb_id else { continue };
            let this = self.clone();
            let key = tmdb_key.to_string();
            let db = registry.db().clone();
            let sem = sem.clone();
            tasks.spawn(async move {
                let _permit = sem.acquire().await;
                let path = if kind == "movie" { "movie" } else { "tv" };
                #[derive(Deserialize)]
                struct Details {
                    #[serde(default)]
                    original_language: Option<String>,
                }
                let req = this
                    .http
                    .get(format!("https://api.themoviedb.org/3/{path}/{tmdb_id}"))
                    .query(&[("api_key", key.as_str())]);
                let lang = match this.http.send(req).await.and_then(|r| Ok(r.error_for_status()?)) {
                    Ok(resp) => match resp.json::<Details>().await {
                        Ok(det) => det.original_language.unwrap_or_default(),
                        Err(_) => String::new(),
                    },
                    Err(e) => {
                        tracing::debug!(tmdb_id, error = %e, "details fetch failed");
                        return; // transient: stays NULL, retried next run
                    }
                };
                let _ = sqlx::query(
                    "UPDATE merged_metadata SET original_language = ? WHERE item_id = ?",
                )
                .bind(&lang)
                .bind(&item_id)
                .execute(&db)
                .await;
            });
        }
        while tasks.join_next().await.is_some() {}
        tracing::info!("original-language backfill complete");
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
        // Fetch for shows with metadata-less episodes, plus absolute-
        // numbered shows whose episodes lack the HUB-31 season
        // projection (backfill). ponytail: a show TVDB never curated
        // absolute numbers for re-fetches each run — a few cached-token
        // pages per anime show; revisit if a library full of them appears.
        let shows = sqlx::query(
            "SELECT i.id, m.provider, m.provider_id, m.mapped_tvdb, m.mapped_tmdb, m.anidb_id
             FROM items i
             JOIN merged_metadata m ON m.item_id = i.id
             WHERE i.kind = 'show' AND m.provider_id != ''
               AND m.confidence != 'rejected'
               -- Episode data follows the chain like everything else
               -- (HUB-5): every provider that identified this show is an
               -- episode source, so a show whose owner carries no episode
               -- list still gets one from the other. A provider that has
               -- answered for an episode is not asked again; a recorded
               -- miss goes stale after a week so airing shows converge.
               AND (EXISTS (
                 SELECT 1 FROM provider_metadata sp
                 JOIN items e ON e.parent_id = i.id
                 LEFT JOIN provider_metadata ep
                        ON ep.item_id = e.id AND ep.provider = sp.provider
                 WHERE sp.item_id = i.id AND sp.provider_id != ''
                   AND sp.provider IN ('tmdb', 'tvdb')
                   AND (ep.item_id IS NULL
                        OR (ep.confidence = 'miss'
                            AND ep.updated_at < unixepoch() - 7 * 86400)))
               OR EXISTS (
                 SELECT 1 FROM items e
                 JOIN merged_metadata em ON em.item_id = e.id
                 WHERE e.parent_id = i.id AND e.season IS NULL
                   AND em.proj_episode IS NULL
                   AND em.updated_at < unixepoch() - 7 * 86400))",
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
            let show_id = row.get::<String, _>("id");
            let aid = row.get::<Option<i64>, _>("anidb_id").map(|a| a as u32);
            // Every episode source this show has: the providers that
            // identified it, plus the anime-lists mapped ids (HUB-29/31)
            // for anime, whose own services carry no episode lists.
            let mut sources: Vec<(String, String)> = sqlx::query(
                "SELECT provider, provider_id FROM provider_metadata
                 WHERE item_id = ? AND provider_id != '' AND provider IN ('tmdb','tvdb')",
            )
            .bind(&show_id)
            .fetch_all(registry.db())
            .await?
            .into_iter()
            .map(|r| (r.get::<String, _>("provider"), r.get::<String, _>("provider_id")))
            .collect();
            for (p, mapped) in [
                ("tvdb", row.get::<Option<i64>, _>("mapped_tvdb")),
                ("tmdb", row.get::<Option<i64>, _>("mapped_tmdb")),
            ] {
                if let Some(id) = mapped
                    && !sources.iter().any(|(sp, _)| sp == p)
                {
                    sources.push((p.to_string(), id.to_string()));
                }
            }
            for (provider, pid) in sources {
                let this = self.clone();
                let key = tmdb_key.to_string();
                let token = tvdb_token.cloned();
                let db = registry.db().clone();
                let sem = sem.clone();
                let show = show_id.clone();
                tasks.spawn(async move {
                    let _permit = sem.acquire().await;
                    if let Err(e) = this
                        .enrich_show_episodes(&db, &show, &provider, &pid, &key, token.as_deref(), aid)
                        .await
                    {
                        tracing::warn!(show = %show, provider = %provider,
                            error = format!("{e:#}"), "episode fetch failed");
                    }
                });
            }
        }
        while tasks.join_next().await.is_some() {}
        tracing::info!("episode enrichment complete");
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn enrich_show_episodes(
        &self,
        db: &sqlx::SqlitePool,
        show_id: &str,
        provider: &str,
        pid: &str,
        tmdb_key: &str,
        tvdb_token: Option<&String>,
        anidb_id: Option<u32>,
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
        // HUB-31: absolute number → (season, episode) in the provider's
        // seasoned order — the season-view projection.
        let mut proj: std::collections::HashMap<i64, (i64, i64)> = Default::default();
        // Distinguishes "the provider has nothing for this show" from
        // "we could not ask it" — the first is an answer to record, the
        // second must be retried.
        let (mut fetch_ok, mut fetch_failed) = (0u32, 0u32);
        match (provider, absolute) {
            ("tvdb", false) => {
                let token = tvdb_token.context("tvdb-matched show but no tvdb token")?;
                for e in self.tvdb_episodes_english(token, pid, "default").await? {
                    if let (Some(s), n) = (e.season, e.episode) {
                        by_key.insert((Some(s), n), e);
                    }
                }
            }
            ("tvdb", true) => {
                let token = tvdb_token.context("tvdb-matched show but no tvdb token")?;
                let eps_abs = self.tvdb_episodes_english(token, pid, "absolute").await?;
                for (i, e) in eps_abs.into_iter().enumerate() {
                    let n = e.absolute.unwrap_or(i as i64 + 1);
                    by_key.insert((None, n), e);
                }
                // The default order carries absoluteNumber where TVDB
                // curates it (usual for anime) — that join IS the
                // season projection.
                for e in
                    self.tvdb_episodes(token, pid, "default", None).await.unwrap_or_default()
                {
                    if let (Some(abs), Some(s)) = (e.absolute, e.season) {
                        proj.insert(abs, (s, e.episode));
                    }
                }
            }
            (_, false) => {
                let seasons: Vec<i64> = eps
                    .iter()
                    .filter_map(|r| r.get::<Option<i64>, _>("season"))
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                // Ask which seasons TMDB actually has before requesting
                // any: local numbering often disagrees, and requesting a
                // season it does not carry answers 404 — which looks like
                // a failure and is really "there is nothing there".
                // "The Continental" reports zero seasons and cost a
                // wasted request every run until this.
                match self.tmdb_seasons(tmdb_key, pid).await {
                    Ok(have) => {
                        fetch_ok += 1;
                        let have: std::collections::HashSet<i64> =
                            have.into_iter().map(|(s, _)| s).collect();
                        for s in seasons.iter().filter(|s| have.contains(s)) {
                            match self.tmdb_season(tmdb_key, pid, *s).await {
                                Ok(list) => {
                                    for e in list {
                                        by_key.insert((Some(*s), e.episode), e);
                                    }
                                }
                                Err(e) => {
                                    fetch_failed += 1;
                                    tracing::debug!(show = pid, season = s, error = %e,
                                        "season fetch failed");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        fetch_failed += 1;
                        tracing::debug!(show = pid, error = %e, "season list fetch failed");
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
                        proj.insert(abs, (s, n));
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
        // An empty episode list is itself an answer — the provider has
        // nothing under this id — and it must be recorded, or the show
        // reads as short of episodes forever and every run re-queries it
        // (this is what kept "The Continental" coming back). But only if
        // we actually got to ask: all fetches failing is a retry.
        anyhow::ensure!(
            !(by_key.is_empty() && fetch_failed > 0 && fetch_ok == 0),
            "every episode-list fetch failed for {provider} id {pid}"
        );
        // HUB-5 field-level claim: AniDB owns episode TITLES for anime;
        // the TVDB/TMDB bridge keeps stills, overviews, air dates and
        // the HUB-31 projection.
        if absolute && let Some(aid) = anidb_id {
            let wanted: Vec<i64> = eps
                .iter()
                .filter(|r| r.get::<Option<i64>, _>("season").is_none())
                .map(|r| r.get::<i64, _>("episode"))
                .collect();
            match crate::anime::anidb_episode_titles(&self.http, &self.data_dir, aid, &wanted).await
            {
                Ok(titles) => {
                    for ((s, n), e) in by_key.iter_mut() {
                        if s.is_none()
                            && let Some(t) = titles.get(n)
                        {
                            e.title = Some(t.clone());
                        }
                    }
                }
                Err(e) => tracing::warn!(
                    aid,
                    error = format!("{e:#}"),
                    "anidb episode titles unavailable; keeping bridge titles"
                ),
            }
        }

        let mut wrote = 0;
        // Episodes inherit their show's chain: same media type, same
        // precedence, so a reorder covers them too.
        let media_type = crate::providers::media_type_of_item(db, show_id).await;
        let chain = crate::providers::chain_in_force(db, &media_type).await;
        for r in &eps {
            let key = (r.get::<Option<i64>, _>("season"), r.get::<i64, _>("episode"));
            let Some(e) = by_key.get(&key) else { continue };
            let item_id: String = r.get("id");
            // Season projection applies to absolute-numbered rows only.
            let p = if key.0.is_none() { proj.get(&key.1) } else { None };
            // An episode's description is a provider answer like any
            // other (HUB-5) — one provider supplies it today, but it
            // goes through the same store, so a merge can never revert
            // what the episode pass wrote.
            crate::providers::store_answer(
                db,
                &item_id,
                provider,
                &e.provider_id,
                "auto",
                crate::providers::Fields {
                    title: e.title.clone(),
                    overview: e.overview.clone(),
                    poster_path: e.image.clone(),
                    rating: e.rating,
                    premiered: e.aired.clone(),
                    ..Default::default()
                },
                &chain,
            )
            .await?;
            // The season/absolute projection is identity, not
            // description: the merge never touches it (HUB-31).
            sqlx::query(
                "UPDATE merged_metadata SET proj_season = ?, proj_episode = ?
                 WHERE item_id = ?",
            )
            .bind(p.map(|v| v.0))
            .bind(p.map(|v| v.1))
            .bind(&item_id)
            .execute(db)
            .await?;
            wrote += 1;
        }
        // Episodes the provider had nothing for: record the attempt, or
        // this show is selected again on every single run (it was — nine
        // times in one day, re-fetching whole episode lists each time).
        let mut unmatched = 0;
        for r in &eps {
            let key = (r.get::<Option<i64>, _>("season"), r.get::<i64, _>("episode"));
            if by_key.contains_key(&key) {
                continue;
            }
            let item_id: String = r.get("id");
            crate::providers::store_answer(
                db,
                &item_id,
                provider,
                "",
                "miss",
                crate::providers::Fields::default(),
                &chain,
            )
            .await?;
            unmatched += 1;
        }
        tracing::info!(show = show_id, episodes = wrote, unmatched,
            "episode metadata stored");
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
        let resp = self.http.send(self.http.get(&url)).await?.error_for_status()?;
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
            original_language: None,
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

// ---------- HUB-5 provider adapters ----------

/// Persist a generic-provider match (or a miss when `pick` is None).
impl Enricher {
    pub(crate) async fn store_generic(
        &self,
        db: &sqlx::SqlitePool,
        item_id: &str,
        provider: &str,
        pick: Option<&(Candidate, &'static str)>,
    ) -> Result<()> {
        self.store_answer_for(db, item_id, provider, pick, "movies").await
    }

    /// Record one provider's answer and re-merge the item (HUB-5).
    pub(crate) async fn store_answer_for(
        &self,
        db: &sqlx::SqlitePool,
        item_id: &str,
        provider: &str,
        pick: Option<&(Candidate, &'static str)>,
        media_type: &str,
    ) -> Result<()> {
        let (provider_id, confidence, c) = match pick {
            Some((c, conf)) => (c.id.to_string(), *conf, Some(c)),
            None => (String::new(), "miss", None),
        };
        let fields = crate::providers::Fields {
            title: c.map(|c| c.title.clone()),
            overview: c.and_then(|c| c.overview.clone()),
            poster_path: c.and_then(|c| c.poster_path.clone()),
            rating: c.and_then(|c| c.vote_average),
            premiered: c.and_then(|c| c.release_date.clone()),
            original_language: c.and_then(|c| c.original_language.clone()),
            genres: None, // TMDB/TVDB search results carry no genre names
        };
        let chain = crate::providers::chain_in_force(db, media_type).await;
        crate::providers::store_answer(
            db,
            item_id,
            provider,
            &provider_id,
            confidence,
            fields,
            &chain,
        )
        .await
    }

    /// TheTVDB record by id — the bridged path, same rule as TMDB's:
    /// used only where a mapping already says which record this is.
    pub(crate) async fn tvdb_details(
        &self,
        token: &str,
        kind: &str,
        tvdb_id: i64,
    ) -> Result<Candidate> {
        #[derive(Deserialize)]
        struct Extended {
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            overview: Option<String>,
            #[serde(default)]
            image: Option<String>,
            #[serde(default)]
            score: Option<f64>,
            #[serde(default, alias = "firstAired")]
            first_aired: Option<String>,
        }
        #[derive(Deserialize)]
        struct Resp {
            data: Extended,
        }
        let path = if kind == "movie" { "movies" } else { "series" };
        let req = self
            .http
            .get(format!("https://api4.thetvdb.com/v4/{path}/{tvdb_id}/extended"))
            .bearer_auth(token);
        let r: Resp = self
            .http
            .send(req)
            .await
            .context("tvdb details")?
            .error_for_status()?
            .json()
            .await
            .context("tvdb details json")?;
        Ok(Candidate {
            id: tvdb_id as u64,
            title: r.data.name.unwrap_or_default(),
            original_title: None,
            overview: r.data.overview,
            poster_path: r.data.image,
            vote_average: r.data.score,
            release_date: r.data.first_aired,
            original_language: None,
        })
    }

    /// TMDB details by id — the bridged path (HUB-31): an anime item
    /// identified by AniDB carries a mapped TMDB id, and this fills the
    /// description fields AniList left empty WITHOUT re-matching it.
    pub(crate) async fn tmdb_details(
        &self,
        key: &str,
        kind: &str,
        tmdb_id: i64,
    ) -> Result<Candidate> {
        let path = if kind == "movie" { "movie" } else { "tv" };
        let mut req = self.http.get(format!("https://api.themoviedb.org/3/{path}/{tmdb_id}"));
        if key.starts_with("eyJ") {
            req = req.bearer_auth(key);
        } else {
            req = req.query(&[("api_key", key)]);
        }
        let c: Candidate = self
            .http
            .send(req)
            .await
            .context("tmdb details")?
            .error_for_status()?
            .json()
            .await
            .context("tmdb details json")?;
        Ok(c)
    }
}

struct TmdbProvider {
    enricher: Arc<Enricher>,
    key: String,
}

impl TmdbProvider {
    /// Supply missing fields for an item another provider identified.
    ///
    /// Anime is bridged, never re-matched (HUB-5/HUB-31): the mapped
    /// TMDB id from anime-lists is the only way in, and if there is no
    /// mapping we decline rather than guess. When TMDB merely ranks
    /// below another general provider, an ordinary search is fair game
    /// — that ban applies to anime identity, not to description.
    async fn fill_gaps(
        &self,
        db: &sqlx::SqlitePool,
        item: &crate::providers::ItemRef,
        owner: &str,
    ) -> Result<crate::providers::Outcome> {
        let mapped: Option<i64> =
            sqlx::query_scalar("SELECT mapped_tmdb FROM merged_metadata WHERE item_id = ?")
                .bind(&item.id)
                .fetch_optional(db)
                .await?
                .flatten();
        let candidate = match (owner, mapped) {
            // Bridged: AniDB decided what this is, TMDB describes it.
            ("anime", Some(id)) => self.enricher.tmdb_details(&self.key, &item.kind, id).await?,
            // No mapped id yet — anime-lists refreshes weekly, so this
            // is "cannot ask", not "asked and missed".
            ("anime", None) => return Ok(crate::providers::Outcome::NotApplicable),
            _ => {
                let cands =
                    self.enricher.search(&self.key, &item.kind, &item.title, item.year).await?;
                match pick_candidate(&cands, &item.title, item.year) {
                    Some((c, _)) => c.clone(),
                    None => return Ok(crate::providers::Outcome::Declined),
                }
            }
        };
        // provider_id is recorded so the row is a real answer, but the
        // merge only hands identity to the top-ranked matcher.
        let pick = (candidate, "auto");
        self.enricher
            .store_answer_for(
                db,
                &item.id,
                "tmdb",
                Some(&pick),
                &crate::providers::media_type_of_item(db, &item.id).await,
            )
            .await?;
        tracing::debug!(title = %item.title, owner,
            "tmdb recorded its answer for an item it did not identify");
        Ok(crate::providers::Outcome::Contributed)
    }
}


#[async_trait::async_trait]
impl crate::providers::Provider for TmdbProvider {
    fn name(&self) -> &'static str {
        "tmdb"
    }

    async fn enrich(
        &self,
        db: &sqlx::SqlitePool,
        item: &crate::providers::ItemRef,
    ) -> Result<crate::providers::Outcome> {
        if !matches!(item.kind.as_str(), "movie" | "show") {
            return Ok(crate::providers::Outcome::NotApplicable);
        }
        // Someone above us owns this item's identity. Fill what they
        // left empty; never re-decide what the item IS.
        if let Some(owner) = &item.owner
            && owner != self.name()
        {
            return self.fill_gaps(db, item, owner).await;
        }
        // Query ladder: TMDB's search has holes (a literal "And" finds
        // nothing where "&" or a shortened query hits); the strict
        // verifier still judges candidates against the FULL local title.
        let title = &item.title;
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
        let mut picked: Option<(Candidate, &'static str)> = None;
        for (vi, q) in variants.iter().enumerate() {
            let cands = self.enricher.search(&self.key, &item.kind, q, item.year).await?;
            if let Some((c, conf)) = pick_candidate(&cands, title, item.year) {
                picked = Some((c.clone(), conf));
                if vi > 0 {
                    tracing::debug!(title, variant = %q, "matched via query variant");
                }
                break;
            }
        }
        if picked.is_none()
            && let Some(alt) = &item.alt
        {
            let alt_year = alt.year.map(|y| y as i64).or(item.year);
            let cands = self.enricher.search(&self.key, &item.kind, &alt.title, alt_year).await?;
            if let Some((c, conf)) = pick_candidate(&cands, &alt.title, alt_year) {
                picked = Some((c.clone(), conf));
                tracing::debug!(title, alt = %alt.title, "matched via directory name");
            }
        }
        match picked {
            Some(pick) => {
                let conf = pick.1;
                self.enricher.store_generic(db, &item.id, "tmdb", Some(&pick)).await?;
                Ok(crate::providers::Outcome::Matched(conf))
            }
            None => Ok(crate::providers::Outcome::Declined),
        }
    }
}

struct TvdbProvider {
    enricher: Arc<Enricher>,
    token: std::sync::Arc<String>,
}

#[async_trait::async_trait]
impl crate::providers::Provider for TvdbProvider {
    fn name(&self) -> &'static str {
        "tvdb"
    }

    async fn enrich(
        &self,
        db: &sqlx::SqlitePool,
        item: &crate::providers::ItemRef,
    ) -> Result<crate::providers::Outcome> {
        if !matches!(item.kind.as_str(), "movie" | "show") {
            return Ok(crate::providers::Outcome::NotApplicable);
        }
        // Anime identity is AniDB's; TVDB may describe, never re-match
        // (HUB-5/HUB-31). Without a mapped id there is no honest way in,
        // so it declines rather than searching by title.
        if item.owner.as_deref() == Some("anilist") {
            let mapped: Option<i64> =
                sqlx::query_scalar("SELECT mapped_tvdb FROM merged_metadata WHERE item_id = ?")
                    .bind(&item.id)
                    .fetch_optional(db)
                    .await?
                    .flatten();
            let Some(tvdb_id) = mapped else {
                return Ok(crate::providers::Outcome::NotApplicable);
            };
            let c = self.enricher.tvdb_details(&self.token, &item.kind, tvdb_id).await?;
            let pick = (c, "auto");
            self.enricher
                .store_answer_for(db, &item.id, "tvdb", Some(&pick), "anime")
                .await?;
            return Ok(crate::providers::Outcome::Contributed);
        }
        let cands = self.enricher.tvdb_search(&self.token, &item.kind, &item.title).await?;
        match pick_candidate(&cands, &item.title, item.year) {
            Some((c, conf)) => {
                let pick = (c.clone(), conf);
                self.enricher.store_generic(db, &item.id, "tvdb", Some(&pick)).await?;
                tracing::debug!(title = %item.title, "matched via TVDB fallback");
                Ok(crate::providers::Outcome::Matched(conf))
            }
            None => Ok(crate::providers::Outcome::Declined),
        }
    }
}

/// The anime chain's composite: AniDB identity (ED2K exact > reverse
/// mapping > titles dump), AniList description + relations. "AniDB is
/// special" (rate limits, bans, never-ask-twice) stays inside.
pub(crate) struct AnimeProvider {
    enricher: Arc<Enricher>,
    titles: crate::anime::AnidbTitles,
    lists: crate::anime::AnimeLists,
}

#[async_trait::async_trait]
impl crate::providers::Provider for AnimeProvider {
    fn name(&self) -> &'static str {
        "anime"
    }

    async fn enrich(
        &self,
        db: &sqlx::SqlitePool,
        item: &crate::providers::ItemRef,
    ) -> Result<crate::providers::Outcome> {
        if !matches!(item.kind.as_str(), "movie" | "show") {
            return Ok(crate::providers::Outcome::NotApplicable);
        }
        // ED2K-exact identity first; a failure disables the UDP client
        // for the rest of the run (ban safety).
        let mut exact_aid: Option<u32> = None;
        {
            let mut guard = self.enricher.anidb.lock().await;
            if let Some(client) = guard.as_mut() {
                match self.enricher.anidb_identify(db, client, &item.id).await {
                    Ok(aid) => exact_aid = aid,
                    Err(e) => {
                        tracing::warn!(error = format!("{e:#}"), "anidb lookup failed; disabling for this run");
                        *guard = None;
                    }
                }
            }
        }
        if item.identified {
            // Already matched by name: only a DISAGREEING canonical hash
            // re-decides; agreement or no-answer settles the chain.
            match exact_aid {
                Some(aid) if item.known_aid != Some(aid) => {
                    tracing::warn!(title = %item.title, old_aid = item.known_aid, new_aid = aid,
                        "ed2k identification disagrees with name match; hash wins");
                }
                _ => return Ok(crate::providers::Outcome::Settled),
            }
        }
        let matched = self
            .enricher
            .anime_one(
                db,
                &self.titles,
                &self.lists,
                &item.id,
                &item.kind,
                &item.title,
                item.year,
                item.existing.clone(),
                item.manual,
                exact_aid,
            )
            .await?;
        Ok(if matched {
            crate::providers::Outcome::Matched("auto")
        } else {
            crate::providers::Outcome::Declined
        })
    }

    async fn finish(&self) {
        // Deliberately does NOT log out or drop the client: the session
        // lives as long as the process so the next run reuses it.
    }
}

struct MusicbrainzProvider {
    enricher: Arc<Enricher>,
}

#[async_trait::async_trait]
impl crate::providers::Provider for MusicbrainzProvider {
    fn name(&self) -> &'static str {
        "musicbrainz"
    }

    async fn enrich(
        &self,
        db: &sqlx::SqlitePool,
        item: &crate::providers::ItemRef,
    ) -> Result<crate::providers::Outcome> {
        let (Some(artist), "album") = (&item.artist, item.kind.as_str()) else {
            return Ok(crate::providers::Outcome::NotApplicable);
        };
        // Pacing lives in the gate now (one request per second, keyed on
        // musicbrainz.org) — a sleep here would only double it.
        let Some(rg) = self.enricher.musicbrainz_album(&item.title, artist).await? else {
            return Ok(crate::providers::Outcome::Declined);
        };
        crate::providers::store_answer(
            db,
            &item.id,
            "musicbrainz",
            &rg.id,
            "auto",
            crate::providers::Fields {
                title: Some(rg.title.clone()),
                poster_path: Some(format!(
                    "https://coverartarchive.org/release-group/{}/front-500",
                    rg.id
                )),
                premiered: rg.first_release_date.clone(),
                genres: Some(serde_json::to_string(&rg.genres)?),
                ..Default::default()
            },
            &crate::providers::chain_in_force(db, "music").await,
        )
        .await?;
        Ok(crate::providers::Outcome::Matched("auto"))
    }
}
