//! HUB-21/22/24: external subtitle provider. OpenSubtitles.com REST
//! client behind a small trait so a second provider is one impl. The
//! app key ships with the binary (DEFAULT_API_KEY, overridable only by
//! the config file) and grants anonymous use: 5 requests/s and 5
//! downloads per 24 h shared by the whole deployment. A user who
//! attaches their own account in Settings spends their own entitlement
//! instead; what they download is shared with everyone (HUB-23).
//!
//! Matching (HUB-22): the file's OpenSubtitles moviehash — which is
//! exactly the `oshash` the mediahost already computes (size + u64-LE
//! sums of the first/last 64 KiB) — is the primary search key, with a
//! title/year fallback for files the provider doesn't recognize by hash.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const API: &str = "https://api.opensubtitles.com/api/v1";
/// They ask for "AppName vX.Y" specifically, so this stays alongside
/// the gate's generic UA.
const USER_AGENT: &str = "kahawai v1";

/// kahawai's registered consumer key. Baked in on purpose: the key
/// identifies the APPLICATION, not the user — OpenSubtitles explicitly
/// asks integrators to ship it ("API key should be provided within your
/// Program... we just need to have user credentials", jellyfin-plugin-
/// opensubtitles#70), and Jellyfin ships theirs as a public const the
/// same way. The KEY_SETTING override exists for forks and for the day
/// this key gets rate-limited.
const DEFAULT_API_KEY: &str = "etagFAIulrKkxGBC8iRePjhBRTwhjMid";

pub fn default_api_key() -> &'static str {
    DEFAULT_API_KEY
}

/// Per-USER preference keys (user_prefs, global scope): each user
/// attaches their own OpenSubtitles account, spending their own
/// download entitlement. What they download is shared with everyone —
/// subtitles belong to the item, not the downloader (HUB-23).
pub const USER_PREF_USERNAME: &str = "opensubtitles.username";
pub const USER_PREF_PASSWORD: &str = "opensubtitles.password";

/// The download entitlement is spent — this account's, or the server's shared
/// anonymous one.
///
/// A type rather than a sentence so the API can give it a code of its own. It
/// is not an upstream fault and it is not a network problem: telling somebody
/// the provider did not answer when they have simply used their five downloads
/// sends them to retry instead of to add an account. The anonymous budget is
/// five per 24 h, so this is an ordinary Tuesday, not an incident.
///
/// The message travels with it because it is authored for a reader and names
/// the way out. That is what makes it safe to put on the wire, and why the
/// API reads THIS type's `Display` rather than the anyhow chain around it.
#[derive(Debug)]
pub struct QuotaSpent(pub String);

impl std::fmt::Display for QuotaSpent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for QuotaSpent {}

/// Deployment-level provider config (kahawai.toml): just the
/// application key, and only when a deployment wants its own. Empty =
/// the admin setting, then the built-in key. Account credentials live
/// in the admin page, not here.
#[derive(Debug, Clone, Default)]
pub struct ProviderConfig {
    pub api_key: String,
}

/// One search result the user can choose to download.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct Candidate {
    pub provider: &'static str,
    /// Opaque download handle (OpenSubtitles file_id, stringified).
    pub file_id: String,
    #[schema(required)]
    pub language: Option<String>,
    #[schema(required)]
    pub release_name: Option<String>,
    /// True when the provider matched by moviehash (exact file, HUB-22)
    /// rather than by title — surfaced so the UI can rank/badge it.
    pub hash_match: bool,
    pub downloads: i64,
    /// HUB-24 display fields: who uploaded it and how it rates.
    #[schema(required)]
    pub uploader: Option<String>,
    #[schema(required)]
    pub rating: Option<f64>,
    /// Frames per second the subtitle was timed against, when the
    /// provider knows it (0/None = unknown). A mismatch with the file
    /// is the classic cause of progressive drift.
    #[schema(required)]
    pub fps: Option<f64>,
}

/// HUB-21/24: what is left of the download entitlement. Anonymous
/// usage shares one budget across the whole deployment, which the UI
/// must say out loud.
#[derive(Debug, Clone, Default, serde::Serialize, utoipa::ToSchema)]
pub struct Quota {
    #[schema(required)]
    pub remaining: Option<i64>,
    #[schema(required)]
    pub total: Option<i64>,
    /// Seconds until the entitlement resets, when the provider says.
    #[schema(required)]
    pub resets_in_secs: Option<i64>,
    /// False = anonymous, i.e. shared by everyone using this hub.
    pub per_account: bool,
}

/// Downloaded subtitle bytes + normalized format.
pub struct Downloaded {
    pub bytes: Vec<u8>,
    pub format: String, // "srt" | "ass"
    pub release_name: Option<String>,
}

/// What a subtitle provider does (HUB-21). Kept minimal: search, then
/// download a chosen candidate.
#[async_trait::async_trait]
pub trait SubtitleProvider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn search(&self, q: &SearchQuery) -> Result<Vec<Candidate>>;
    async fn download(&self, file_id: &str) -> Result<Downloaded>;
    /// Last known entitlement state (updated by download responses).
    fn quota(&self) -> Quota;
    /// Ask the provider for the current entitlement, if it can say.
    async fn refresh_quota(&self) {}
}

pub struct SearchQuery {
    pub moviehash: Option<u64>,
    /// External ids from enrichment (HUB-22): tried before title search.
    pub tmdb_id: Option<i64>,
    pub imdb_id: Option<String>,
    pub title: Option<String>,
    pub year: Option<i64>,
    /// Season/episode for series (OpenSubtitles wants them explicitly).
    pub season: Option<i64>,
    pub episode: Option<i64>,
    pub languages: Vec<String>,
}

pub struct OpenSubtitles {
    /// Paced by the shared gate: their standard tier is 1 request per
    /// second, and /login is capped harder still.
    http: std::sync::Arc<crate::gate::Http>,
    api_key: String,
    username: Option<String>,
    password: Option<String>,
    /// Cached download token (OpenSubtitles issues it via /login and it
    /// lasts ~24 h; download quota is tracked against it).
    token: tokio::sync::Mutex<Option<String>>,
    quota: std::sync::Mutex<Quota>,
    quota_checked: tokio::sync::Mutex<Option<tokio::time::Instant>>,
}

impl OpenSubtitles {
    pub fn new(
        http: std::sync::Arc<crate::gate::Http>,
        api_key: String,
        username: Option<String>,
        password: Option<String>,
    ) -> Self {
        let per_account = username.is_some() && password.is_some();
        Self {
            http,
            api_key,
            username,
            password,
            token: tokio::sync::Mutex::new(None),
            quota: std::sync::Mutex::new(Quota {
                per_account,
                ..Default::default()
            }),
            quota_checked: tokio::sync::Mutex::new(None),
        }
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{API}{path}"))
            .header("Api-Key", &self.api_key)
            .header("User-Agent", USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/json")
    }

    /// Downloads work anonymously against the app key (5 per 24 h);
    /// an account raises that quota. Returns None when no account is
    /// configured — the caller then downloads anonymously.
    async fn token(&self) -> Result<Option<String>> {
        {
            let cached = self.token.lock().await;
            if let Some(t) = cached.as_ref() {
                return Ok(Some(t.clone()));
            }
        }
        let (Some(user), Some(pass)) = (&self.username, &self.password) else {
            return Ok(None);
        };
        #[derive(Deserialize)]
        struct LoginResp {
            token: String,
        }
        let req = self
            .req(reqwest::Method::POST, "/login")
            .json(&serde_json::json!({ "username": user, "password": pass }));
        let resp = self.http.send(req).await?;
        if !resp.status().is_success() {
            bail!(
                "OpenSubtitles login failed: {} {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
        }
        let token = resp.json::<LoginResp>().await?.token;
        *self.token.lock().await = Some(token.clone());
        Ok(Some(token))
    }
}

/// OpenSubtitles reports the reset as "23 hours and 12 minutes" (and
/// occasionally an ISO timestamp). Parse the human form we actually
/// see; anything else just leaves the countdown unknown.
fn parse_reset_secs(s: &str) -> Option<i64> {
    let lower = s.to_ascii_lowercase();
    let mut secs = 0i64;
    let mut found = false;
    let words: Vec<&str> = lower.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        let Ok(n) = w.parse::<i64>() else { continue };
        match words.get(i + 1).copied().unwrap_or("") {
            u if u.starts_with("hour") => {
                secs += n * 3600;
                found = true;
            }
            u if u.starts_with("minute") => {
                secs += n * 60;
                found = true;
            }
            u if u.starts_with("second") => {
                secs += n;
                found = true;
            }
            _ => {}
        }
    }
    found.then_some(secs)
}

/// Rank results: exact-file (hash) matches first, then the caller's
/// language order — the request's list is a preference, not merely a
/// filter — then popularity. Languages not in the list sort last (they
/// only appear at all in an unfiltered search).
fn rank_candidates(out: &mut [Candidate], languages: &[String]) {
    let lang_rank = |l: &Option<String>| -> usize {
        let Some(l) = l.as_deref() else {
            return usize::MAX;
        };
        let l = l.to_ascii_lowercase();
        languages
            .iter()
            .position(|want| {
                let want = want.to_ascii_lowercase();
                l == want || l.split(['-', '_']).next() == want.split(['-', '_']).next()
            })
            .unwrap_or(usize::MAX)
    };
    out.sort_by(|x, y| {
        y.hash_match
            .cmp(&x.hash_match)
            .then(lang_rank(&x.language).cmp(&lang_rank(&y.language)))
            .then(y.downloads.cmp(&x.downloads))
    });
}

#[async_trait::async_trait]
impl SubtitleProvider for OpenSubtitles {
    fn name(&self) -> &'static str {
        "opensubtitles"
    }

    async fn search(&self, q: &SearchQuery) -> Result<Vec<Candidate>> {
        let mut params: Vec<(String, String)> = Vec::new();
        if let Some(h) = q.moviehash {
            params.push(("moviehash".into(), format!("{h:016x}")));
        }
        if let Some(id) = q.tmdb_id {
            params.push(("tmdb_id".into(), id.to_string()));
        }
        if let Some(id) = &q.imdb_id {
            params.push(("imdb_id".into(), id.clone()));
        }
        if let Some(t) = &q.title {
            params.push(("query".into(), t.clone()));
        }
        if let Some(y) = q.year {
            params.push(("year".into(), y.to_string()));
        }
        if let Some(s) = q.season {
            params.push(("season_number".into(), s.to_string()));
        }
        if let Some(e) = q.episode {
            params.push(("episode_number".into(), e.to_string()));
        }
        if !q.languages.is_empty() {
            params.push(("languages".into(), q.languages.join(",")));
        }
        anyhow::ensure!(
            q.moviehash.is_some()
                || q.tmdb_id.is_some()
                || q.imdb_id.is_some()
                || q.title.is_some(),
            "subtitle search needs a hash, an external id, or a title"
        );

        #[derive(Deserialize)]
        struct Resp {
            data: Vec<Item>,
        }
        #[derive(Deserialize)]
        struct Item {
            attributes: Attrs,
        }
        #[derive(Deserialize)]
        struct Attrs {
            #[serde(default)]
            language: Option<String>,
            #[serde(default)]
            release: Option<String>,
            #[serde(default)]
            moviehash_match: bool,
            #[serde(default)]
            download_count: i64,
            #[serde(default)]
            ratings: Option<f64>,
            #[serde(default)]
            fps: Option<f64>,
            #[serde(default)]
            uploader: Option<Uploader>,
            files: Vec<FileRef>,
        }
        #[derive(Deserialize)]
        struct Uploader {
            #[serde(default)]
            name: Option<String>,
        }
        #[derive(Deserialize)]
        struct FileRef {
            file_id: i64,
        }

        let req = self.req(reqwest::Method::GET, "/subtitles").query(&params);
        let resp = self
            .http
            .send(req)
            .await?
            .error_for_status()
            .context("OpenSubtitles search")?;
        let parsed: Resp = resp.json().await?;
        let mut out = Vec::new();
        for item in parsed.data {
            let a = item.attributes;
            let Some(f) = a.files.into_iter().next() else {
                continue;
            };
            out.push(Candidate {
                provider: "opensubtitles",
                file_id: f.file_id.to_string(),
                language: a.language,
                release_name: a.release,
                hash_match: a.moviehash_match,
                downloads: a.download_count,
                uploader: a.uploader.and_then(|u| u.name),
                rating: a.ratings.filter(|r| *r > 0.0),
                fps: a.fps.filter(|f| *f > 0.0),
            });
        }
        rank_candidates(&mut out, &q.languages);
        Ok(out)
    }

    fn quota(&self) -> Quota {
        self.quota.lock().unwrap().clone()
    }

    /// HUB-24 wants the entitlement shown BEFORE a download too. With
    /// an account the provider will tell us (/infos/user); anonymously
    /// there is no such endpoint, so it stays unknown until the first
    /// download reports it. Refreshed at most once a minute.
    async fn refresh_quota(&self) {
        {
            let q = self.quota.lock().unwrap();
            if !q.per_account {
                return;
            }
        }
        if let Some(t) = *self.quota_checked.lock().await
            && t.elapsed() < Duration::from_secs(60)
        {
            return;
        }
        *self.quota_checked.lock().await = Some(tokio::time::Instant::now());
        let Ok(Some(token)) = self.token().await else {
            return;
        };
        #[derive(Deserialize)]
        struct Resp {
            data: UserInfo,
        }
        #[derive(Deserialize)]
        struct UserInfo {
            #[serde(default)]
            allowed_downloads: Option<i64>,
            #[serde(default)]
            remaining_downloads: Option<i64>,
        }
        let req = self
            .req(reqwest::Method::GET, "/infos/user")
            .bearer_auth(&token);
        let got = self
            .http
            .send(req)
            .await
            .ok()
            .filter(|r| r.status().is_success());
        let Some(resp) = got else { return };
        if let Ok(parsed) = resp.json::<Resp>().await {
            let mut q = self.quota.lock().unwrap();
            q.remaining = parsed.data.remaining_downloads;
            q.total = parsed.data.allowed_downloads;
        }
    }

    async fn download(&self, file_id: &str) -> Result<Downloaded> {
        let file_id: i64 = file_id.parse().context("bad file_id")?;
        let token = self.token().await?;
        #[derive(Deserialize)]
        struct DlResp {
            link: String,
            #[serde(default)]
            file_name: Option<String>,
            // The entitlement counters ride the download response.
            #[serde(default)]
            requests: Option<i64>,
            #[serde(default)]
            remaining: Option<i64>,
            #[serde(default)]
            reset_time_utc: Option<String>,
        }
        let mut req = self
            .req(reqwest::Method::POST, "/download")
            .json(&serde_json::json!({ "file_id": file_id }));
        if let Some(t) = &token {
            req = req.bearer_auth(t);
        }
        // A 429 never reaches here — that's rate limiting, and the gate
        // owns it. These codes are the entitlement being spent up.
        let resp = self.http.send(req).await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            *self.token.lock().await = None; // force re-login next time
            bail!("OpenSubtitles rejected the download token — retry");
        }
        if matches!(resp.status().as_u16(), 402 | 406 | 407) {
            bail!(QuotaSpent(format!(
                "OpenSubtitles download quota exhausted{} — it resets 24 h after your first \
                 download today",
                if token.is_some() {
                    ""
                } else {
                    " (anonymous: 5 per 24 h; add an account for more)"
                }
            )));
        }
        let dl: DlResp = resp
            .error_for_status()
            .context("OpenSubtitles download")?
            .json()
            .await?;
        {
            // HUB-24: remember what's left so every response can say so.
            let mut q = self.quota.lock().unwrap();
            q.remaining = dl.remaining;
            q.total = dl.remaining.and_then(|r| dl.requests.map(|used| r + used));
            q.resets_in_secs = dl.reset_time_utc.as_deref().and_then(parse_reset_secs);
        }
        let req = self.http.get(&dl.link).header("User-Agent", USER_AGENT);
        let bytes = self
            .http
            .send(req)
            .await?
            .error_for_status()?
            .bytes()
            .await?
            .to_vec();
        // OpenSubtitles serves .srt overwhelmingly; sniff ASS.
        let format = if dl.file_name.as_deref().is_some_and(|n| {
            n.to_ascii_lowercase().ends_with(".ass") || n.to_ascii_lowercase().ends_with(".ssa")
        }) || bytes.starts_with(b"[Script Info]")
        {
            "ass"
        } else {
            "srt"
        };
        Ok(Downloaded {
            bytes,
            format: format.into(),
            release_name: dl.file_name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(lang: &str, hash: bool, downloads: i64) -> Candidate {
        Candidate {
            provider: "opensubtitles",
            file_id: format!("{lang}-{downloads}"),
            language: Some(lang.into()),
            release_name: None,
            hash_match: hash,
            downloads,
            uploader: None,
            rating: None,
            fps: None,
        }
    }

    #[test]
    fn parses_the_reset_phrase() {
        assert_eq!(
            parse_reset_secs("23 hours and 12 minutes"),
            Some(23 * 3600 + 12 * 60)
        );
        assert_eq!(parse_reset_secs("45 minutes"), Some(45 * 60));
        assert_eq!(parse_reset_secs("30 seconds"), Some(30));
        assert_eq!(parse_reset_secs("tomorrow"), None);
    }

    /// The ranking the UI depends on: hash matches first, then the
    /// caller's language order, then popularity.
    #[test]
    fn ranks_hash_then_language_order_then_popularity() {
        let want = ["en".to_string(), "nl".to_string()];
        let mut out = vec![
            cand("nl", true, 500),
            cand("de", true, 9999), // not requested: after both wanted
            cand("en", true, 10),
            cand("en", false, 9999), // no hash: below every hash match
            cand("pt-BR", true, 1),
        ];
        rank_candidates(&mut out, &want);
        let order: Vec<_> = out
            .iter()
            .map(|c| (c.language.clone().unwrap(), c.hash_match))
            .collect();
        assert_eq!(
            order,
            vec![
                ("en".into(), true), // hash + first preferred language
                ("nl".into(), true), // hash + second
                ("de".into(), true), // hash, unrequested language
                ("pt-BR".into(), true),
                ("en".into(), false), // no hash, however popular
            ]
        );
    }
}
