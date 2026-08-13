//! Metadata enrichment (M4/HUB-7): match movies and shows against TMDB,
//! store overview/poster/rating per item. Matching is conservative —
//! normalized-title equality (plus year within ±1 when known) is an
//! `auto` match; a lone plausible result is `weak`; anything else is a
//! recorded `miss` so the next run doesn't re-search it. The admin can
//! re-run after fixing titles; a review queue (HUB-8) comes later.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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

/// TheTVDB credentials as configured. Carried where a token used to
/// be: holding the token meant deciding at run start whether TVDB
/// exists, which is a question only a request can answer.
#[derive(Clone)]
pub(crate) struct TvdbCreds {
    key: String,
    pin: Option<String>,
}

pub struct Enricher {
    /// Every provider call goes out through this: pacing and
    /// rate-limit backoff live in `gate.rs`, not at the call sites.
    http: std::sync::Arc<crate::gate::Http>,
    data_dir: std::path::PathBuf,
    anilist: crate::anime::Anilist,
    last_nudge: std::sync::atomic::AtomicU64,
    running: AtomicBool,
    scheduled: AtomicBool,
    rerun_requested: AtomicBool,
    /// (matched, weak, missed) of the current/last run.
    progress: (AtomicUsize, AtomicUsize, AtomicUsize),
    /// The UDP session, kept for the PROCESS lifetime — not per run.
    /// A login per enrichment run is what got this client banned twice
    /// in one evening; sessions are cheap to hold and expensive to
    /// re-establish.
    anidb: tokio::sync::Mutex<Option<crate::anidb::Anidb>>,
    /// TheTVDB's bearer token, fetched on FIRST USE and kept for the
    /// process (it is valid for weeks). Lazy so that a login failure
    /// cannot remove TVDB from a whole run: TMDB is present whenever
    /// its key is set and fails per request, and TVDB behaving
    /// differently made a transient outage indistinguishable from "no
    /// TVDB configured" — including to the selection, which then
    /// stopped counting TVDB work as owed.
    tvdb: tokio::sync::Mutex<Option<std::sync::Arc<String>>>,
    /// The byte plane, for HUB-9: reading a .nfo means leasing it from the
    /// mediahost that holds it. Attached at startup; absent in tests, where
    /// the local provider then simply is not in the chain.
    sessions: std::sync::OnceLock<Arc<crate::sessions::Sessions>>,
}

/// One file moved to the episode its hash says it is (HUB-30).
#[derive(Debug)]
/// A file left where the name put it, kept for the collision pass:
/// what AniDB says it is, and which slot it currently shares.
struct SlotOccupant {
    item_id: String,
    source_id: i64,
    path: String,
    season: Option<i64>,
    episode: i64,
    eid: Option<i64>,
    epno: String,
}

#[derive(Debug)]
pub struct EpisodeRebind {
    pub path: String,
    pub from: (Option<i64>, i64),
    pub to: (Option<i64>, i64),
}

/// AniDB's episode string, classified. Regular episodes are bare digits
/// ("1", "01"); one letter prefixes the rest: S special, C credit,
/// T trailer, P parody, O other.
///
/// Every typed kind slots into SEASON 0, in disjoint hundred-bands —
/// S=n, C=100+n, T=200+n, P=300+n, O=400+n — so a credits reel can
/// never collide with a special. The bands are this hub's own layout,
/// not a provider convention; a show would need a hundred specials to
/// breach one, and AniDB numbers none anywhere near that.
enum Epno {
    Regular(i64),
    Zero(i64),
}

fn parse_epno(epno: &str) -> Option<Epno> {
    if let Ok(n) = epno.parse::<i64>() {
        return Some(Epno::Regular(n));
    }
    let mut chars = epno.chars();
    let band = match chars.next()?.to_ascii_uppercase() {
        'S' => 0,
        'C' => 100,
        'T' => 200,
        'P' => 300,
        'O' => 400,
        _ => return None,
    };
    chars
        .as_str()
        .parse::<i64>()
        .ok()
        .map(|n| Epno::Zero(band + n))
}

fn fmt_slot(season: Option<i64>, episode: i64) -> String {
    match season {
        Some(s) => format!("S{s:02}E{episode:02}"),
        None => format!("abs {episode}"),
    }
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
impl Candidate {
    /// The provider's record id for this candidate.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Fixture for the matcher tests: id, title, air date.
    pub fn for_test(id: u64, title: &str, aired: Option<&str>) -> Self {
        Self {
            id,
            title: title.into(),
            original_title: None,
            overview: None,
            poster_path: None,
            vote_average: None,
            release_date: aired.map(str::to_string),
            original_language: None,
        }
    }
}

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
    // A year both sides actually state and agree on. Distinct from
    // year_ok, which passes when either side is silent — that leniency
    // let TMDB's 1952 "The Continental" win an exact-title match over
    // the 2023 "The Continental: From the World of John Wick" the local
    // file plainly meant.
    let year_agrees =
        |c: &Candidate| matches!((year, c.year()), (Some(w), Some(h)) if (w - h).abs() <= 1);
    // The local title with the candidate's subtitle after it: a folder
    // named "The Continental" against "The Continental: From the World
    // of John Wick". Only counts at a separator, so "The Office" does
    // not swallow "The Officer".
    // Tested on the RAW title, because fold() strips the punctuation
    // that makes it a subtitle: without the colon, "Heat Wave" would
    // read as "Heat" plus a subtitle.
    let subtitled = |c: &Candidate| {
        c.title
            .split_once([':', '-', '\u{2013}', '\u{2014}'])
            .is_some_and(|(head, _)| fold(head) == norm)
    };

    // Confirmed year beats a silent one, whichever title form matched.
    if let Some(c) = candidates.iter().find(|c| title_eq(c) && year_agrees(c)) {
        return Some((c, "auto"));
    }
    if let Some(c) = candidates.iter().find(|c| subtitled(c) && year_agrees(c)) {
        return Some((c, "auto"));
    }
    // No year on either side: an exact title is still the best signal.
    if let Some(c) = candidates.iter().find(|c| title_eq(c) && year_ok(c)) {
        return Some((c, "auto"));
    }
    if let Some(c) = candidates.iter().find(|c| subtitled(c) && year_ok(c)) {
        return Some((c, "weak"));
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
/// The generic pass's selection (movies/series; anime items are carved
/// out to their own pass). Extracted so `scale_bench` can time the real
/// statement: this runs at the top of every enrichment pass, so its
/// quiescent cost at catalogue scale is a standing tax. Binds ?1 =
/// [`crate::providers::QUERY_REV`], ?2 = a JSON array of the searcher
/// names read off the run's own `ProviderSet`
/// ([`ProviderSet::searchers_in`]) — never a list written out beside
/// it. A searcher outside the bound set is never owed, so one that
/// cannot answer this run does not force a re-select of every item
/// lacking its answer.
///
/// Read off, because "configured" and "able to answer" are different
/// questions and diverge exactly when something is broken. A provider
/// whose key is set but whose login failed is absent from the set: a
/// list built from the key would still claim its work is owed, and
/// nothing in the chain could ever clear that debt — a permanent
/// full-catalogue re-select against the one statement whose cost this
/// doc calls a standing tax.
pub const GENERIC_SELECTION_SQL: &str = "SELECT i.id,i.kind,i.title,i.norm_title,i.year,
                    (SELECT f.path_rel FROM files f JOIN file_bindings fb ON fb.file_id=f.id WHERE fb.item_id=i.id LIMIT 1) AS src_path,
                    c0.media_type AS media_type
             FROM items i JOIN collections c0
               ON (c0.module_id,c0.collection_id)=(i.module_id,i.collection_id)
             WHERE i.kind IN ('movie', 'show')
               AND (
                    -- HUB-5: a searcher is owed work while its CURRENT
                    -- title question has no provider_queries row and no
                    -- real answer stands. Never-matched, renamed and
                    -- QUERY_REV-bumped items all land here; misses never
                    -- gate. json_each(?2) walks exactly the searcher
                    -- names the pass bound this run, so a conditional
                    -- provider (TVDB today, whatever's next tomorrow)
                    -- needs no SQL change: absent from ?2, it is never
                    -- owed.
                    EXISTS (
                      SELECT 1 FROM json_each(?2) sp
                      WHERE NOT EXISTS (
                          SELECT 1 FROM provider_metadata pm
                          WHERE pm.item_id = i.id AND pm.provider = sp.value
                            AND pm.provider_id <> '')
                        AND NOT EXISTS (
                          SELECT 1 FROM provider_queries q
                          WHERE q.item_id = i.id AND q.provider = sp.value
                            AND q.query_type = 'title'
                            AND q.query = i.norm_title || '|' || COALESCE(i.year, '')
                            AND q.rev >= ?1))
                    -- HUB-9: local owes an answer. Gated on there
                    -- actually being something beside the media, or an
                    -- item with no cover and no .nfo would be re-selected
                    -- every run for a provider that stores nothing.
                    -- local's answer and what the scan can see disagree,
                    -- in either direction: a sidecar appeared and nobody
                    -- has read it, or the one it was built from is gone
                    -- and the answer now describes a deleted file.
                    OR (EXISTS (SELECT 1 FROM provider_metadata pl
                                 WHERE pl.item_id = i.id AND pl.provider = 'local'
                                   AND pl.provider_id <> '')
                        != (i.id IN (SELECT COALESCE(ch.parent_id, ch.id)
                                       FROM files f5 JOIN items ch ON ch.id=f5.item_id
                                      WHERE json_extract(f5.streams_json, '$.nfo') IS NOT NULL)))
                    OR (NOT EXISTS (SELECT 1 FROM provider_metadata pl
                                     WHERE pl.item_id = i.id AND pl.provider = 'local')
                        AND i.id IN (SELECT COALESCE(ch.parent_id, ch.id)
                                       FROM files f4 JOIN items ch ON ch.id=f4.item_id
                                      WHERE json_extract(f4.streams_json, '$.artwork') IS NOT NULL
                                         OR json_extract(f4.streams_json, '$.nfo') IS NOT NULL))
                    -- or a provider refused and is due again (bans and
                    -- rate limits reschedule, they never drop work).
                    OR EXISTS (
                      SELECT 1 FROM enrichment_queue q
                      WHERE q.item_id = i.id AND q.due_at <= unixepoch()))
               AND c0.media_type<>'anime'
             ORDER BY i.title";

/// Is this error, anywhere in its chain, an HTTP 404? A mapped id that
/// the provider answers 404 for is an ANSWER — "no such record" (legacy
/// series ids TVDB v4 dropped, movies that changed namespace) — and
/// must record its question and decline terminally, not reschedule
/// forever as if the network had hiccuped.
fn is_http_404(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<reqwest::Error>()
            .and_then(reqwest::Error::status)
            .is_some_and(|s| s == reqwest::StatusCode::NOT_FOUND)
    })
}

pub(crate) fn fold(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    const WORDS: &[(&str, &str)] = &[
        ("zero", "0"),
        ("one", "1"),
        ("two", "2"),
        ("three", "3"),
        ("four", "4"),
        ("five", "5"),
        ("six", "6"),
        ("seven", "7"),
        ("eight", "8"),
        ("nine", "9"),
        ("ten", "10"),
        ("eleven", "11"),
        ("twelve", "12"),
        ("thirteen", "13"),
        ("fourteen", "14"),
        ("fifteen", "15"),
        ("sixteen", "16"),
        ("seventeen", "17"),
        ("eighteen", "18"),
        ("nineteen", "19"),
        ("twenty", "20"),
    ];
    let s = s.replace(['&', '+'], " and ");
    let base: String = kahawai_core::names::normalize_title(&s)
        .nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect();
    const ROMAN: &[(&str, &str)] = &[
        ("ii", "2"),
        ("iii", "3"),
        ("iv", "4"),
        ("vi", "6"),
        ("vii", "7"),
        ("viii", "8"),
        ("ix", "9"),
        ("xi", "11"),
        ("xii", "12"),
        ("xiii", "13"),
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
            scheduled: AtomicBool::new(false),
            rerun_requested: AtomicBool::new(false),
            last_nudge: std::sync::atomic::AtomicU64::new(0),
            progress: Default::default(),
            anidb: Default::default(),
            tvdb: Default::default(),
            sessions: Default::default(),
        }
    }

    /// Wire the byte plane in (HUB-9). Without it the local provider is
    /// left out of the chain rather than failing per item.
    pub fn attach_sessions(&self, sessions: Arc<crate::sessions::Sessions>) {
        let _ = self.sessions.set(sessions);
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

    async fn search(
        &self,
        key: &str,
        kind: &str,
        title: &str,
        year: Option<i64>,
    ) -> Result<Vec<Candidate>> {
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
        Ok(resp
            .json::<SearchResponse>()
            .await
            .context("tmdb json")?
            .results)
    }

    /// The cached bearer token, logging in on first use. Concurrent
    /// callers queue on the mutex, so a fleet of episode tasks starting
    /// together still costs one login.
    pub(crate) async fn tvdb_token(&self, creds: &TvdbCreds) -> Result<std::sync::Arc<String>> {
        let mut slot = self.tvdb.lock().await;
        if let Some(t) = slot.clone() {
            return Ok(t);
        }
        let token = std::sync::Arc::new(self.tvdb_login(&creds.key, creds.pin.as_deref()).await?);
        *slot = Some(token.clone());
        Ok(token)
    }

    /// Drop the cached token so the next use logs in again. A token
    /// lasts weeks, so the usual reason a request fails on auth is that
    /// this one finally expired — and the process outlives that.
    async fn tvdb_forget(&self) {
        *self.tvdb.lock().await = None;
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
            .send(
                self.http
                    .post("https://api4.thetvdb.com/v4/login")
                    .json(&body),
            )
            .await
            .context("tvdb login request")?
            .error_for_status()
            .context("tvdb login rejected (key/pin?)")?;
        Ok(resp
            .json::<LoginResp>()
            .await
            .context("tvdb login json")?
            .data
            .token)
    }

    async fn tvdb_search(&self, token: &str, kind: &str, title: &str) -> Result<Vec<Candidate>> {
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
    async fn tmdb_season(&self, key: &str, show_id: &str, season: i64) -> Result<Vec<EpisodeData>> {
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
        let mut req = self.http.get(format!(
            "https://api.themoviedb.org/3/tv/{show_id}/season/{season}"
        ));
        if key.starts_with("eyJ") {
            req = req.bearer_auth(key);
        } else {
            req = req.query(&[("api_key", key)]);
        }
        let s: Season = self
            .http
            .send(req)
            .await?
            .error_for_status()?
            .json()
            .await?;
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
        let mut req = self
            .http
            .get(format!("https://api.themoviedb.org/3/tv/{show_id}"));
        if key.starts_with("eyJ") {
            req = req.bearer_auth(key);
        } else {
            req = req.query(&[("api_key", key)]);
        }
        let s: Show = self
            .http
            .send(req)
            .await?
            .error_for_status()?
            .json()
            .await?;
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
        let by_id: std::collections::HashMap<String, EpisodeData> = eng
            .into_iter()
            .map(|e| (e.provider_id.clone(), e))
            .collect();
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
        self.schedule(registry, std::time::Duration::ZERO);
    }

    /// A completed scan is authoritative owed-work input even when every file
    /// was already hashed. Completions settle briefly and coalesce; a request
    /// arriving during a run causes one follow-up pass.
    pub fn scan_complete(self: &Arc<Self>, registry: Arc<Registry>) {
        self.schedule(registry, std::time::Duration::from_secs(2));
    }

    fn schedule(self: &Arc<Self>, registry: Arc<Registry>, settle: std::time::Duration) {
        self.rerun_requested.store(true, Ordering::SeqCst);
        if self
            .scheduled
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(settle).await;
            loop {
                // A manual run may own the runner. Keep the request pending.
                if this.running.load(Ordering::SeqCst) {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
                this.rerun_requested.store(false, Ordering::SeqCst);
                if let Err(e) = this.run_once(&registry).await {
                    tracing::warn!(error = format!("{e:#}"), "scheduled enrichment failed");
                }
                if this.rerun_requested.load(Ordering::SeqCst) {
                    continue;
                }

                this.scheduled.store(false, Ordering::SeqCst);
                // Close the request-vs-exit race: a requester either spawned
                // its own worker or left the flag set for this one to reacquire.
                if !this.rerun_requested.load(Ordering::SeqCst)
                    || this
                        .scheduled
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_err()
                {
                    break;
                }
            }
        });
    }

    pub async fn run_once(
        self: &Arc<Self>,
        registry: &Arc<Registry>,
    ) -> Result<(usize, usize, usize)> {
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
    async fn run_inner(
        self: &Arc<Self>,
        registry: &Arc<Registry>,
    ) -> Result<(usize, usize, usize)> {
        registry.emit(serde_json::json!({ "kind": "enrich", "running": true }));
        // HUB-5a: OPTIONAL. Chains are declared per media type, so one
        // type's credential must not decide whether another type runs —
        // `chain_for("music")` is musicbrainz alone and needs no key at
        // all, and anime holds TMDB only as the fallback behind the
        // AniDB/AniList composite. Bailing here told a music-only
        // library that a provider its chain does not contain was
        // missing, and enriched nothing.
        let tmdb_key = registry.get_setting(TMDB_KEY_SETTING).await?;
        // The two passes below the chains are TMDB's own work (episode
        // detail, detail backfill), so they keep their own copy and
        // skip themselves when there is no key.
        let tmdb_for_details = tmdb_key.clone();
        // TVDB is the backup resolver: only consulted when the TMDB
        // ladder comes up empty, same strict verifier. Configured is
        // all that is asked here — the login happens on first use, so a
        // TVDB outage costs the requests it breaks, not the run.
        let tvdb_creds = registry
            .get_setting(TVDB_KEY_SETTING)
            .await?
            .map(|key| async move {
                TvdbCreds {
                    key,
                    pin: registry.get_setting(TVDB_PIN_SETTING).await.ok().flatten(),
                }
            });
        let tvdb_creds = match tvdb_creds {
            Some(f) => Some(f.await),
            None => None,
        };
        for c in [&self.progress.0, &self.progress.1, &self.progress.2] {
            c.store(0, Ordering::SeqCst);
        }
        // HUB-5: instantiate the run's providers; chains are declared in
        // providers::chain_for. Unconfigured providers stay absent.
        let mut set = crate::providers::ProviderSet::default();
        // HUB-9: a human's .nfo leads the chain where one exists.
        if let Some(sessions) = self.sessions.get() {
            set.add(Box::new(LocalProvider {
                sessions: sessions.clone(),
                registry: Arc::clone(registry),
            }));
        }
        if let Some(key) = tmdb_key {
            set.add(Box::new(TmdbProvider {
                enricher: self.clone(),
                key,
            }));
        }
        if let Some(creds) = tvdb_creds.clone() {
            set.add(Box::new(TvdbProvider {
                enricher: self.clone(),
                creds,
            }));
        }
        // Recover any bridge ids that went missing before deciding what
        // needs work — a rebuilt id is one less item to re-identify.
        let lists_for_rebuild = crate::anime::AnimeLists::load(&self.http, &self.data_dir)
            .await
            .ok();
        match Self::rebuild_anime_ids(registry.db(), lists_for_rebuild.as_ref()).await {
            Ok(n) if n > 0 => {
                tracing::info!(rows = n, "anime bridge ids rebuilt from stored answers")
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = format!("{e:#}"), "anime id rebuild failed"),
        }
        let anime_items = self.select_anime_items(registry).await.unwrap_or_default();
        // The provider (and with it the AniDB session) must also build
        // when the only work is BARE files awaiting hash lookups — they
        // belong to no item, so no selection can ever represent them.
        // Same predicate resolve_bare_hashes scans with.
        let bare_pending: bool = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS (
               SELECT 1 FROM files f
               JOIN collections col ON (col.module_id, col.collection_id)
                                     = (f.module_id, f.collection_id)
               WHERE col.media_type = 'anime' AND f.ed2k IS NOT NULL
                 AND NOT EXISTS(SELECT 1 FROM file_bindings fb WHERE fb.file_id=f.id)
                 AND NOT EXISTS (SELECT 1 FROM ed2k_aid c
                                  WHERE c.ed2k = f.ed2k
                                    AND (c.aid IS NULL OR c.epno IS NOT NULL)))",
        )
        .fetch_one(registry.db())
        .await
        .map(|v| v != 0)
        .unwrap_or(false);
        if !anime_items.is_empty() || bare_pending {
            match self.build_anime_provider(registry).await {
                Ok(p) => set.add(Box::new(p)),
                Err(e) => {
                    tracing::warn!(
                        error = format!("{e:#}"),
                        "anime provider unavailable this run"
                    )
                }
            }
        }
        set.add(Box::new(MusicbrainzProvider {
            enricher: self.clone(),
        }));
        let providers = Arc::new(set);

        // HUB-5a: every pass runs, and an absent provider is silent. Which
        // providers exist is the operator's choice, not a fault: no
        // TMDB key means no TMDB, the same way no TVDB key means no
        // TVDB, and `run_chain` simply skips what the set does not
        // hold. `local` is asked before every chain (HUB-9), so a pass
        // with no network searcher still reads the owner's .nfo files.
        //
        // Anime chain first (HUB-29): sequential — its providers pace
        // themselves against AniDB/AniList.
        if let Err(e) = self.enrich_anime(registry, &providers, anime_items).await {
            tracing::warn!(error = format!("{e:#}"), "anime enrichment failed");
        }
        // Ask the SQL about the searchers this run ACTUALLY holds —
        // read off the set, not restated beside it. A second list has
        // to be kept in step by hand, and would answer a subtly
        // different question: "is it configured" rather than "can it
        // answer", which diverge exactly when a provider's login has
        // failed. Then nothing in the chain can clear the debt the SQL
        // reports, and every item lacking that provider's answer is
        // re-selected on every run for as long as the fault lasts.
        let generic_searchers = providers.searchers_in(crate::providers::chain_for("movies"));
        let items = sqlx::query(GENERIC_SELECTION_SQL)
            .bind(crate::providers::QUERY_REV)
            .bind(serde_json::to_string(&generic_searchers)?)
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
                row.get::<Option<String>, _>("media_type")
                    .as_deref()
                    .unwrap_or_default(),
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
                norm_title: row.get("norm_title"),
                year,
                artist: None,
                norm_artist: None,
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
                    // No miss row from here: the walker's Declined arm
                    // records misses, guarded so a decline never
                    // overwrites a standing answer. A chain declines in
                    // full whenever every question is already on file —
                    // routine on a restart — and an unconditional miss
                    // upsert here erased the whole catalogue's matches
                    // each time the hub came up.
                    None => {
                        this.progress.2.fetch_add(1, Ordering::SeqCst);
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
        // Both are TMDB's own passes: no key, nothing for them to do.
        // Skipped, not fatal — the chains above may have enriched
        // plenty without one (HUB-5a).
        if let Some(key) = tmdb_for_details.as_deref() {
            if let Err(e) = self
                .enrich_episodes(registry, key, tvdb_creds.as_ref())
                .await
            {
                tracing::warn!(error = format!("{e:#}"), "episode enrichment failed");
            }
            if let Err(e) = self.backfill_details(registry, key).await {
                tracing::warn!(error = format!("{e:#}"), "tmdb details backfill failed");
            }
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
            "SELECT i.id, i.title, i.norm_title, i.artist, i.norm_artist FROM items i
             WHERE i.kind = 'album' AND i.artist IS NOT NULL
               AND (
                    -- Same rule as the video pass: MusicBrainz is owed
                    -- work while its CURRENT question has no
                    -- provider_queries row and no real answer stands.
                    (NOT EXISTS (
                       SELECT 1 FROM provider_metadata pm
                       WHERE pm.item_id = i.id AND pm.provider = 'musicbrainz'
                         AND pm.provider_id <> '')
                     AND NOT EXISTS (
                       SELECT 1 FROM provider_queries q
                       WHERE q.item_id = i.id AND q.provider = 'musicbrainz'
                         AND q.query_type = 'title'
                         AND q.query = COALESCE(i.norm_artist, '') || '|' || i.norm_title
                         AND q.rev >= ?1))
                    -- HUB-9: local owes an answer. Gated on there
                    -- actually being something beside the media, or an
                    -- item with no cover and no .nfo would be re-selected
                    -- every run for a provider that stores nothing.
                    -- local's answer and what the scan can see disagree,
                    -- in either direction: a sidecar appeared and nobody
                    -- has read it, or the one it was built from is gone
                    -- and the answer now describes a deleted file.
                    OR (EXISTS (SELECT 1 FROM provider_metadata pl
                                 WHERE pl.item_id = i.id AND pl.provider = 'local'
                                   AND pl.provider_id <> '')
                        != (i.id IN (SELECT COALESCE(ch.parent_id, ch.id)
                                       FROM files f5 JOIN items ch ON ch.id=f5.item_id
                                      WHERE json_extract(f5.streams_json, '$.nfo') IS NOT NULL)))
                    OR (NOT EXISTS (SELECT 1 FROM provider_metadata pl
                                     WHERE pl.item_id = i.id AND pl.provider = 'local')
                        AND i.id IN (SELECT COALESCE(ch.parent_id, ch.id)
                                       FROM files f4 JOIN items ch ON ch.id=f4.item_id
                                      WHERE json_extract(f4.streams_json, '$.artwork') IS NOT NULL
                                         OR json_extract(f4.streams_json, '$.nfo') IS NOT NULL))
                    -- Work the chain still owes: a provider that refused
                    -- and is due again. Without this a rescheduled album
                    -- would sit in the queue forever (HUB-5).
                    OR EXISTS (
                      SELECT 1 FROM enrichment_queue q
                      WHERE q.item_id = i.id AND q.due_at <= unixepoch()))
             ORDER BY i.title",
        )
        .bind(crate::providers::QUERY_REV)
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
                norm_title: row.get("norm_title"),
                year: None,
                artist: row.get("artist"),
                norm_artist: row.get("norm_artist"),
                alt: None,
                existing: None,
                manual: false,
                known_aid: None,
                identified: false,
                owner: None,
            };
            // Same rule as the generic pass: the walker records misses,
            // never the caller — a fully-declined chain is not a miss.
            match providers.run_chain("music", registry.db(), &item).await {
                Some(_) => matched += 1,
                None => missed += 1,
            }
            if (n + 1) % 100 == 0 {
                tracing::info!(
                    done = n + 1,
                    total = albums.len(),
                    matched,
                    "music enrichment progress"
                );
            }
        }
        tracing::info!(matched, missed, "music enrichment complete");
        Ok(())
    }

    /// Strictly verified release-group search: the fold of title AND
    /// artist must match a candidate exactly — never guess.
    async fn musicbrainz_album(&self, title: &str, artist: &str) -> Result<Option<MbReleaseGroup>> {
        let query = format!(
            "releasegroup:\"{}\" AND artist:\"{}\"",
            title.replace('"', ""),
            artist.replace('"', "")
        );
        let encoded: String = query
            .bytes()
            .flat_map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    vec![b as char]
                }
                _ => format!("%{b:02X}").chars().collect(),
            })
            .collect();
        let url =
            format!("https://musicbrainz.org/ws/2/release-group?query={encoded}&fmt=json&limit=8");
        // MB allows one request per second per IP and answers 503 to
        // everything above it; the gate holds us to that, and carries
        // the identifying UA it also requires.
        let resp: serde_json::Value = self
            .http
            .send(self.http.get(&url))
            .await?
            .error_for_status()?
            .json()
            .await?;
        let groups = resp["release-groups"]
            .as_array()
            .cloned()
            .unwrap_or_default();
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
                        c["name"]
                            .as_str()
                            .map(|n| fold(n) == want_artist)
                            .unwrap_or(false)
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
    pub async fn select_anime_items(
        &self,
        registry: &Registry,
    ) -> Result<Vec<crate::providers::ItemRef>> {
        let rows = sqlx::query(
            "SELECT DISTINCT i.id, i.kind, i.title, i.norm_title, i.year,
                    i.artist, i.norm_artist,
                    m.provider, m.provider_id, COALESCE(m.manual, 0) AS manual,
                    a.anidb_id, a.anilist_id
             FROM items i
             LEFT JOIN item_match m ON m.item_id = i.id
             LEFT JOIN anime_ids a ON a.item_id = i.id
             -- Set membership, not a join: `s.item_id = i.id OR s.item_id IN
             -- (children of i)` as a JOIN condition re-runs a correlated
             -- subquery per (item x source) pair, which measured 103 s on
             -- 1.1k anime items. Resolving each source to its top-level
             -- item with COALESCE(parent_id, id) makes the set computable
             -- once, off the primary keys.
             JOIN collections own ON (own.module_id,own.collection_id)
                                  =(i.module_id,i.collection_id)
             WHERE i.kind IN ('movie','show') AND own.media_type='anime'
               AND (
                 -- The NAME question is owed: no anime identity stands
                 -- and the current title anchor was never asked. A
                 -- repaired title or a QUERY_REV bump re-opens this
                 -- automatically; misses never gate (HUB-5).
                 (NOT EXISTS (SELECT 1 FROM provider_metadata pm
                               WHERE pm.item_id = i.id AND pm.provider = 'anilist'
                                 AND pm.provider_id <> '')
                  AND NOT EXISTS (SELECT 1 FROM provider_queries q
                                   WHERE q.item_id = i.id AND q.provider = 'anime'
                                     AND q.query_type = 'title'
                                     AND q.query = i.norm_title || '|' || COALESCE(i.year, '')
                                     AND q.rev >= ?1))
                 -- A BRIDGE fetch is owed: identity mapped, no real
                 -- tail answer, that mapped id never fetched. (TMDB's
                 -- title-search-while-unowned rides the name branch
                 -- above — both anchors record in the same walk.)
                 OR (a.mapped_tmdb IS NOT NULL
                     AND NOT EXISTS (SELECT 1 FROM provider_metadata pm
                                      WHERE pm.item_id = i.id AND pm.provider = 'tmdb'
                                        AND pm.provider_id <> '')
                     AND NOT EXISTS (SELECT 1 FROM provider_queries q
                                      WHERE q.item_id = i.id AND q.provider = 'tmdb'
                                        AND q.query_type = 'mapped_id'
                                        AND q.query = CAST(a.mapped_tmdb AS TEXT)
                                        AND q.rev >= ?1))
                 OR (a.mapped_tvdb IS NOT NULL
                     AND NOT EXISTS (SELECT 1 FROM provider_metadata pm
                                      WHERE pm.item_id = i.id AND pm.provider = 'tvdb'
                                        AND pm.provider_id <> '')
                     AND NOT EXISTS (SELECT 1 FROM provider_queries q
                                      WHERE q.item_id = i.id AND q.provider = 'tvdb'
                                        AND q.query_type = 'mapped_id'
                                        AND q.query = CAST(a.mapped_tvdb AS TEXT)
                                        AND q.rev >= ?1))
                    -- HUB-9: local owes an answer. Gated on there
                    -- actually being something beside the media, or an
                    -- item with no cover and no .nfo would be re-selected
                    -- every run for a provider that stores nothing.
                    -- local's answer and what the scan can see disagree,
                    -- in either direction: a sidecar appeared and nobody
                    -- has read it, or the one it was built from is gone
                    -- and the answer now describes a deleted file.
                    OR (EXISTS (SELECT 1 FROM provider_metadata pl
                                 WHERE pl.item_id = i.id AND pl.provider = 'local'
                                   AND pl.provider_id <> '')
                        != (i.id IN (SELECT COALESCE(ch.parent_id, ch.id)
                                       FROM files f5 JOIN items ch ON ch.id=f5.item_id
                                      WHERE json_extract(f5.streams_json, '$.nfo') IS NOT NULL)))
                    OR (NOT EXISTS (SELECT 1 FROM provider_metadata pl
                                     WHERE pl.item_id = i.id AND pl.provider = 'local')
                        AND i.id IN (SELECT COALESCE(ch.parent_id, ch.id)
                                       FROM files f4 JOIN items ch ON ch.id=f4.item_id
                                      WHERE json_extract(f4.streams_json, '$.artwork') IS NOT NULL
                                         OR json_extract(f4.streams_json, '$.nfo') IS NOT NULL))
                 OR EXISTS (
                   SELECT 1 FROM enrichment_queue q
                   WHERE q.item_id = i.id AND q.due_at <= unixepoch())
                 -- Same shape, same reason: one pass over the unmapped
                 -- hashes rather than one per candidate item.
                 OR i.id IN (SELECT COALESCE(ch.parent_id,ch.id)
                               FROM files f JOIN file_bindings fb ON fb.file_id=f.id JOIN items ch ON ch.id=fb.item_id
                              WHERE f.ed2k IS NOT NULL
                                AND f.ed2k NOT IN (SELECT ed2k FROM ed2k_aid)))
             ORDER BY i.title",
        )
        .bind(crate::providers::QUERY_REV)
        .fetch_all(registry.db())
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| crate::providers::ItemRef {
                id: row.get("id"),
                kind: row.get("kind"),
                title: row.get("title"),
                norm_title: row.get("norm_title"),
                year: row.get("year"),
                artist: row.get("artist"),
                norm_artist: row.get("norm_artist"),
                alt: None,
                existing: row
                    .get::<Option<String>, _>("provider")
                    .zip(row.get::<Option<String>, _>("provider_id")),
                manual: row.get::<i64, _>("manual") != 0,
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
            return Ok(AnimeProvider {
                enricher: self.clone(),
                titles,
                lists,
            });
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
                match crate::anidb::Anidb::login(&self.data_dir, &user, &pass, key.as_deref()).await
                {
                    Ok(c) => Some(c),
                    Err(e) => {
                        tracing::warn!(
                            error = format!("{e:#}"),
                            "anidb login failed; title matching only"
                        );
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
            "SELECT a.item_id, a.anidb_id, i.kind FROM anime_ids a
             JOIN items i ON i.id = a.item_id
             WHERE a.anidb_id IS NOT NULL
               AND a.mapped_tvdb IS NULL AND a.mapped_tmdb IS NULL",
        )
        .fetch_all(registry.db())
        .await?;
        for row in stale {
            let aid = row.get::<i64, _>("anidb_id") as u32;
            let Some(m) = lists.by_anidb(aid) else {
                continue;
            };
            let tmdb = m.tmdb_for(&row.get::<String, _>("kind"));
            if m.tvdb_id.is_none() && tmdb.is_none() {
                continue;
            }
            sqlx::query("UPDATE anime_ids SET mapped_tvdb = ?, mapped_tmdb = ? WHERE item_id = ?")
                .bind(m.tvdb_id)
                .bind(tmdb)
                .bind(row.get::<String, _>("item_id"))
                .execute(registry.db())
                .await?;
            tracing::info!(aid, tvdb = ?m.tvdb_id, tmdb = ?tmdb, "mapped ids backfilled");
        }
        *self.anidb.lock().await = anidb;
        Ok(AnimeProvider {
            enricher: self.clone(),
            titles,
            lists,
        })
    }

    async fn enrich_anime(
        self: &Arc<Self>,
        registry: &Registry,
        providers: &Arc<crate::providers::ProviderSet>,
        items: Vec<crate::providers::ItemRef>,
    ) -> Result<()> {
        // Bind FIRST, for every identified show — not just the selected
        // ones. Selection gates network traffic; binding is idempotent
        // database work costing milliseconds, and gating it too meant a
        // binder rule change could never reach a show whose hashes were
        // already fully cached. The cached answers are the input, the
        // binding is derived from them: a disagreement is work owed
        // whether or not any provider needs asking.
        let identified: Vec<(String, i64)> = sqlx::query_as(
            "SELECT a.item_id, a.anidb_id FROM anime_ids a WHERE a.anidb_id IS NOT NULL",
        )
        .fetch_all(registry.db())
        .await?;
        for (show_id, aid) in &identified {
            match self
                .bind_hashed_episodes(registry.db(), show_id, *aid as u32)
                .await
            {
                Ok(moves) if !moves.is_empty() => {
                    tracing::info!(show = %show_id, moved = moves.len(),
                        "episodes re-bound to hash identity");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = format!("{e:#}"), "episode binding failed"),
            }
        }
        // Bare files likewise, UNCONDITIONALLY: they belong to no item,
        // so no selection can ever surface them — and this section runs
        // before the empty-selection return below, which is the second
        // time that return has quietly gated derivation work it must not.
        const HASH_LOOKUP_BUDGET: usize = 500;
        let mut lookups = 0usize;
        {
            let mut guard = self.anidb.lock().await;
            if let Some(client) = guard.as_mut() {
                match self
                    .resolve_bare_hashes(registry.db(), client, HASH_LOOKUP_BUDGET)
                    .await
                {
                    Ok(n) => lookups += n,
                    Err(e) => {
                        tracing::warn!(error = format!("{e:#}"), "bare-file hash lookups failed")
                    }
                }
            }
        }
        match self.bind_bare_files(registry.db()).await {
            Ok(n) if n > 0 => tracing::info!(bound = n, "bare files identified by hash"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = format!("{e:#}"), "bare-file binding failed"),
        }
        if items.is_empty() {
            return Ok(());
        }
        tracing::info!(items = items.len(), "anime enrichment starting");
        // HUB-30: per-run budget for episode-hash lookups, shared across
        // shows. The client already paces every packet to AniDB's flood
        // rule; this only bounds how long one run can spend, and the
        // remainder carries over — each answer is cached forever.
        let mut done = 0usize;
        for item in &items {
            // Exact-file episode identity, BEFORE the chain runs: the
            // bridge projection then writes titles onto the corrected
            // slots in this same pass. Shows identified for the first
            // time this run (no known aid yet) get theirs next pass.
            if let Some(aid) = item.known_aid {
                let asked = {
                    let mut guard = self.anidb.lock().await;
                    match guard.as_mut() {
                        Some(client) if lookups < HASH_LOOKUP_BUDGET => self
                            .resolve_episode_hashes(
                                registry.db(),
                                client,
                                &item.id,
                                HASH_LOOKUP_BUDGET - lookups,
                            )
                            .await
                            .unwrap_or_else(|e| {
                                tracing::warn!(
                                    error = format!("{e:#}"),
                                    "episode hash lookups failed; binding what is cached"
                                );
                                0
                            }),
                        _ => 0,
                    }
                };
                lookups += asked;
                match self
                    .bind_hashed_episodes(registry.db(), &item.id, aid)
                    .await
                {
                    Ok(moves) if !moves.is_empty() => {
                        tracing::info!(title = %item.title, moved = moves.len(),
                            "episodes re-bound to hash identity");
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = format!("{e:#}"), "episode binding failed"),
                }
            }
            // Same rule as the generic pass: the walker records misses,
            // never the caller — a fully-declined chain is not a miss.
            match providers.run_chain("anime", registry.db(), item).await {
                Some("settled") => {}
                Some(_) => done += 1,
                None => tracing::debug!(title = %item.title, "no anime identity this run"),
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
            "SELECT f.ed2k,f.size FROM files f
             WHERE f.ed2k IS NOT NULL
               AND (EXISTS(SELECT 1 FROM file_bindings fb WHERE fb.file_id=f.id AND (fb.item_id=?1 OR fb.item_id IN (SELECT id FROM items WHERE parent_id=?1))))
             -- Prefer a file AniDB has never been asked about: a cached
             -- miss on the alphabetically-first file must not block the
             -- siblings from ever being consulted.
             ORDER BY EXISTS (SELECT 1 FROM ed2k_aid c WHERE c.ed2k=f.ed2k),
                      f.path_rel LIMIT 1",
        )
        .bind(item_id)
        .fetch_optional(db)
        .await?
        else {
            return Ok(None);
        };
        let (ed2k, size) = (
            row.get::<String, _>("ed2k"),
            row.get::<i64, _>("size") as u64,
        );

        if let Some(cached) =
            sqlx::query_scalar::<_, Option<i64>>("SELECT aid FROM ed2k_aid WHERE ed2k = ?")
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
        sqlx::query(
            "INSERT OR REPLACE INTO ed2k_aid (ed2k, aid, updated_at) VALUES (?, ?, unixepoch())",
        )
        .bind(&ed2k)
        .bind(aid)
        .execute(db)
        .await?;
        Ok(aid)
    }

    /// Ask AniDB which episode each of a show's hashed files IS, up to
    /// `budget` lookups. Answers land in `ed2k_aid`, misses included, so
    /// every file is asked at most once ever; the client paces itself to
    /// AniDB's flood rule, and the budget only bounds how long one
    /// enrichment run can spend here — the remainder is picked up next
    /// run.
    async fn resolve_episode_hashes(
        &self,
        db: &sqlx::SqlitePool,
        client: &mut crate::anidb::Anidb,
        show_id: &str,
        budget: usize,
    ) -> Result<usize> {
        // A row with an aid but no epno predates 0042 and is re-asked
        // once; a NULL aid is a recorded miss and stays terminal.
        let files: Vec<(String, i64)> = sqlx::query_as(
            "SELECT f.ed2k,f.size FROM files f JOIN file_bindings fb ON fb.file_id=f.id JOIN items ep ON ep.id=fb.item_id
             WHERE ep.parent_id = ?1 AND f.ed2k IS NOT NULL
               -- Span episodes (batch files, 0045) answer to their
               -- range, not to a single-epno FILE reply: skip the ask.
               AND ep.episode_end IS NULL
               AND NOT EXISTS (SELECT 1 FROM ed2k_aid c
                                WHERE c.ed2k = f.ed2k
                                  AND (c.aid IS NULL OR c.epno IS NOT NULL))
             ORDER BY f.path_rel",
        )
        .bind(show_id)
        .fetch_all(db)
        .await?;
        let mut asked = 0;
        for (ed2k, size) in files.iter().take(budget) {
            let hit = client.file_by_ed2k(*size as u64, ed2k).await?;
            sqlx::query(
                "INSERT OR REPLACE INTO ed2k_aid
                   (ed2k, aid, eid, epno, gid, group_name, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, unixepoch())",
            )
            .bind(ed2k)
            .bind(hit.as_ref().map(|h| h.aid))
            .bind(hit.as_ref().map(|h| h.eid))
            .bind(hit.as_ref().map(|h| h.epno.clone()))
            .bind(hit.as_ref().map(|h| h.gid))
            .bind(hit.as_ref().map(|h| h.group_name.clone()))
            .execute(db)
            .await?;
            asked += 1;
        }
        if files.len() > budget {
            tracing::info!(
                show = show_id,
                remaining = files.len() - budget,
                "episode hash lookups continue next run"
            );
        }
        Ok(asked)
    }

    /// Ask AniDB about hashed files in anime collections that are bound
    /// to NOTHING — the NCOP/NCED extras and movie files whose names
    /// carry no episode shape. The old code's comment promised "ed2k
    /// matching will identify those precisely later" and later never
    /// came: the per-show resolver only walks bound files, so bare files
    /// were invisible to it, to browse, and to AniDB alike.
    async fn resolve_bare_hashes(
        &self,
        db: &sqlx::SqlitePool,
        client: &mut crate::anidb::Anidb,
        budget: usize,
    ) -> Result<usize> {
        let files: Vec<(String, i64)> = sqlx::query_as(
            "SELECT f.ed2k, f.size FROM files f
             JOIN collections col ON (col.module_id, col.collection_id)
                                   = (f.module_id, f.collection_id)
             WHERE col.media_type = 'anime' AND f.ed2k IS NOT NULL
               AND NOT EXISTS(SELECT 1 FROM file_bindings fb WHERE fb.file_id=f.id)
               AND NOT EXISTS (SELECT 1 FROM ed2k_aid c
                                WHERE c.ed2k = f.ed2k
                                  AND (c.aid IS NULL OR c.epno IS NOT NULL))
             ORDER BY f.path_rel",
        )
        .fetch_all(db)
        .await?;
        let mut asked = 0;
        for (ed2k, size) in files.iter().take(budget) {
            let hit = client.file_by_ed2k(*size as u64, ed2k).await?;
            sqlx::query(
                "INSERT OR REPLACE INTO ed2k_aid
                   (ed2k, aid, eid, epno, gid, group_name, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, unixepoch())",
            )
            .bind(ed2k)
            .bind(hit.as_ref().map(|h| h.aid))
            .bind(hit.as_ref().map(|h| h.eid))
            .bind(hit.as_ref().map(|h| h.epno.clone()))
            .bind(hit.as_ref().map(|h| h.gid))
            .bind(hit.as_ref().map(|h| h.group_name.clone()))
            .execute(db)
            .await?;
            asked += 1;
        }
        Ok(asked)
    }

    /// Bind bare anime-collection files whose cached hash answer names
    /// an identity the catalogue holds: an episode slot under the show
    /// matched to that aid, or the movie item itself. Pure database
    /// work; a file whose aid matches nothing stays bare and is logged.
    pub async fn bind_bare_files(&self, db: &sqlx::SqlitePool) -> Result<usize> {
        let rows = sqlx::query(
            "SELECT f.id AS source_id,f.module_id,f.collection_id,f.path_rel,c.aid,c.epno
             FROM files f
             JOIN collections col ON (col.module_id, col.collection_id)
                                   = (f.module_id, f.collection_id)
             JOIN ed2k_aid c ON c.ed2k = f.ed2k
             WHERE col.media_type = 'anime'
               AND c.aid IS NOT NULL AND c.epno IS NOT NULL
               AND NOT EXISTS(SELECT 1 FROM file_bindings fb WHERE fb.file_id=f.id)
             ORDER BY f.path_rel",
        )
        .fetch_all(db)
        .await?;
        let mut bound = 0;
        for r in rows {
            let (aid, epno) = (r.get::<i64, _>("aid"), r.get::<String, _>("epno"));
            let path: String = r.get("path_rel");
            let module_id: String = r.get("module_id");
            let collection_id: String = r.get("collection_id");
            let owner_row = sqlx::query_as::<_, (String, String)>(
                "SELECT i.id,i.kind FROM anime_ids a JOIN items i ON i.id=a.item_id
                  WHERE a.anidb_id=? AND i.module_id=? AND i.collection_id=? LIMIT 1",
            )
            .bind(aid)
            .bind(&module_id)
            .bind(&collection_id)
            .fetch_optional(db)
            .await?;
            let (owner, kind) = match owner_row {
                Some(pair) => pair,
                // Nothing owns this aid. If AniDB says it is a MOVIE,
                // the item is minted (or an aid-less twin adopted) from
                // the provider's answer — the only place an item
                // originates from an answer rather than a filename,
                // agreed 2026-07-28, because a yearless "Akira.mkv" can
                // never earn an item any other way. A series stays bare:
                // one stray file must not scaffold a show.
                None => match self
                    .mint_movie_for_aid(db, aid as u32, &module_id, &collection_id)
                    .await
                {
                    Ok(Some(id)) => (id, "movie".to_string()),
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::debug!(path = %path, aid, error = format!("{e:#}"),
                            "could not establish what this aid is; leaving bare");
                        continue;
                    }
                },
            };
            let target = match kind.as_str() {
                // The file IS (part of) the movie itself.
                "movie" => owner.clone(),
                _ => {
                    let slot = match parse_epno(&epno) {
                        Some(Epno::Regular(n)) => (None::<i64>, n),
                        Some(Epno::Zero(n)) => (Some(0), n),
                        None => {
                            tracing::debug!(path = %path, epno = %epno, "unrecognised epno");
                            continue;
                        }
                    };
                    let existing: Option<String> = sqlx::query_scalar(
                        "SELECT id FROM items WHERE parent_id = ?1 AND season IS ?2 AND episode = ?3",
                    )
                    .bind(&owner)
                    .bind(slot.0)
                    .bind(slot.1)
                    .fetch_optional(db)
                    .await?;
                    match existing {
                        Some(id) => id,
                        None => {
                            let id = ulid::Ulid::generate().to_string();
                            let stem = path
                                .rsplit('/')
                                .next()
                                .unwrap_or(&path)
                                .rsplit_once('.')
                                .map(|(s, _)| s)
                                .unwrap_or(&path)
                                .to_string();
                            sqlx::query(
                                "INSERT INTO items
                                   (id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id)
                                 VALUES (?,'episode',?,?,?,?,?,?,?)",
                            )
                            .bind(&id)
                            .bind(&stem)
                            .bind(kahawai_core::names::normalize_title(&stem))
                            .bind(&owner)
                            .bind(slot.0)
                            .bind(slot.1)
                            .bind(&module_id)
                            .bind(&collection_id)
                            .execute(db)
                            .await?;
                            id
                        }
                    }
                }
            };
            sqlx::query("UPDATE files SET item_id=? WHERE id=?")
                .bind(&target)
                .bind(r.get::<i64, _>("source_id"))
                .execute(db)
                .await?;
            tracing::info!(path = %path, epno = %epno, "bare file identified by hash and bound");
            bound += 1;
        }
        Ok(bound)
    }

    /// A movie item for an aid nothing owns — adopted if an aid-less
    /// twin exists under the same normalized title and year (a TMDB
    /// title-match of the same film), minted from AniDB's answer
    /// otherwise. Returns None for non-movie types.
    async fn mint_movie_for_aid(
        &self,
        db: &sqlx::SqlitePool,
        aid: u32,
        module_id: &str,
        collection_id: &str,
    ) -> Result<Option<String>> {
        let info = crate::anime::anidb_anime_info(&self.http, &self.data_dir, aid).await?;
        // Movie-shaped: AniDB's Movie type, or a single-episode OVA/Web
        // entry — one sitting, no series structure to invent. Multi-
        // episode series types deliberately stay bare (never conjure a
        // show around one stray file).
        let movie_shaped = info.kind == "Movie"
            || (matches!(info.kind.as_str(), "OVA" | "Web") && info.episode_count == Some(1));
        if !movie_shaped {
            tracing::debug!(aid, kind = %info.kind, "not movie-shaped; leaving bare");
            return Ok(None);
        }
        let norm = kahawai_core::names::normalize_title(&info.title);
        let twin: Option<String> = sqlx::query_scalar(
            "SELECT i.id FROM items i WHERE i.module_id=?1 AND i.collection_id=?2
                AND i.kind='movie' AND i.norm_title=?3 AND i.year IS ?4
                AND NOT EXISTS(SELECT 1 FROM anime_ids a
                                WHERE a.item_id=i.id AND a.anidb_id IS NOT NULL)",
        )
        .bind(module_id)
        .bind(collection_id)
        .bind(&norm)
        .bind(info.year)
        .fetch_optional(db)
        .await?;
        let id = match twin {
            Some(id) => {
                tracing::info!(aid, title = %info.title, item = %id,
                    "hash identity adopted an existing movie");
                id
            }
            None => {
                let id = ulid::Ulid::generate().to_string();
                sqlx::query(
                    "INSERT INTO items
                       (id,kind,title,norm_title,year,module_id,collection_id)
                     VALUES (?,'movie',?,?,?,?,?)",
                )
                .bind(&id)
                .bind(&info.title)
                .bind(&norm)
                .bind(info.year)
                .bind(module_id)
                .bind(collection_id)
                .execute(db)
                .await?;
                tracing::info!(aid, title = %info.title, year = ?info.year,
                    "movie minted from hash identity");
                id
            }
        };
        sqlx::query(
            "INSERT INTO anime_ids (item_id, anidb_id) VALUES (?, ?)
             ON CONFLICT (item_id) DO UPDATE SET anidb_id = excluded.anidb_id",
        )
        .bind(&id)
        .bind(aid)
        .execute(db)
        .await?;
        Ok(Some(id))
    }

    /// Bind each hashed file to the episode AniDB says it IS (HUB-30a:
    /// the hash states what the file is, so on disagreement the hash
    /// wins). Pure database work — the lookups above already happened.
    ///
    /// Deliberately narrow where the numbering spaces are not the same:
    /// `epno` is scoped to ONE AniDB entry, so only files whose aid
    /// matches their show's are touched (a mismatch usually means AniDB
    /// splits the series into per-season entries; logged, left alone).
    /// Regular numbers are enforced only for absolute-keyed episodes —
    /// the anime norm, and the space AniDB numbers in. Every TYPED
    /// number — specials, credits, trailers, parodies, other — is
    /// enforced unconditionally into season 0's banded layout (see
    /// [`Epno`]): these are precisely the files name-parsing cannot
    /// place, and a credits reel filename-squatting on an episode slot
    /// is an artifact of the numbering the hash exists to correct.
    pub async fn bind_hashed_episodes(
        &self,
        db: &sqlx::SqlitePool,
        show_id: &str,
        show_aid: u32,
    ) -> Result<Vec<EpisodeRebind>> {
        let rows = sqlx::query(
            "SELECT f.id AS source_id,f.path_rel AS source_path,ep.id AS item_id,
                    ep.title,ep.norm_title,ep.season,ep.episode,c.aid,c.epno,c.eid
             FROM files f JOIN file_bindings fb ON fb.file_id=f.id JOIN items ep ON ep.id=fb.item_id
             JOIN ed2k_aid c ON c.ed2k = f.ed2k
             WHERE ep.parent_id = ?1 AND c.aid IS NOT NULL AND c.epno IS NOT NULL
               -- Span episodes (batch files, 0045) are their own truth;
               -- a single-epno answer must not collapse the range.
               AND ep.episode_end IS NULL
             ORDER BY f.path_rel",
        )
        .bind(show_id)
        .fetch_all(db)
        .await?;

        let mut moves = Vec::new();
        let mut elsewhere: Vec<SlotOccupant> = Vec::new();
        for r in rows {
            let (aid, epno) = (r.get::<i64, _>("aid") as u32, r.get::<String, _>("epno"));
            let path: String = r.get("source_path");
            if aid != show_aid {
                // Declined by measurement, not by omission: AniDB splits a
                // televised series into an entry per season, so most files
                // whose aid differs are already in the right slot and
                // moving them would break it (HUB-30, amended 2026-08-06).
                // One question about them is still open — whether this file
                // SHARES its slot with a different episode — so hand it to
                // the collision pass below.
                tracing::debug!(path = %path, file_aid = aid, show_aid,
                    "file belongs to a different anidb entry; not rebinding");
                elsewhere.push(SlotOccupant {
                    item_id: r.get("item_id"),
                    source_id: r.get("source_id"),
                    season: r.get::<Option<i64>, _>("season"),
                    episode: r.get::<i64, _>("episode"),
                    eid: r.get::<Option<i64>, _>("eid"),
                    path,
                    epno,
                });
                continue;
            }
            let (cur_season, cur_ep) = (
                r.get::<Option<i64>, _>("season"),
                r.get::<i64, _>("episode"),
            );
            let target = match parse_epno(&epno) {
                Some(Epno::Regular(n)) => {
                    // Meaningful where the show's own numbering IS
                    // AniDB's: absolute-keyed episodes — and season 0,
                    // which is OUR OWN speculative parking (a name-
                    // guessed OVA slot); a regular hash number reclaims
                    // a file parked there. Real SxxEyy keys stay.
                    if cur_season.is_some() && cur_season != Some(0) {
                        continue;
                    }
                    (None, n)
                }
                Some(Epno::Zero(n)) => (Some(0), n),
                None => {
                    tracing::warn!(path = %path, epno = %epno,
                        "unrecognised anidb epno; leaving name-derived binding");
                    continue;
                }
            };
            if (cur_season, cur_ep) == (target.0, target.1) {
                continue;
            }

            let item_id: String = r.get("item_id");
            let mut tx = db.begin().await?;
            let existing: Option<String> = sqlx::query_scalar(
                "SELECT id FROM items WHERE parent_id = ?1 AND season IS ?2 AND episode = ?3",
            )
            .bind(show_id)
            .bind(target.0)
            .bind(target.1)
            .fetch_optional(&mut *tx)
            .await?;
            let target_id = match existing {
                Some(id) => id,
                None => {
                    let id = ulid::Ulid::generate().to_string();
                    // The file's own title travels with it: it described
                    // this content, whatever number it wore.
                    sqlx::query(
                        "INSERT INTO items
                           (id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id)
                         SELECT ?,'episode',?,?,?,?,?,module_id,collection_id
                           FROM items WHERE id=?",
                    )
                    .bind(&id)
                    .bind(r.get::<String, _>("title"))
                    .bind(r.get::<String, _>("norm_title"))
                    .bind(show_id)
                    .bind(target.0)
                    .bind(target.1)
                    .bind(show_id)
                    .execute(&mut *tx)
                    .await?;
                    id
                }
            };
            sqlx::query("UPDATE files SET item_id=? WHERE id=?")
                .bind(&target_id)
                .bind(r.get::<i64, _>("source_id"))
                .execute(&mut *tx)
                .await?;
            // Watch state follows the FILE — the user watched this
            // content under whatever number it was misfiled as.
            sqlx::query(
                "UPDATE watch_state SET item_id = ?1
                 WHERE item_id = ?2
                   AND NOT EXISTS (SELECT 1 FROM watch_state w2
                                    WHERE w2.item_id = ?1 AND w2.user_id = watch_state.user_id)",
            )
            .bind(&target_id)
            .bind(&item_id)
            .execute(&mut *tx)
            .await?;
            // A misnumbered episode item left with no sources is a ghost
            // row in the season view; its provider answers describe the
            // NUMBER and go with it (the projection refills the target).
            sqlx::query(
                "DELETE FROM items
                 WHERE id = ?1 AND kind = 'episode'
                   AND NOT EXISTS(SELECT 1 FROM playable_sources s WHERE s.item_id=?1)",
            )
            .bind(&item_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;

            tracing::info!(path = %path,
                from = %fmt_slot(cur_season, cur_ep), to = %fmt_slot(target.0, target.1),
                "hash-corrected episode binding");
            moves.push(EpisodeRebind {
                path,
                from: (cur_season, cur_ep),
                to: (target.0, target.1),
            });
        }
        moves.extend(self.break_slot_collisions(db, show_id, elsewhere).await?);
        Ok(moves)
    }

    /// Files sharing one episode slot whose hashes name DIFFERENT AniDB
    /// episodes are different episodes, and each gets its own item.
    ///
    /// Several sources on a slot is otherwise the ordinary two-copies
    /// case — a 1080p and a 720p rip of one episode share an eid — so the
    /// eid is the test, and the count is not.
    ///
    /// The numbering cannot come from the hash here, which is why this is
    /// a separate pass rather than part of the loop above: these files
    /// belong to another AniDB entry, whose episode 1 is not this show's
    /// episode 1. Megazone 23 is the case it was written for — `pt.03-a`
    /// is episode 1 of aid 3545 while `pt.01` is episode 1 of aid 1729 —
    /// so claiming that number would land on a correct slot. Instead the
    /// file AniDB numbers first keeps the contested slot and the rest go
    /// to free numbers in the SAME season, in eid order: distinct and
    /// stable, claiming nothing about a keyspace this show does not use.
    async fn break_slot_collisions(
        &self,
        db: &sqlx::SqlitePool,
        show_id: &str,
        occupants: Vec<SlotOccupant>,
    ) -> Result<Vec<EpisodeRebind>> {
        use std::collections::{HashMap, HashSet};
        let mut by_slot: HashMap<String, Vec<SlotOccupant>> = HashMap::new();
        for o in occupants {
            by_slot.entry(o.item_id.clone()).or_default().push(o);
        }

        let mut moves = Vec::new();
        for (_slot, mut group) in by_slot {
            let eids: HashSet<Option<i64>> = group.iter().map(|o| o.eid).collect();
            // One episode in several copies, or an answer that cannot tell
            // them apart: nothing to break.
            if group.len() < 2 || eids.len() < 2 || eids.contains(&None) {
                continue;
            }
            // AniDB's own order decides who keeps the slot; the path breaks
            // ties so a rescan cannot shuffle them.
            group.sort_by(|a, b| (a.eid, a.path.as_str()).cmp(&(b.eid, b.path.as_str())));
            for o in group.into_iter().skip(1) {
                // The slot's OWN season, not season 0: an anime show is
                // normally absolute-numbered, so its episodes carry a NULL
                // season (HUB-31), and parking in 0 both invents a season
                // the show does not use and starts numbering at 1 — on top
                // of a real episode 1. Measured on the live database, which
                // the fixture had not reproduced.
                let episode: i64 = sqlx::query_scalar(
                    "SELECT COALESCE(MAX(episode), 0) + 1 FROM items
                      WHERE parent_id = ?1 AND season IS ?2",
                )
                .bind(show_id)
                .bind(o.season)
                .fetch_one(db)
                .await?;
                // Its own name, because the slot's title described the
                // episode it is being separated from.
                let title = std::path::Path::new(&o.path)
                    .file_stem()
                    .map(|st| st.to_string_lossy().to_string())
                    .unwrap_or_else(|| o.path.clone());

                let mut tx = db.begin().await?;
                let id = ulid::Ulid::generate().to_string();
                sqlx::query(
                    "INSERT INTO items
                       (id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id)
                     SELECT ?,'episode',?,?,?,?,?,module_id,collection_id
                       FROM items WHERE id=?",
                )
                .bind(&id)
                .bind(&title)
                .bind(kahawai_core::names::normalize_title(&title))
                .bind(show_id)
                .bind(o.season)
                .bind(episode)
                .bind(show_id)
                .execute(&mut *tx)
                .await?;
                sqlx::query("UPDATE files SET item_id=? WHERE id=?")
                    .bind(&id)
                    .bind(o.source_id)
                    .execute(&mut *tx)
                    .await?;
                // Watch state follows the FILE, as everywhere else here:
                // the user watched this content under the shared number.
                sqlx::query(
                    "UPDATE watch_state SET item_id = ?1
                      WHERE item_id = ?2
                        AND NOT EXISTS (SELECT 1 FROM watch_state w2
                                         WHERE w2.item_id = ?1
                                           AND w2.user_id = watch_state.user_id)",
                )
                .bind(&id)
                .bind(&o.item_id)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;

                tracing::info!(path = %o.path, eid = ?o.eid, epno = %o.epno,
                    from = %fmt_slot(o.season, o.episode), to = %fmt_slot(o.season, episode),
                    "split a slot shared by different anidb episodes");
                moves.push(EpisodeRebind {
                    path: o.path,
                    from: (o.season, o.episode),
                    to: (o.season, episode),
                });
            }
        }
        Ok(moves)
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
            self.store_anime(db, item_id, kind, &media, Some(aid), Some(m))
                .await?;
            tracing::info!(
                title,
                anilist = media.id,
                anidb = aid,
                "anime matched (ed2k exact)"
            );
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
            let Some((aid, media)) = chosen else {
                return Ok(false);
            };
            (media, Some(aid))
        };

        let mapping = anidb_id.and_then(|aid| lists.by_anidb(aid));
        self.store_anime(db, item_id, kind, &media, anidb_id, mapping)
            .await?;
        tracing::info!(title, anilist = media.id, anidb = anidb_id, "anime matched");
        Ok(true)
    }

    /// Persist an AniList match: metadata upsert + relations graph.
    pub async fn store_anime(
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
                cast_json: None,
            },
        )
        .await?;
        // Identity columns the merge never touches: they say what this
        // anime IS and how it bridges to the other services.
        sqlx::query(
            "INSERT INTO anime_ids (item_id, anidb_id, anilist_id, mapped_tvdb, mapped_tmdb)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (item_id) DO UPDATE SET
               anidb_id = excluded.anidb_id,
               anilist_id = excluded.anilist_id,
               mapped_tvdb = COALESCE(excluded.mapped_tvdb, anime_ids.mapped_tvdb),
               mapped_tmdb = COALESCE(excluded.mapped_tmdb, anime_ids.mapped_tmdb)",
        )
        .bind(item_id)
        .bind(anidb_id)
        .bind(media.id)
        .bind(mapping.and_then(|m| m.tvdb_id))
        .bind(mapping.and_then(|m| m.tmdb_for(kind)))
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
                    "SEQUEL"
                        | "PREQUEL"
                        | "SIDE_STORY"
                        | "ALTERNATIVE"
                        | "PARENT"
                        | "FULL_STORY"
                        | "SUMMARY"
                        | "SPIN_OFF"
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
                .bind(
                    node.title
                        .english
                        .clone()
                        .or_else(|| node.title.romaji.clone()),
                )
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
    /// Rebuild the anime bridge ids from answers already on disk.
    ///
    /// `anime_ids` is derived state, but it used to be written only at
    /// match time — so losing it lost it for good: never-ask-twice sees
    /// the provider answers still there, skips the provider, and the id
    /// never comes back. Both columns are recoverable without contacting
    /// anyone: the AniList id IS that provider's `provider_id`, and the
    /// AniDB id is in `ed2k_aid`, keyed by the same first-file hash the
    /// lookup used. `mapped_tvdb`/`mapped_tmdb` then heal themselves
    /// through the existing backfill, which keys on the AniDB id.
    ///
    /// Fills holes only — COALESCE keeps whatever is already there, so a
    /// human's correction or a fresher answer always wins over a
    /// reconstruction.
    pub async fn rebuild_anime_ids(
        db: &sqlx::SqlitePool,
        lists: Option<&crate::anime::AnimeLists>,
    ) -> Result<u64> {
        let done = sqlx::query(
            "INSERT INTO anime_ids (item_id, anidb_id, anilist_id)
             SELECT item_id, aid, anilist FROM (
               SELECT i.id AS item_id,
                 (SELECT ea.aid FROM ed2k_aid ea
                    JOIN files f ON f.ed2k = ea.ed2k
                   WHERE EXISTS(SELECT 1 FROM file_bindings fb WHERE fb.file_id=f.id
                          AND (fb.item_id=i.id OR fb.item_id IN (SELECT id FROM items WHERE parent_id=i.id)))
                     AND ea.aid IS NOT NULL
                   -- Same file the identification used, so the same answer.
                   ORDER BY f.path_rel LIMIT 1) AS aid,
                 (SELECT CAST(pm.provider_id AS INTEGER) FROM provider_metadata pm
                   WHERE pm.item_id = i.id AND pm.provider = 'anilist'
                     AND pm.provider_id <> '') AS anilist
               FROM items i
               WHERE i.kind IN ('movie', 'show')
             )
             WHERE aid IS NOT NULL OR anilist IS NOT NULL
             ON CONFLICT (item_id) DO UPDATE SET
               anidb_id = COALESCE(anime_ids.anidb_id, excluded.anidb_id),
               anilist_id = COALESCE(anime_ids.anilist_id, excluded.anilist_id)",
        )
        .execute(db)
        .await?
        .rows_affected();

        // Whatever the hash could not answer for, the mapping can: an
        // anime identified by TITLE never had its aid recorded anywhere
        // except the column being rebuilt, but the AniList id survives as
        // that provider's answer, and anime-lists maps one to the other.
        // That is a recorded fact rather than a re-run of the heuristic —
        // re-matching a title against a dump that has moved on can land
        // somewhere the original never did.
        let Some(lists) = lists else { return Ok(done) };
        let orphans = sqlx::query(
            "SELECT item_id, anilist_id FROM anime_ids
              WHERE anidb_id IS NULL AND anilist_id IS NOT NULL",
        )
        .fetch_all(db)
        .await?;
        let mut healed = 0u64;
        for row in orphans {
            let anilist: i64 = row.get("anilist_id");
            let aids = lists.reverse("anilist", &anilist.to_string());
            // Only an unambiguous mapping. Two aids behind one AniList
            // entry is a split-season case, and guessing which half an
            // item is would be worse than leaving the column empty.
            let [aid] = aids[..] else { continue };
            sqlx::query("UPDATE anime_ids SET anidb_id = ? WHERE item_id = ? AND anidb_id IS NULL")
                .bind(aid)
                .bind(row.get::<String, _>("item_id"))
                .execute(db)
                .await?;
            healed += 1;
        }
        Ok(done + healed)
    }

    /// How much of the billing to keep. Enough for "who's in this?" on a
    /// detail page; the tail of a big film is crowd and stand-ins.
    const CAST_LIMIT: usize = 15;

    /// HUB-6: one TMDB details request per item fills original_language,
    /// genres and cast together. `append_to_response=credits` folds the
    /// credits sub-request into the SAME call (verified against the live
    /// API: /movie/63 returns genres and 68 cast entries in one response),
    /// so adding cast costs no extra provider traffic — which is the only
    /// reason it is affordable at all under HUB-7 pacing.
    async fn backfill_details(self: &Arc<Self>, registry: &Registry, tmdb_key: &str) -> Result<()> {
        let rows = sqlx::query(
            "SELECT pm.item_id, i.kind, pm.provider_id AS tmdb_id
             FROM provider_metadata pm JOIN items i ON i.id = pm.item_id
             WHERE pm.provider = 'tmdb' AND pm.provider_id != ''
               AND i.kind IN ('movie', 'show')
               -- Any of the three missing is worth the one request that
               -- fills all three.
               AND (pm.original_language IS NULL OR pm.genres IS NULL
                    OR pm.cast_json IS NULL)",
        )
        .fetch_all(registry.db())
        .await?;
        if rows.is_empty() {
            return Ok(());
        }
        tracing::info!(items = rows.len(), "tmdb details backfill starting");
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
                struct Named {
                    name: String,
                }
                #[derive(Deserialize)]
                struct CastMember {
                    name: String,
                    #[serde(default)]
                    character: Option<String>,
                }
                #[derive(Deserialize)]
                struct Credits {
                    #[serde(default)]
                    cast: Vec<CastMember>,
                }
                #[derive(Deserialize)]
                struct Details {
                    #[serde(default)]
                    original_language: Option<String>,
                    #[serde(default)]
                    genres: Vec<Named>,
                    #[serde(default)]
                    credits: Option<Credits>,
                }
                let req = this
                    .http
                    .get(format!("https://api.themoviedb.org/3/{path}/{tmdb_id}"))
                    .query(&[("api_key", key.as_str()), ("append_to_response", "credits")]);
                let det = match this
                    .http
                    .send(req)
                    .await
                    .and_then(|r| Ok(r.error_for_status()?))
                {
                    Ok(resp) => match resp.json::<Details>().await {
                        Ok(det) => det,
                        Err(e) => {
                            tracing::debug!(tmdb_id, error = %e, "details decode failed");
                            return;
                        }
                    },
                    Err(e) => {
                        tracing::debug!(tmdb_id, error = %e, "details fetch failed");
                        return; // transient: stays NULL, retried next run
                    }
                };
                let lang = det.original_language.unwrap_or_default();
                let genres: Vec<String> = det.genres.into_iter().map(|g| g.name).collect();
                // Billing order, top of the bill only: TMDB returns 68 for
                // a 1995 film and no UI shows a cast of 68. Empty stays
                // empty rather than becoming "[]", so the row still reads
                // as unanswered and is retried.
                let cast: Vec<serde_json::Value> = det
                    .credits
                    .map(|c| c.cast)
                    .unwrap_or_default()
                    .into_iter()
                    .take(Self::CAST_LIMIT)
                    .map(|p| serde_json::json!({ "name": p.name, "character": p.character }))
                    .collect();
                let _ = sqlx::query(
                    "UPDATE provider_metadata
                        SET original_language = ?,
                            genres = COALESCE(?, genres),
                            cast_json = COALESCE(?, cast_json),
                            updated_at = unixepoch()
                      WHERE item_id = ? AND provider = 'tmdb'",
                )
                .bind(&lang)
                .bind((!genres.is_empty()).then(|| serde_json::to_string(&genres).unwrap()))
                .bind((!cast.is_empty()).then(|| serde_json::to_string(&cast).unwrap()))
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
        tvdb: Option<&TvdbCreds>,
    ) -> Result<()> {
        // Fetch for shows with metadata-less episodes, plus absolute-
        // numbered shows whose episodes lack the HUB-31 season
        // projection (backfill). ponytail: a show TVDB never curated
        // absolute numbers for re-fetches each run — a few cached-token
        // pages per anime show; revisit if a library full of them appears.
        let shows = sqlx::query(
            "SELECT i.id, a.mapped_tvdb, a.mapped_tmdb, a.anidb_id
             FROM items i
             JOIN item_match m ON m.item_id = i.id AND m.provider_id != ''
             LEFT JOIN anime_ids a ON a.item_id = i.id
             WHERE i.kind = 'show'
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
                 JOIN provider_metadata em ON em.item_id = e.id
                 WHERE e.parent_id = i.id AND e.season IS NULL
                   AND em.provider IN ('tmdb', 'tvdb')
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
            .map(|r| {
                (
                    r.get::<String, _>("provider"),
                    r.get::<String, _>("provider_id"),
                )
            })
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
                let tvdb = tvdb.cloned();
                let db = registry.db().clone();
                let sem = sem.clone();
                let show = show_id.clone();
                tasks.spawn(async move {
                    let _permit = sem.acquire().await;
                    if let Err(e) = this
                        .enrich_show_episodes(&db, &show, &provider, &pid, &key, tvdb.as_ref(), aid)
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
        tvdb: Option<&TvdbCreds>,
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
        let absolute = eps
            .iter()
            .any(|r| r.get::<Option<i64>, _>("season").is_none());

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
                let creds = tvdb.context("tvdb-matched show but no tvdb key configured")?;
                let token = self.tvdb_token(creds).await?;
                for e in self.tvdb_episodes_english(&token, pid, "default").await? {
                    if let (Some(s), n) = (e.season, e.episode) {
                        by_key.insert((Some(s), n), e);
                    }
                }
            }
            ("tvdb", true) => {
                let creds = tvdb.context("tvdb-matched show but no tvdb key configured")?;
                let token = self.tvdb_token(creds).await?;
                let eps_abs = self.tvdb_episodes_english(&token, pid, "absolute").await?;
                for (i, e) in eps_abs.into_iter().enumerate() {
                    let n = e.absolute.unwrap_or(i as i64 + 1);
                    by_key.insert((None, n), e);
                }
                // The default order carries absoluteNumber where TVDB
                // curates it (usual for anime) — that join IS the
                // season projection.
                for e in self
                    .tvdb_episodes(&token, pid, "default", None)
                    .await
                    .unwrap_or_default()
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
                        if let std::collections::hash_map::Entry::Vacant(e) = fetched.entry(s) {
                            let list = self.tmdb_season(tmdb_key, pid, s).await.unwrap_or_default();
                            e.insert(list);
                        }
                        if let Some(e) = fetched[&s].iter().find(|e| e.episode == n).cloned() {
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
        for r in &eps {
            let key = (
                r.get::<Option<i64>, _>("season"),
                r.get::<i64, _>("episode"),
            );
            let Some(e) = by_key.get(&key) else { continue };
            let item_id: String = r.get("id");
            // Season projection applies to absolute-numbered rows only.
            let p = if key.0.is_none() {
                proj.get(&key.1)
            } else {
                None
            };
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
            )
            .await?;
            // The season/absolute projection is identity, not
            // description: the merge never touches it (HUB-31).
            sqlx::query(
                "UPDATE provider_metadata SET proj_season = ?, proj_episode = ?
                 WHERE item_id = ? AND provider = ?",
            )
            .bind(p.map(|v| v.0))
            .bind(p.map(|v| v.1))
            .bind(&item_id)
            .bind(provider)
            .execute(db)
            .await?;
            wrote += 1;
        }
        // Episodes the provider had nothing for: record the attempt, or
        // this show is selected again on every single run (it was — nine
        // times in one day, re-fetching whole episode lists each time).
        let mut unmatched = 0;
        for r in &eps {
            let key = (
                r.get::<Option<i64>, _>("season"),
                r.get::<i64, _>("episode"),
            );
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
            )
            .await?;
            unmatched += 1;
        }
        tracing::info!(
            show = show_id,
            episodes = wrote,
            unmatched,
            "episode metadata stored"
        );
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
        item: Option<&str>,
    ) -> Result<serde_json::Value> {
        // Which identity space does this item live in? An anime-
        // collection item wants AniList candidates ahead of the
        // generic providers at equal relevance — and vice versa.
        let anime_first = match item {
            Some(id) => sqlx::query_scalar::<_, i64>(
                "SELECT EXISTS(SELECT 1 FROM items i JOIN collections c
                    ON (c.module_id,c.collection_id)=(i.module_id,i.collection_id)
                  WHERE i.id=?1 AND c.media_type='anime')",
            )
            .bind(id)
            .fetch_one(registry.db())
            .await
            .map(|v| v != 0)
            .unwrap_or(false),
            None => false,
        };
        let mut out: Vec<serde_json::Value> = Vec::new();
        if let Some(key) = registry.get_setting(TMDB_KEY_SETTING).await? {
            match self.search(&key, kind, query, year).await {
                Ok(cands) => out.extend(cands.iter().map(|c| {
                    let mut v = serde_json::to_value(c).unwrap();
                    v["provider"] = serde_json::json!("tmdb");
                    // The search hit one typed endpoint, so the format
                    // is the endpoint's, not per-candidate data.
                    v["format"] =
                        serde_json::json!(if kind == "movie" { "Movie" } else { "Series" });
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
        if let Some(key) = registry.get_setting(TVDB_KEY_SETTING).await? {
            let creds = TvdbCreds {
                key,
                pin: registry.get_setting(TVDB_PIN_SETTING).await?,
            };
            // Same cache as the enrichment run: a reviewer trying five
            // titles in a row used to log in five times.
            if let Ok(token) = self.tvdb_token(&creds).await {
                match self.tvdb_search(&token, kind, query).await {
                    Ok(cands) => out.extend(cands.iter().map(|c| {
                        let mut v = serde_json::to_value(c).unwrap();
                        v["provider"] = serde_json::json!("tvdb");
                        v["format"] =
                            serde_json::json!(if kind == "movie" { "Movie" } else { "Series" });
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
        // Anime identity had no voice here: TMDB's "Kite" is the 2014
        // live-action film, and the 1998 OVA is AniList's to name. The
        // pin machinery is provider-generic, so offering 'anilist'
        // candidates is all HUB-8 needs to hand-match anime.
        match self.anilist.search(query).await {
            Ok(media) => out.extend(media.iter().map(|m| {
                serde_json::json!({
                    "id": m.id,
                    "provider": "anilist",
                    "title": m.display_title(),
                    "overview": m.plain_description(),
                    "poster_path": m.cover_image.as_ref()
                        .and_then(|c| c.extra_large.clone().or_else(|| c.large.clone())),
                    "poster_url": m.cover_image.as_ref()
                        .and_then(|c| c.large.clone().or_else(|| c.extra_large.clone())),
                    "release_date": m.premiered(),
                    "vote_average": m.average_score.map(|s| s / 10.0),
                    "format": m.format.as_deref().map(|f| match f {
                        "TV" => "TV",
                        "TV_SHORT" => "TV Short",
                        "MOVIE" => "Movie",
                        "OVA" => "OVA",
                        "ONA" => "ONA",
                        "SPECIAL" => "Special",
                        "MUSIC" => "Music",
                        other => other,
                    }),
                })
            })),
            Err(e) => tracing::warn!(error = format!("{e:#}"), "review anilist search failed"),
        }
        rank_candidates(&mut out, query, year, anime_first);
        Ok(serde_json::json!(out))
    }

    /// Fetch a TMDB poster (used by the artwork store when an item has
    /// no local artwork).
    /// `Ok(None)` when the provider says there is no such image.
    ///
    /// A provider holding no artwork is an ANSWER, not a failure: Cover
    /// Art Archive 404s for a release group nobody has uploaded a cover
    /// for, which is the ordinary case for obscure records. Carried as an
    /// `Err` it reached the client as a 500 whose body quoted the upstream
    /// URL — a server error for a record with no sleeve, and the provider's
    /// own address handed to whoever asked (SEC-WEB-7).
    ///
    /// Anything else — a timeout, a 5xx, a refused connection — stays an
    /// `Err`, because that one really is our problem and might not be true
    /// a minute later.
    pub async fn fetch_poster(&self, poster_path: &str) -> Result<Option<Vec<u8>>> {
        // TMDB stores relative paths; TVDB image URLs are absolute.
        let url = if poster_path.starts_with("http") {
            poster_path.to_string()
        } else {
            format!("https://image.tmdb.org/t/p/w500{poster_path}")
        };
        let resp = self.http.send(self.http.get(&url)).await?;
        if matches!(
            resp.status(),
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
        ) {
            tracing::debug!(%url, "provider holds no poster here");
            return Ok(None);
        }
        let resp = resp.error_for_status()?;
        Ok(Some(resp.bytes().await?.to_vec()))
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
        let cands = vec![
            cand(1, "Heat Wave", "1995-01-01"),
            cand(2, "Heat", "1995-12-15"),
        ];
        let (c, conf) = pick_candidate(&cands, "Heat", Some(1995)).unwrap();
        assert_eq!((c.id, conf), (2, "auto"));
        // Year mismatch beyond ±1 disqualifies the title match.
        assert!(pick_candidate(&cands[1..], "Heat", Some(2006)).is_none());
        // Local title + the candidate's subtitle, and the year agrees:
        // "Leon (1994)" IS "Léon: The Professional (1994)".
        let one = vec![cand(3, "Léon: The Professional", "1994-09-14")];
        let (c, conf) = pick_candidate(&one, "Leon", Some(1994)).unwrap();
        assert_eq!((c.id, conf), (3, "auto"));
        // Single plausible result that is not a subtitle form → weak.
        let vague = vec![cand(9, "The Professional", "1994-09-14")];
        let (c, conf) = pick_candidate(&vague, "Leon", Some(1994)).unwrap();
        assert_eq!((c.id, conf), (9, "weak"));
        // Multiple results, none matching → miss.
        let many = vec![cand(4, "A", "2000-01-01"), cand(5, "B", "2000-01-01")];
        assert!(pick_candidate(&many, "C", None).is_none());
        // Normalized equality: punctuation/case don't matter.
        let lp = vec![cand(6, "Léon: The Professional", "1994-09-14")];
        let (_, conf) = pick_candidate(&lp, "Leon The Professional", None).unwrap();
        assert_eq!(conf, "auto");
        // Number words fold: "12 Monkeys" == "Twelve Monkeys".
        let tm = vec![
            cand(7, "Twelve Monkeys", "1995-12-29"),
            cand(8, "12 Rounds", "2009-03-19"),
        ];
        let (c, conf) = pick_candidate(&tm, "12 Monkeys", None).unwrap();
        assert_eq!((c.id, conf), (7, "auto"));
        // Roman numerals fold (2+ chars only — "I" and "V" are words).
        let mib = vec![cand(13, "Men in Black II", "2002-07-03")];
        let (c, conf) = pick_candidate(&mib, "Men in Black 2", None).unwrap();
        assert_eq!((c.id, conf), (13, "auto"));
        let vfv = vec![
            cand(14, "V for Vendetta", "2006-03-15"),
            cand(15, "5 for Vendetta", "2000-01-01"),
        ];
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
        self.store_answer_for(db, item_id, provider, pick, "movies")
            .await
    }

    /// Record one provider's answer and re-merge the item (HUB-5).
    pub(crate) async fn store_answer_for(
        &self,
        db: &sqlx::SqlitePool,
        item_id: &str,
        provider: &str,
        pick: Option<&(Candidate, &'static str)>,
        _media_type: &str,
    ) -> Result<()> {
        let (provider_id, confidence, c) = match pick {
            Some((c, conf)) => (c.id.to_string(), *conf, Some(c)),
            None => (String::new(), "miss", None),
        };
        let fields = crate::providers::Fields {
            title: c.map(|c| c.title.clone()),
            overview: c.and_then(|c| c.overview.clone()),
            poster_path: c.and_then(|c| c.poster_path.clone()),
            // TMDB says 0 for unrated; that is absent, not a score of zero.
            rating: c.and_then(|c| c.vote_average).filter(|r| *r > 0.0),
            premiered: c.and_then(|c| c.release_date.clone()),
            original_language: c.and_then(|c| c.original_language.clone()),
            // Search results carry neither: both arrive with the details
            // request that also fills original_language (HUB-6).
            genres: None,
            cast_json: None,
        };
        crate::providers::store_answer(db, item_id, provider, &provider_id, confidence, fields)
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
            .get(format!(
                "https://api4.thetvdb.com/v4/{path}/{tvdb_id}/extended"
            ))
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
        let mut req = self
            .http
            .get(format!("https://api.themoviedb.org/3/{path}/{tmdb_id}"));
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
            sqlx::query_scalar("SELECT mapped_tmdb FROM anime_ids WHERE item_id = ?")
                .bind(&item.id)
                .fetch_optional(db)
                .await?
                .flatten();
        let candidate = match (owner, mapped) {
            // Bridged: AniDB decided what this is, TMDB describes it.
            ("anime", Some(id)) => {
                let q = id.to_string();
                if !crate::providers::question_pending(db, &item.id, "tmdb", "mapped_id", &q).await
                {
                    return Ok(crate::providers::Outcome::NotApplicable);
                }
                let c = match self.enricher.tmdb_details(&self.key, &item.kind, id).await {
                    Ok(c) => c,
                    Err(e) if is_http_404(&e) => {
                        crate::providers::record_question(db, &item.id, "tmdb", "mapped_id", &q)
                            .await;
                        return Ok(crate::providers::Outcome::Declined);
                    }
                    Err(e) => return Err(e),
                };
                crate::providers::record_question(db, &item.id, "tmdb", "mapped_id", &q).await;
                c
            }
            // No mapped id yet — anime-lists refreshes weekly, so this
            // is "cannot ask", not "asked and missed".
            ("anime", None) => return Ok(crate::providers::Outcome::NotApplicable),
            _ => {
                let anchor = crate::providers::title_anchor(&item.norm_title, item.year);
                if !crate::providers::question_pending(db, &item.id, "tmdb", "title", &anchor).await
                {
                    return Ok(crate::providers::Outcome::NotApplicable);
                }
                let cands = self
                    .enricher
                    .search(&self.key, &item.kind, &item.title, item.year)
                    .await?;
                crate::providers::record_question(db, &item.id, "tmdb", "title", &anchor).await;
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
        // The question log gates the network, not the walker: the
        // ladder's variants are subsumed by the primary anchor (a
        // ladder change is a derivation change — bump QUERY_REV), the
        // directory alt-key is its own question.
        let anchor = crate::providers::title_anchor(&item.norm_title, item.year);
        let alt_anchor = item.alt.as_ref().map(|a| {
            crate::providers::title_anchor(
                &kahawai_core::names::normalize_title(&a.title),
                a.year.map(|y| y as i64).or(item.year),
            )
        });
        let ask_primary =
            crate::providers::question_pending(db, &item.id, "tmdb", "title", &anchor).await;
        let ask_alt = match &alt_anchor {
            Some(a) => crate::providers::question_pending(db, &item.id, "tmdb", "title", a).await,
            None => false,
        };
        if !ask_primary && !ask_alt {
            return Ok(crate::providers::Outcome::Declined);
        }
        // Query ladder: TMDB's search has holes (a literal "And" finds
        // nothing where "&" or a shortened query hits); the strict
        // verifier still judges candidates against the FULL local title.
        let title = &item.title;
        let mut picked: Option<(Candidate, &'static str)> = None;
        if ask_primary {
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
            for (vi, q) in variants.iter().enumerate() {
                let cands = self
                    .enricher
                    .search(&self.key, &item.kind, q, item.year)
                    .await?;
                if let Some((c, conf)) = pick_candidate(&cands, title, item.year) {
                    picked = Some((c.clone(), conf));
                    if vi > 0 {
                        tracing::debug!(title, variant = %q, "matched via query variant");
                    }
                    break;
                }
            }
            crate::providers::record_question(db, &item.id, "tmdb", "title", &anchor).await;
        }
        if picked.is_none()
            && ask_alt
            && let Some(alt) = &item.alt
        {
            let alt_year = alt.year.map(|y| y as i64).or(item.year);
            let cands = self
                .enricher
                .search(&self.key, &item.kind, &alt.title, alt_year)
                .await?;
            if let Some(a) = &alt_anchor {
                crate::providers::record_question(db, &item.id, "tmdb", "title", a).await;
            }
            if let Some((c, conf)) = pick_candidate(&cands, &alt.title, alt_year) {
                picked = Some((c.clone(), conf));
                tracing::debug!(title, alt = %alt.title, "matched via directory name");
            }
        }
        match picked {
            Some(pick) => {
                let conf = pick.1;
                self.enricher
                    .store_generic(db, &item.id, "tmdb", Some(&pick))
                    .await?;
                Ok(crate::providers::Outcome::Matched(conf))
            }
            None => Ok(crate::providers::Outcome::Declined),
        }
    }
}

struct TvdbProvider {
    enricher: Arc<Enricher>,
    /// Credentials, not a token: see `Enricher::tvdb_token`. Present
    /// whenever a key is configured, exactly like TMDB.
    creds: TvdbCreds,
}

impl TvdbProvider {
    /// The token for one request, dropping the cache when the request
    /// that used it failed — a failure the chain then reschedules, so
    /// the retry logs in fresh.
    async fn token(&self) -> Result<std::sync::Arc<String>> {
        self.enricher.tvdb_token(&self.creds).await
    }
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
        // so it declines rather than searching by title. The owner
        // arrives as a CHAIN name ("anime", identity_owner maps it) —
        // comparing against the raw column name left this guard dead
        // and TVDB title-searching anime; 11 movie-namespace answers
        // diverged from the mapping before 2026-07-28 caught it.
        if item.owner.as_deref() == Some("anime") {
            let mapped: Option<i64> =
                sqlx::query_scalar("SELECT mapped_tvdb FROM anime_ids WHERE item_id = ?")
                    .bind(&item.id)
                    .fetch_optional(db)
                    .await?
                    .flatten();
            let Some(tvdb_id) = mapped else {
                return Ok(crate::providers::Outcome::NotApplicable);
            };
            let q = tvdb_id.to_string();
            if !crate::providers::question_pending(db, &item.id, "tvdb", "mapped_id", &q).await {
                return Ok(crate::providers::Outcome::NotApplicable);
            }
            let token = self.token().await?;
            let c = match self
                .enricher
                .tvdb_details(&token, &item.kind, tvdb_id)
                .await
            {
                Ok(c) => c,
                Err(e) if is_http_404(&e) => {
                    crate::providers::record_question(db, &item.id, "tvdb", "mapped_id", &q).await;
                    return Ok(crate::providers::Outcome::Declined);
                }
                // Not a 404, so the token is a suspect: drop it and let
                // the chain reschedule. The retry logs in fresh.
                Err(e) => {
                    self.enricher.tvdb_forget().await;
                    return Err(e);
                }
            };
            crate::providers::record_question(db, &item.id, "tvdb", "mapped_id", &q).await;
            let pick = (c, "auto");
            self.enricher
                .store_answer_for(db, &item.id, "tvdb", Some(&pick), "anime")
                .await?;
            return Ok(crate::providers::Outcome::Contributed);
        }
        let anchor = crate::providers::title_anchor(&item.norm_title, item.year);
        if !crate::providers::question_pending(db, &item.id, "tvdb", "title", &anchor).await {
            return Ok(crate::providers::Outcome::Declined);
        }
        let token = self.token().await?;
        let cands = match self
            .enricher
            .tvdb_search(&token, &item.kind, &item.title)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                self.enricher.tvdb_forget().await;
                return Err(e);
            }
        };
        crate::providers::record_question(db, &item.id, "tvdb", "title", &anchor).await;
        match pick_candidate(&cands, &item.title, item.year) {
            Some((c, conf)) => {
                let pick = (c.clone(), conf);
                self.enricher
                    .store_generic(db, &item.id, "tvdb", Some(&pick))
                    .await?;
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
                        tracing::warn!(
                            error = format!("{e:#}"),
                            "anidb lookup failed; disabling for this run"
                        );
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
        // The name question is gated by its anchor; the hash side keeps
        // its own ledger (ed2k_aid). An exact hash hit always proceeds —
        // it is the identity, whatever was asked before.
        let anchor = crate::providers::title_anchor(&item.norm_title, item.year);
        if exact_aid.is_none()
            && !crate::providers::question_pending(db, &item.id, "anime", "title", &anchor).await
        {
            return Ok(crate::providers::Outcome::Declined);
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
        crate::providers::record_question(db, &item.id, "anime", "title", &anchor).await;
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
        let anchor = crate::providers::music_anchor(item.norm_artist.as_deref(), &item.norm_title);
        if !crate::providers::question_pending(db, &item.id, "musicbrainz", "title", &anchor).await
        {
            return Ok(crate::providers::Outcome::Declined);
        }
        // Pacing lives in the gate now (one request per second, keyed on
        // musicbrainz.org) — a sleep here would only double it.
        let rg = self.enricher.musicbrainz_album(&item.title, artist).await?;
        crate::providers::record_question(db, &item.id, "musicbrainz", "title", &anchor).await;
        let Some(rg) = rg else {
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
        )
        .await?;
        Ok(crate::providers::Outcome::Matched("auto"))
    }
}

// ---------- HUB-9: local metadata as a provider ----------

/// What a Kodi-style .nfo says. Everything is optional: these files are
/// hand-made and half of them are a single `<title>`.
fn parse_nfo(xml: &str) -> Option<(crate::providers::Fields, Option<String>)> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let root = doc.root_element();
    // <movie>, <tvshow>, <episodedetails> — the tag names differ, the
    // children do not.
    let text = |name: &str| {
        root.children()
            .find(|c| c.has_tag_name(name))
            .and_then(|c| c.text())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let genres: Vec<String> = root
        .children()
        .filter(|c| c.has_tag_name("genre"))
        .filter_map(|c| c.text())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    // A year on its own is enough to date an item; premiered wins.
    let premiered = text("premiered").or_else(|| text("year").map(|y| format!("{y}-01-01")));
    // The id a human curated, if any: <uniqueid> first, then the older
    // dedicated tags. It becomes this answer's provider_id, so a local
    // record is as identifiable as any other.
    let unique = root
        .children()
        .find(|c| c.has_tag_name("uniqueid") && c.attribute("default") == Some("true"))
        .or_else(|| root.children().find(|c| c.has_tag_name("uniqueid")))
        .and_then(|c| c.text())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| text("tmdbid"))
        .or_else(|| text("imdbid"));
    let fields = crate::providers::Fields {
        title: text("title").or_else(|| text("originaltitle")),
        overview: text("plot").or_else(|| text("outline")),
        rating: text("rating")
            .and_then(|r| r.parse::<f64>().ok())
            .filter(|r| *r > 0.0),
        premiered,
        genres: (!genres.is_empty()).then(|| serde_json::to_string(&genres).unwrap_or_default()),
        ..Default::default()
    };
    // A file with nothing usable in it is not an answer.
    if fields.title.is_none() && fields.overview.is_none() && fields.premiered.is_none() {
        return None;
    }
    Some((fields, unique))
}

/// HUB-9: the .nfo beside the media is a provider like any other, and by
/// default the first one — a human wrote it, so it outranks a search
/// result. It never reaches the network: the file is read through the
/// same byte-plane lease artwork uses.
pub(crate) struct LocalProvider {
    sessions: Arc<crate::sessions::Sessions>,
    registry: Arc<Registry>,
}

#[async_trait::async_trait]
impl crate::providers::Provider for LocalProvider {
    fn name(&self) -> &'static str {
        "local"
    }

    async fn enrich(
        &self,
        db: &sqlx::SqlitePool,
        item: &crate::providers::ItemRef,
    ) -> Result<crate::providers::Outcome> {
        // Both local sources come out of the scan record, so an item with
        // neither costs one DB read and no lease.
        let art = crate::artwork::find_artwork_source(&self.registry, &item.id).await?;
        let nfo = nfo_source(&self.registry, &item.id).await?;
        if art.is_none() && nfo.is_none() {
            // Nothing beside the media any more. If local previously
            // answered, that answer describes a file that has been
            // deleted — retract it rather than leave the item claiming an
            // identity from a .nfo nobody can read. Deleting the answer is
            // the whole obligation: the item is handed back to whichever
            // provider actually has it by the trigger on this statement.
            let stale = sqlx::query(
                "DELETE FROM provider_metadata WHERE item_id = ? AND provider = 'local'",
            )
            .bind(&item.id)
            .execute(db)
            .await?
            .rows_affected();
            if stale > 0 {
                tracing::info!(item = %item.id, "local metadata withdrawn; its sidecars are gone");
            }
            return Ok(crate::providers::Outcome::NotApplicable);
        }
        // The cover next to the media is local metadata too (HUB-9), and
        // routing it through the chain is what makes "local first" a rank
        // the user can change rather than an if-statement in artwork.rs.
        let mut fields = crate::providers::Fields {
            poster_path: art
                .as_ref()
                .map(|(_, _, _, p)| format!("{}{p}", crate::artwork::LOCAL)),
            ..Default::default()
        };
        // Empty id on purpose: a cover is a field, not an identity. It
        // side-fills at whatever rank `local` holds, but a picture next to
        // the file says nothing about WHICH work this is, so it must never
        // become the item's match. A .nfo does state that, and sets one.
        let mut provider_id = String::new();
        // A .nfo is a lease read, so only when the scan saw one.
        if let Some((module_id, collection_id, root_token, nfo_rel)) = nfo {
            let lease = self
                .sessions
                .open_lease(
                    &self.registry,
                    &module_id,
                    &collection_id,
                    &root_token,
                    &nfo_rel,
                )
                .await?;
            let bytes = read_nfo(lease).await?;
            if let Some((parsed, unique)) = parse_nfo(&String::from_utf8_lossy(&bytes)) {
                // The path identifies the record when the file states no
                // id: two items never share one .nfo.
                provider_id = unique.unwrap_or(nfo_rel.clone());
                fields = crate::providers::Fields {
                    poster_path: fields.poster_path,
                    ..parsed
                };
                tracing::debug!(item = %item.id, nfo = %nfo_rel, "local metadata adopted");
            }
        }
        crate::providers::store_answer(db, &item.id, "local", &provider_id, "auto", fields).await?;
        Ok(if provider_id.is_empty() {
            crate::providers::Outcome::Contributed
        } else {
            crate::providers::Outcome::Matched("auto")
        })
    }
}

/// The .nfo recorded on any of this item's (or its children's) sources.
async fn nfo_source(
    registry: &Registry,
    item_id: &str,
) -> Result<Option<(String, String, String, String)>> {
    let row = sqlx::query(
        "SELECT f.module_id,f.collection_id,r.root_token,
                json_extract(f.streams_json,'$.nfo') AS nfo
         FROM files f JOIN collection_roots r ON r.id=f.root_id
         WHERE EXISTS(SELECT 1 FROM file_bindings fb WHERE fb.file_id=f.id
                AND (fb.item_id=?1 OR fb.item_id IN (SELECT id FROM items WHERE parent_id=?1)))
           AND nfo IS NOT NULL
         LIMIT 1",
    )
    .bind(item_id)
    .fetch_optional(registry.db())
    .await
    .context("nfo lookup")?;
    Ok(row.map(|r| {
        (
            r.get("module_id"),
            r.get("collection_id"),
            r.get("root_token"),
            r.get("nfo"),
        )
    }))
}

/// Drain a (small) .nfo through a lease. Capped: a file claiming to be
/// metadata and weighing megabytes is not one.
async fn read_nfo(lease: crate::leases::Lease) -> Result<Vec<u8>> {
    const MAX: u64 = 1 << 20;
    let mut out = Vec::new();
    let mut stream = lease.read_range(0, MAX).into_inner();
    while let Some(chunk) = stream.recv().await {
        out.extend_from_slice(&chunk.map_err(|e| anyhow::anyhow!("lease read: {e}"))?);
    }
    Ok(out)
}

#[cfg(test)]
mod nfo_tests {
    use super::parse_nfo;

    /// A Kodi .nfo, and the half-filled ones people actually have.
    #[test]
    fn reads_what_a_human_wrote() {
        let (f, id) = parse_nfo(
            r#"<?xml version="1.0"?>
            <movie>
              <title>Solaris</title>
              <plot>A psychologist is sent to a station orbiting Solaris.</plot>
              <year>1972</year>
              <rating>8.1</rating>
              <genre>Science Fiction</genre>
              <genre>Drama</genre>
              <uniqueid type="tmdb" default="true">593</uniqueid>
            </movie>"#,
        )
        .expect("a full nfo is an answer");
        assert_eq!(f.title.as_deref(), Some("Solaris"));
        assert_eq!(f.rating, Some(8.1));
        // A bare <year> still dates the item.
        assert_eq!(f.premiered.as_deref(), Some("1972-01-01"));
        assert_eq!(f.genres.as_deref(), Some(r#"["Science Fiction","Drama"]"#));
        assert_eq!(
            id.as_deref(),
            Some("593"),
            "the curated id identifies the record"
        );

        // <premiered> beats a <year>, and a file with only a title counts.
        let (f, id) = parse_nfo(
            "<tvshow><title>Andor</title><year>2021</year><premiered>2022-09-21</premiered></tvshow>",
        )
        .unwrap();
        assert_eq!(f.premiered.as_deref(), Some("2022-09-21"));
        assert_eq!(
            id, None,
            "no id in the file: the caller falls back to the path"
        );

        // Nothing usable is not an answer — better no row than an empty one.
        assert!(parse_nfo("<movie><thumb>poster.jpg</thumb></movie>").is_none());
        assert!(parse_nfo("not xml at all").is_none());
        // A zero rating means unrated here too.
        let (f, _) = parse_nfo("<movie><title>x</title><rating>0.0</rating></movie>").unwrap();
        assert_eq!(f.rating, None);
    }
}

/// Rank hand-matching candidates by relevance to what the admin TYPED,
/// not by which provider answered first: an exact folded-title match
/// outranks a prefix match outranks a substring, a matching year (when
/// one was given) breaks ties, then rating. Searching "Kite" must put
/// the things actually CALLED Kite above "One Day A Letter Arrives
/// from the Dog Kingdom".
pub fn rank_candidates(
    out: &mut [serde_json::Value],
    query: &str,
    year: Option<i64>,
    anime_first: bool,
) {
    let q = fold(query);
    let key = |v: &serde_json::Value| {
        let t = v["title"].as_str().map(fold).unwrap_or_default();
        let title_score = if t == q {
            0u8
        } else if t.starts_with(&q) {
            1
        } else if t.contains(&q) {
            2
        } else {
            3
        };
        let year_miss = match (year, v["release_date"].as_str().and_then(|d| d.get(..4))) {
            (Some(want), Some(got)) => (got.parse::<i64>().ok() != Some(want)) as u8,
            _ => 0,
        };
        // At equal relevance, the provider that owns the item's
        // identity space leads: anilist for anime items, the generic
        // pair otherwise.
        let is_anilist = v["provider"].as_str() == Some("anilist");
        let provider_rank = (is_anilist != anime_first) as u8;
        // Rating descending, NaN-safe, as an integer key.
        let rating = -(v["vote_average"].as_f64().unwrap_or(0.0) * 10.0) as i64;
        (title_score, year_miss, provider_rank, rating)
    };
    out.sort_by_key(key);
}

#[cfg(test)]
mod candidate_rank_tests {
    use super::rank_candidates;
    use serde_json::json;

    #[test]
    fn typed_title_beats_provider_order() {
        let mut cands = vec![
            json!({"title": "One Day A Letter Arrives", "release_date": "2015-01-01", "vote_average": 9.0}),
            json!({"title": "Kite Liberator", "release_date": "2008-03-21", "vote_average": 6.0}),
            json!({"title": "Kite", "release_date": "2014-10-09", "vote_average": 5.0}),
            json!({"title": "Kite", "release_date": "1998-02-25", "vote_average": 7.0}),
        ];
        rank_candidates(&mut cands, "Kite", Some(1998), false);
        let titles: Vec<(&str, &str)> = cands
            .iter()
            .map(|c| {
                (
                    c["title"].as_str().unwrap(),
                    &c["release_date"].as_str().unwrap()[..4],
                )
            })
            .collect();
        assert_eq!(
            titles,
            [
                ("Kite", "1998"),
                ("Kite", "2014"),
                ("Kite Liberator", "2008"),
                ("One Day A Letter Arrives", "2015")
            ],
            "exact + year first, prefix next, unrelated last whatever its rating"
        );
    }

    #[test]
    fn collection_identity_space_leads_at_equal_relevance() {
        let mk = || {
            vec![
                json!({"title": "Kite", "provider": "tmdb", "release_date": "2014-10-09", "vote_average": 9.0}),
                json!({"title": "Kite", "provider": "anilist", "release_date": "1998-02-25", "vote_average": 7.0}),
            ]
        };
        let mut anime = mk();
        rank_candidates(&mut anime, "Kite", None, true);
        assert_eq!(
            anime[0]["provider"], "anilist",
            "anime item: anilist leads its rating notwithstanding"
        );
        let mut generic = mk();
        rank_candidates(&mut generic, "Kite", None, false);
        assert_eq!(generic[0]["provider"], "tmdb", "generic item: tmdb leads");
    }
}
