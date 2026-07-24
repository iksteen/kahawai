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

pub struct Enricher {
    http: reqwest::Client,
    running: AtomicBool,
    /// (matched, weak, missed) of the current/last run.
    progress: (AtomicUsize, AtomicUsize, AtomicUsize),
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    results: Vec<Candidate>,
}

#[derive(Debug, Clone, Deserialize)]
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
    let title_eq = |c: &Candidate| {
        fold(&c.title) == norm
            || c.original_title.as_deref().is_some_and(|t| fold(t) == norm)
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
fn fold(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    const WORDS: &[(&str, &str)] = &[
        ("zero", "0"), ("one", "1"), ("two", "2"), ("three", "3"), ("four", "4"),
        ("five", "5"), ("six", "6"), ("seven", "7"), ("eight", "8"), ("nine", "9"),
        ("ten", "10"), ("eleven", "11"), ("twelve", "12"), ("thirteen", "13"),
        ("fourteen", "14"), ("fifteen", "15"), ("sixteen", "16"), ("seventeen", "17"),
        ("eighteen", "18"), ("nineteen", "19"), ("twenty", "20"),
    ];
    let base: String = kahawai_core::names::normalize_title(s)
        .nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect();
    base.split_whitespace()
        .map(|w| WORDS.iter().find(|(word, _)| *word == w).map(|(_, d)| *d).unwrap_or(w))
        .collect::<Vec<_>>()
        .join(" ")
}

impl Enricher {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent("kahawai")
                .build()
                .expect("http client"),
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
        for c in [&self.progress.0, &self.progress.1, &self.progress.2] {
            c.store(0, Ordering::SeqCst);
        }
        let items = sqlx::query(
            "SELECT i.id, i.kind, i.title, i.year FROM items i
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
            let this = self.clone();
            let key = key.clone();
            let db = registry.db().clone();
            let sem = sem.clone();
            tasks.spawn(async move {
                let _permit = sem.acquire().await;
                let cands = match this.search(&key, &kind, &title, year).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(title, error = format!("{e:#}"), "tmdb search failed");
                        return;
                    }
                };
                let picked = pick_candidate(&cands, &title, year);
                let (provider_id, confidence, c) = match &picked {
                    Some((c, conf)) => (c.id.to_string(), *conf, Some(*c)),
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
                     VALUES (?, 'tmdb', ?, ?, ?, ?, ?, ?, NULL, ?, unixepoch())
                     ON CONFLICT (item_id) DO UPDATE SET
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
        Ok((m, w, x))
    }

    /// Fetch a TMDB poster (used by the artwork store when an item has
    /// no local artwork).
    pub async fn fetch_poster(&self, poster_path: &str) -> Result<Vec<u8>> {
        let url = format!("https://image.tmdb.org/t/p/w500{poster_path}");
        let resp = self.http.get(&url).send().await?.error_for_status()?;
        Ok(resp.bytes().await?.to_vec())
    }
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
