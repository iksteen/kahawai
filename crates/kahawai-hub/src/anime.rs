//! Anime metadata plumbing (HUB-29): AniDB as the identity authority,
//! AniList as the description/relations source, bridged by the
//! community anime-lists mapping.
//!
//! AniDB discipline: the daily anime-titles dump answers ALL title
//! searches locally — zero API calls. (The UDP file-by-ED2K client
//! needs a registered AniDB client; it slots in behind settings later.)

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

const TITLES_URL: &str = "https://anidb.net/api/anime-titles.dat.gz";
/// AniDB says: at most one dump download per day per IP.
const TITLES_MAX_AGE: Duration = Duration::from_secs(24 * 3600);

const MAPPING_URL: &str =
    "https://raw.githubusercontent.com/Fribb/anime-lists/master/anime-list-full.json";
const MAPPING_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

const ANILIST_URL: &str = "https://graphql.anilist.co";

/// Download `url` to `path` unless a fresh copy exists (best-effort:
/// a stale cache beats a failed refresh).
async fn cached_download(
    http: &crate::gate::Http,
    url: &str,
    path: &Path,
    max_age: Duration,
) -> Result<()> {
    if let Ok(meta) = std::fs::metadata(path)
        && meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .map(|a| a < max_age)
            .unwrap_or(false)
    {
        return Ok(());
    }
    tracing::info!(url, "refreshing anime data file");
    match http
        .send(http.get(url))
        .await
        .and_then(|r| Ok(r.error_for_status()?))
    {
        Ok(resp) => {
            let bytes = resp.bytes().await?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, path)?;
            Ok(())
        }
        Err(e) if path.exists() => {
            tracing::warn!(url, error = %e, "refresh failed; using stale cache");
            Ok(())
        }
        Err(e) => Err(e).context("downloading"),
    }
}

// ---------- AniDB titles dump ----------

/// The daily titles dump, indexed for local matching. Lines:
/// `<aid>|<type>|<language>|<title>` — type 1=primary, 2=synonym,
/// 3=short, 4=official.
pub struct AnidbTitles {
    /// folded title → aids (deduped, primary/official first).
    index: HashMap<String, Vec<u32>>,
}

impl AnidbTitles {
    pub async fn load(http: &crate::gate::Http, data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("anime").join("anime-titles.dat.gz");
        cached_download(http, TITLES_URL, &path, TITLES_MAX_AGE).await?;
        let raw = std::fs::read(&path)?;
        let mut text = String::new();
        flate2::read::GzDecoder::new(&raw[..]).read_to_string(&mut text)?;

        // (aid, title-type) per folded title; official/primary first.
        let mut staging: HashMap<String, Vec<(u32, u8)>> = HashMap::new();
        for line in text.lines() {
            if line.starts_with('#') {
                continue;
            }
            let mut parts = line.splitn(4, '|');
            let (Some(aid), Some(ttype), Some(_lang), Some(title)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let (Ok(aid), Ok(ttype)) = (aid.parse::<u32>(), ttype.parse::<u8>()) else {
                continue;
            };
            if ttype == 3 {
                continue; // short forms collide wildly
            }
            let folded = crate::enrich::fold(title);
            if folded.is_empty() {
                continue;
            }
            staging.entry(folded).or_default().push((aid, ttype));
        }
        let mut index = HashMap::with_capacity(staging.len());
        for (k, mut v) in staging {
            // primary(1)/official(4) outrank synonyms(2)
            v.sort_by_key(|(_, t)| match t {
                1 => 0u8,
                4 => 1,
                _ => 2,
            });
            let mut seen = std::collections::HashSet::new();
            let aids: Vec<u32> = v
                .into_iter()
                .filter(|(a, _)| seen.insert(*a))
                .collect::<Vec<_>>()
                .into_iter()
                .map(|(a, _)| a)
                .collect();
            index.insert(k, aids);
        }
        tracing::info!(titles = index.len(), "anidb titles index ready");
        Ok(Self { index })
    }

    /// Candidate aids for a local title, best first.
    pub fn candidates(&self, title: &str) -> Vec<u32> {
        self.index
            .get(&crate::enrich::fold(title))
            .cloned()
            .unwrap_or_default()
    }
}

// ---------- anime-lists mapping ----------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Mapping {
    #[serde(default)]
    pub anidb_id: Option<u32>,
    #[serde(default)]
    pub anilist_id: Option<u32>,
    #[serde(default)]
    pub tvdb_id: Option<u32>,
    /// `themoviedb_id` ships as `{"movie": [ids…]}` or `{"tv": id}` (or,
    /// historically, a bare number).
    #[serde(default, rename = "themoviedb_id", deserialize_with = "tmdb_ids")]
    pub tmdb: TmdbIds,
    #[serde(default, rename = "type")]
    pub kind: Option<String>, // TV | MOVIE | OVA | ONA | SPECIAL | …
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TmdbIds {
    pub movie: Option<u32>,
    pub tv: Option<u32>,
}

fn tmdb_ids<'de, D: serde::Deserializer<'de>>(d: D) -> Result<TmdbIds, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(u32),
        Many(Vec<u32>),
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        Obj {
            movie: Option<OneOrMany>,
            tv: Option<OneOrMany>,
        },
        Bare(u32),
        Other(serde::de::IgnoredAny),
    }
    let first = |v: Option<OneOrMany>| match v {
        Some(OneOrMany::One(n)) => Some(n),
        Some(OneOrMany::Many(v)) => v.first().copied(),
        None => None,
    };
    Ok(match V::deserialize(d) {
        Ok(V::Obj { movie, tv }) => TmdbIds {
            movie: first(movie),
            tv: first(tv),
        },
        Ok(V::Bare(n)) => TmdbIds {
            movie: Some(n),
            tv: Some(n),
        },
        _ => TmdbIds::default(),
    })
}

impl Mapping {
    /// The TMDB id matching our item kind (movie vs show).
    pub fn tmdb_for(&self, kind: &str) -> Option<u32> {
        if kind == "movie" {
            self.tmdb.movie
        } else {
            self.tmdb.tv
        }
    }
}

pub struct AnimeLists {
    by_anidb: HashMap<u32, Mapping>,
    /// Reverse maps: one TVDB series covers many AniDB entries (per
    /// season); TMDB movie ids are ~1:1.
    by_tvdb: HashMap<u32, Vec<u32>>,
    by_tmdb: HashMap<u32, Vec<u32>>,
    by_anilist: HashMap<u32, Vec<u32>>,
}

impl AnimeLists {
    pub async fn load(http: &crate::gate::Http, data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("anime").join("anime-list-full.json");
        cached_download(http, MAPPING_URL, &path, MAPPING_MAX_AGE).await?;
        let raw = std::fs::read(&path)?;
        let entries: Vec<Mapping> = serde_json::from_slice(&raw)?;
        let mut by_anidb = HashMap::with_capacity(entries.len());
        let mut by_tvdb: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut by_tmdb: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut by_anilist: HashMap<u32, Vec<u32>> = HashMap::new();
        for e in entries {
            if let Some(aid) = e.anidb_id {
                if let Some(tvdb) = e.tvdb_id {
                    by_tvdb.entry(tvdb).or_default().push(aid);
                }
                for tmdb in [e.tmdb.movie, e.tmdb.tv].into_iter().flatten() {
                    by_tmdb.entry(tmdb).or_default().push(aid);
                }
                if let Some(al) = e.anilist_id {
                    by_anilist.entry(al).or_default().push(aid);
                }
                by_anidb.insert(aid, e);
            }
        }
        // Lowest AniDB id ≈ first season/original entry.
        for v in by_tvdb
            .values_mut()
            .chain(by_tmdb.values_mut())
            .chain(by_anilist.values_mut())
        {
            v.sort_unstable();
        }
        tracing::info!(entries = by_anidb.len(), "anime-lists mapping ready");
        Ok(Self {
            by_anidb,
            by_tvdb,
            by_tmdb,
            by_anilist,
        })
    }

    pub fn by_anidb(&self, aid: u32) -> Option<&Mapping> {
        self.by_anidb.get(&aid)
    }

    /// AniDB entries behind an existing generic-provider match — lets a
    /// manually matched item adopt anime ids WITHOUT re-deciding its
    /// identity.
    pub fn reverse(&self, provider: &str, provider_id: &str) -> Vec<u32> {
        let Ok(id) = provider_id.parse::<u32>() else {
            return Vec::new();
        };
        match provider {
            "tvdb" => self.by_tvdb.get(&id).cloned().unwrap_or_default(),
            "tmdb" => self.by_tmdb.get(&id).cloned().unwrap_or_default(),
            // Recovering an AniDB id from the AniList id we already hold,
            // which is how a title-matched anime is rebuilt: the mapping
            // is a recorded fact, unlike re-running the match.
            "anilist" => self.by_anilist.get(&id).cloned().unwrap_or_default(),
            _ => Vec::new(),
        }
    }
}

// ---------- AniList ----------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnilistMedia {
    pub id: u32,
    pub title: AnilistTitle,
    pub description: Option<String>,
    pub cover_image: Option<AnilistCover>,
    pub average_score: Option<f64>,
    pub start_date: Option<AnilistDate>,
    pub format: Option<String>, // TV | MOVIE | OVA | ONA | SPECIAL | TV_SHORT | MUSIC
    pub genres: Option<Vec<String>>,
    pub country_of_origin: Option<String>, // "JP" | "CN" | "KR" | …
    pub relations: Option<AnilistRelations>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnilistTitle {
    pub romaji: Option<String>,
    pub english: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnilistCover {
    pub extra_large: Option<String>,
    pub large: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnilistDate {
    pub year: Option<i32>,
    pub month: Option<i32>,
    pub day: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnilistRelations {
    pub edges: Vec<AnilistRelationEdge>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnilistRelationEdge {
    pub relation_type: Option<String>,
    pub node: Option<AnilistRelationNode>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnilistRelationNode {
    pub id: u32,
    pub title: AnilistTitle,
    pub format: Option<String>,
}

impl AnilistMedia {
    pub fn display_title(&self) -> Option<String> {
        self.title
            .english
            .clone()
            .or_else(|| self.title.romaji.clone())
    }

    /// Original language from the country of origin (anime is tagged by
    /// country, not language, on AniList).
    pub fn original_language(&self) -> Option<&'static str> {
        match self.country_of_origin.as_deref() {
            Some("JP") => Some("ja"),
            Some("CN") | Some("TW") => Some("zh"),
            Some("KR") => Some("ko"),
            _ => None,
        }
    }

    pub fn premiered(&self) -> Option<String> {
        let d = self.start_date.as_ref()?;
        Some(format!(
            "{:04}-{:02}-{:02}",
            d.year?,
            d.month.unwrap_or(1),
            d.day.unwrap_or(1)
        ))
    }

    /// AniList descriptions carry light HTML; flatten it.
    pub fn plain_description(&self) -> Option<String> {
        let d = self.description.as_ref()?;
        let mut out = d
            .replace("<br>", "\n")
            .replace("<br/>", "\n")
            .replace("<br />", "\n");
        for tag in ["<i>", "</i>", "<b>", "</b>", "<em>", "</em>"] {
            out = out.replace(tag, "");
        }
        Some(out.trim().to_string())
    }
}

const MEDIA_FIELDS: &str = "
  id
  title { romaji english }
  description(asHtml: false)
  coverImage { extraLarge large }
  averageScore
  startDate { year month day }
  format
  genres
  countryOfOrigin
  relations {
    edges { relationType node { id title { romaji english } format } }
  }";

pub struct Anilist {
    /// Pacing and 429 backoff live in the gate (gate.rs).
    http: std::sync::Arc<crate::gate::Http>,
}

impl Anilist {
    pub fn new(http: std::sync::Arc<crate::gate::Http>) -> Self {
        Self { http }
    }

    async fn gql(&self, query: &str, variables: serde_json::Value) -> Result<serde_json::Value> {
        let req = self
            .http
            .post(ANILIST_URL)
            .json(&serde_json::json!({"query": query, "variables": variables}));
        let resp = self
            .http
            .send(req)
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;
        // GraphQL reports failure in-band: a 200 whose body carries
        // `errors` must surface as an error, not deserialize into
        // "no such anime" — that once became a permanent recorded miss.
        if let Some(errs) = resp.get("errors").and_then(|e| e.as_array())
            && !errs.is_empty()
        {
            anyhow::bail!(
                "anilist graphql error: {}",
                errs[0]["message"].as_str().unwrap_or("?")
            );
        }
        Ok(resp)
    }

    pub async fn media_by_id(&self, id: u32) -> Result<Option<AnilistMedia>> {
        let q = format!("query($id: Int) {{ Media(id: $id, type: ANIME) {{{MEDIA_FIELDS}}} }}");
        let v = self.gql(&q, serde_json::json!({"id": id})).await?;
        Ok(serde_json::from_value(v["data"]["Media"].clone()).ok())
    }

    pub async fn search(&self, title: &str) -> Result<Vec<AnilistMedia>> {
        let q = format!(
            "query($s: String) {{ Page(perPage: 8) {{ media(search: $s, type: ANIME) {{{MEDIA_FIELDS}}} }} }}"
        );
        let v = self.gql(&q, serde_json::json!({"s": title})).await?;
        Ok(serde_json::from_value(v["data"]["Page"]["media"].clone()).unwrap_or_default())
    }
}

/// Does an AniList/mapping format fit our item kind?
pub fn format_fits(kind: &str, format: Option<&str>) -> bool {
    match (kind, format) {
        (_, None) => true, // unknown format never disqualifies
        ("movie", Some(f)) => f.eq_ignore_ascii_case("movie"),
        ("show", Some(f)) => !f.eq_ignore_ascii_case("movie") && !f.eq_ignore_ascii_case("music"),
        _ => true,
    }
}

/// Cache root for the anime data files.
pub fn anime_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("anime")
}

// ---------- AniDB HTTP API (episode titles) ----------

/// Registered HTTP-type client (separate registration from the UDP
/// client). Bumping the version requires updating anidb.net FIRST.
const HTTP_CLIENT: &str = "kahawaihttp";
const HTTP_CLIENT_VER: u32 = 1;

/// Minimum age before an anime XML may be re-fetched — AniDB's own
/// cache rule ("cache at least 24h"; re-requesting sooner risks bans).
const HTTP_MIN_CACHE: Duration = Duration::from_secs(24 * 3600);

/// Episode titles for one anime: absolute episode number → English
/// title (transcription fallback). Regular episodes only (epno type 1).
///
/// Caching is demand-driven, not TTL-driven: the XML re-fetches ONLY
/// when it fails to cover an episode we actually hold (an airing
/// series gained one) AND the cached copy is older than 24 h. Finished
/// shows therefore never re-fetch; airing shows re-fetch at most once
/// a day, and only on real growth. Error payloads are never cached.
pub async fn anidb_episode_titles(
    http: &crate::gate::Http,
    data_dir: &Path,
    aid: u32,
    wanted: &[i64],
) -> Result<std::collections::HashMap<i64, String>> {
    let dir = anime_dir(data_dir).join("httpapi");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{aid}.xml"));

    let cached = std::fs::read_to_string(&path).ok();
    let mut titles = match &cached {
        Some(xml) => parse_episode_titles(xml).unwrap_or_default(),
        None => Default::default(),
    };
    let covered = !titles.is_empty() && wanted.iter().all(|n| titles.contains_key(n));
    let old_enough = match std::fs::metadata(&path).and_then(|m| m.modified()) {
        Ok(t) => t.elapsed().unwrap_or_default() >= HTTP_MIN_CACHE,
        Err(_) => true, // no cache file yet
    };
    if covered || !old_enough {
        return Ok(titles);
    }

    let text = fetch_anime_xml(http, &path, aid).await?;
    titles = parse_episode_titles(&text)?;
    Ok(titles)
}

/// One page every two seconds, and a 24 h silence if they ban us: both
/// live in the gate, keyed on api.anidb.net.
async fn fetch_anime_xml(http: &crate::gate::Http, path: &Path, aid: u32) -> Result<String> {
    let req = http.get("http://api.anidb.net:9001/httpapi").query(&[
        ("request", "anime"),
        ("client", HTTP_CLIENT),
        ("clientver", &HTTP_CLIENT_VER.to_string()),
        ("protover", "1"),
        ("aid", &aid.to_string()),
    ]);
    let raw = http.send(req).await?.error_for_status()?.bytes().await?;
    // Responses are usually gzip'd regardless of headers.
    let text = if raw.starts_with(&[0x1f, 0x8b]) {
        use std::io::Read;
        let mut s = String::new();
        flate2::read::GzDecoder::new(&raw[..]).read_to_string(&mut s)?;
        s
    } else {
        String::from_utf8_lossy(&raw).into_owned()
    };
    anyhow::ensure!(
        !text.trim_start().starts_with("<error"),
        "anidb http api error for aid {aid}: {}",
        text.trim().chars().take(120).collect::<String>()
    );
    std::fs::write(path, &text)?;
    tracing::info!(aid, "anidb anime xml fetched");
    Ok(text)
}

/// What an aid IS, from the per-anime XML: enough to mint a movie item
/// from a hash identity (HUB-30). Cache-first, ask-once — the same file
/// the episode-titles path keeps.
pub struct AnidbAnimeInfo {
    /// AniDB's own type string: "Movie", "TV Series", "OVA", "Web", …
    pub kind: String,
    pub title: String,
    pub year: Option<i64>,
    /// `<episodecount>`, when the XML states one. A single-episode
    /// OVA/Web entry is movie-shaped for minting purposes (Kite
    /// Liberator, 2026-07-28: type OVA, one episode, invisible until
    /// this let it mint).
    pub episode_count: Option<i64>,
}

pub async fn anidb_anime_info(
    http: &crate::gate::Http,
    data_dir: &Path,
    aid: u32,
) -> Result<AnidbAnimeInfo> {
    let dir = anime_dir(data_dir).join("httpapi");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{aid}.xml"));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => fetch_anime_xml(http, &path, aid).await?,
    };
    let doc = roxmltree::Document::parse(&text)?;
    let kind = doc
        .descendants()
        .find(|n| n.has_tag_name("type"))
        .and_then(|n| n.text())
        .unwrap_or("")
        .to_string();
    // The official English title where one exists, else the main
    // (romaji) title — the preference the rest of the pipeline shows.
    let titles: Vec<_> = doc
        .descendants()
        .filter(|n| n.has_tag_name("title"))
        .collect();
    let title = titles
        .iter()
        .find(|n| {
            n.attribute("type") == Some("official")
                && n.attribute(("http://www.w3.org/XML/1998/namespace", "lang")) == Some("en")
        })
        .or_else(|| titles.iter().find(|n| n.attribute("type") == Some("main")))
        .and_then(|n| n.text())
        .unwrap_or("")
        .to_string();
    let year = doc
        .descendants()
        .find(|n| n.has_tag_name("startdate"))
        .and_then(|n| n.text())
        .and_then(|d| d.get(..4))
        .and_then(|y| y.parse().ok());
    let episode_count = doc
        .descendants()
        .find(|n| n.has_tag_name("episodecount"))
        .and_then(|n| n.text())
        .and_then(|c| c.parse().ok());
    anyhow::ensure!(!title.is_empty(), "anidb xml for {aid} carries no title");
    Ok(AnidbAnimeInfo {
        kind,
        title,
        year,
        episode_count,
    })
}

fn parse_episode_titles(xml: &str) -> Result<std::collections::HashMap<i64, String>> {
    let doc = roxmltree::Document::parse(xml)?;
    let mut out = std::collections::HashMap::new();
    for ep in doc.descendants().filter(|n| n.has_tag_name("episode")) {
        let Some(epno) = ep.children().find(|n| n.has_tag_name("epno")) else {
            continue;
        };
        if epno.attribute("type") != Some("1") {
            continue; // specials/credits/trailers live in their own space
        }
        let Some(n) = epno.text().and_then(|t| t.trim().parse::<i64>().ok()) else {
            continue;
        };
        let title_for = |lang: &str| {
            ep.children()
                .find(|c| {
                    c.has_tag_name("title")
                        && c.attribute(("http://www.w3.org/XML/1998/namespace", "lang"))
                            == Some(lang)
                })
                .and_then(|c| c.text())
                .map(str::to_string)
        };
        if let Some(t) = title_for("en").or_else(|| title_for("x-jat")) {
            out.insert(n, t);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_parses_real_schema() {
        let json = r#"[
          {"anidb_id": 1218, "anilist_id": 431, "type": "MOVIE",
           "themoviedb_id": {"movie": [4935]}},
          {"anidb_id": 4563, "anilist_id": 1535, "tvdb_id": 79481, "type": "TV",
           "themoviedb_id": {"tv": 30984}},
          {"anilist_id": 99999},
          {"anidb_id": 7729, "type": "MOVIE", "themoviedb_id": null},
          {"anidb_id": 1, "themoviedb_id": 42}
        ]"#;
        let entries: Vec<Mapping> = serde_json::from_str(json).unwrap();
        assert_eq!(entries[0].tmdb.movie, Some(4935));
        assert_eq!(entries[0].tmdb_for("movie"), Some(4935));
        assert_eq!(entries[1].tvdb_id, Some(79481));
        assert_eq!(entries[1].tmdb_for("show"), Some(30984));
        assert_eq!(entries[3].tmdb.movie, None);
        assert_eq!(entries[4].tmdb.movie, Some(42));
    }

    #[test]
    fn format_fitting() {
        assert!(format_fits("movie", Some("MOVIE")));
        assert!(!format_fits("movie", Some("TV")));
        assert!(format_fits("show", Some("TV")));
        assert!(format_fits("show", Some("ONA")));
        assert!(!format_fits("show", Some("MOVIE")));
        assert!(format_fits("show", None));
    }

    #[test]
    fn description_flattening() {
        let m = AnilistMedia {
            id: 1,
            title: AnilistTitle {
                romaji: Some("X".into()),
                english: None,
            },
            country_of_origin: None,
            description: Some("Line one.<br><br>\n<i>Note.</i>".into()),
            cover_image: None,
            average_score: Some(78.0),
            start_date: Some(AnilistDate {
                year: Some(2011),
                month: Some(4),
                day: None,
            }),
            format: Some("TV".into()),
            genres: None,
            relations: None,
        };
        assert_eq!(m.plain_description().unwrap(), "Line one.\n\n\nNote.");
        assert_eq!(m.premiered().unwrap(), "2011-04-01");
        assert_eq!(m.display_title().unwrap(), "X");
    }
}
