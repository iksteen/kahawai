//! HUB-21/22/24: external subtitle provider. OpenSubtitles.com REST
//! client behind a small trait so a second provider is one impl. The
//! app key ships with the binary (see DEFAULT_API_KEY) and grants
//! anonymous use: 5 requests/s and 5 downloads per 24 h. Configuring an
//! account (settings) only raises the download quota.
//!
//! Matching (HUB-22): the file's OpenSubtitles moviehash — which is
//! exactly the `oshash` the mediahost already computes (size + u64-LE
//! sums of the first/last 64 KiB) — is the primary search key, with a
//! title/year fallback for files the provider doesn't recognize by hash.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

const API: &str = "https://api.opensubtitles.com/api/v1";
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

pub const KEY_SETTING: &str = "opensubtitles.api_key";
pub const USER_SETTING: &str = "opensubtitles.username";
pub const PASS_SETTING: &str = "opensubtitles.password";

/// One search result the user can choose to download.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Candidate {
    pub provider: &'static str,
    /// Opaque download handle (OpenSubtitles file_id, stringified).
    pub file_id: String,
    pub language: Option<String>,
    pub release_name: Option<String>,
    /// True when the provider matched by moviehash (exact file, HUB-22)
    /// rather than by title — surfaced so the UI can rank/badge it.
    pub hash_match: bool,
    pub downloads: i64,
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
}

pub struct SearchQuery {
    pub moviehash: Option<u64>,
    pub title: Option<String>,
    pub year: Option<i64>,
    /// Season/episode for series (OpenSubtitles wants them explicitly).
    pub season: Option<i64>,
    pub episode: Option<i64>,
    pub languages: Vec<String>,
}

pub struct OpenSubtitles {
    http: reqwest::Client,
    api_key: String,
    username: Option<String>,
    password: Option<String>,
    /// Cached download token (OpenSubtitles issues it via /login and it
    /// lasts ~24 h; download quota is tracked against it).
    token: tokio::sync::Mutex<Option<String>>,
}

impl OpenSubtitles {
    pub fn new(
        http: reqwest::Client,
        api_key: String,
        username: Option<String>,
        password: Option<String>,
    ) -> Self {
        Self { http, api_key, username, password, token: tokio::sync::Mutex::new(None) }
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
        let resp = self
            .req(reqwest::Method::POST, "/login")
            .json(&serde_json::json!({ "username": user, "password": pass }))
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("OpenSubtitles login failed: {} {}", resp.status(), resp.text().await.unwrap_or_default());
        }
        let token = resp.json::<LoginResp>().await?.token;
        *self.token.lock().await = Some(token.clone());
        Ok(Some(token))
    }
}

/// Rank results: exact-file (hash) matches first, then the caller's
/// language order — the request's list is a preference, not merely a
/// filter — then popularity. Languages not in the list sort last (they
/// only appear at all in an unfiltered search).
fn rank_candidates(out: &mut [Candidate], languages: &[String]) {
    let lang_rank = |l: &Option<String>| -> usize {
        let Some(l) = l.as_deref() else { return usize::MAX };
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
        }
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
                ("en".into(), true),   // hash + first preferred language
                ("nl".into(), true),   // hash + second
                ("de".into(), true),   // hash, unrequested language
                ("pt-BR".into(), true),
                ("en".into(), false),  // no hash, however popular
            ]
        );
    }
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
            q.moviehash.is_some() || q.title.is_some(),
            "subtitle search needs a hash or a title"
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
            files: Vec<FileRef>,
        }
        #[derive(Deserialize)]
        struct FileRef {
            file_id: i64,
        }

        let resp = self
            .req(reqwest::Method::GET, "/subtitles")
            .query(&params)
            .send()
            .await?
            .error_for_status()
            .context("OpenSubtitles search")?;
        let parsed: Resp = resp.json().await?;
        let mut out = Vec::new();
        for item in parsed.data {
            let a = item.attributes;
            let Some(f) = a.files.into_iter().next() else { continue };
            out.push(Candidate {
                provider: "opensubtitles",
                file_id: f.file_id.to_string(),
                language: a.language,
                release_name: a.release,
                hash_match: a.moviehash_match,
                downloads: a.download_count,
            });
        }
        rank_candidates(&mut out, &q.languages);
        Ok(out)
    }

    async fn download(&self, file_id: &str) -> Result<Downloaded> {
        let file_id: i64 = file_id.parse().context("bad file_id")?;
        let token = self.token().await?;
        #[derive(Deserialize)]
        struct DlResp {
            link: String,
            #[serde(default)]
            file_name: Option<String>,
        }
        let mut req = self
            .req(reqwest::Method::POST, "/download")
            .json(&serde_json::json!({ "file_id": file_id }));
        if let Some(t) = &token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            *self.token.lock().await = None; // force re-login next time
            bail!("OpenSubtitles rejected the download token — retry");
        }
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
            || resp.status() == reqwest::StatusCode::PAYMENT_REQUIRED
        {
            bail!(
                "OpenSubtitles download quota exhausted{} — it resets 24 h after your first \
                 download today",
                if token.is_some() { "" } else { " (anonymous: 5 per 24 h; add an account for more)" }
            );
        }
        let dl: DlResp = resp.error_for_status().context("OpenSubtitles download")?.json().await?;
        let bytes = self
            .http
            .get(&dl.link)
            .header("User-Agent", USER_AGENT)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?
            .to_vec();
        // OpenSubtitles serves .srt overwhelmingly; sniff ASS.
        let format = if dl
            .file_name
            .as_deref()
            .is_some_and(|n| n.to_ascii_lowercase().ends_with(".ass") || n.to_ascii_lowercase().ends_with(".ssa"))
            || bytes.starts_with(b"[Script Info]")
        {
            "ass"
        } else {
            "srt"
        };
        Ok(Downloaded { bytes, format: format.into(), release_name: dl.file_name })
    }
}
