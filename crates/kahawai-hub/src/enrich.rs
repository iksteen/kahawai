//! Provider clients and credentials for catalogue enrichment.
//! `catalogue` runs independent provider workers against mediadb's durable jobs.
//! Providers publish their own answers; mediadb resolves metadata and identity.
//! Every external request goes through the shared provider gate.

mod catalogue;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::ToSchema;

use crate::registry::Registry;

/// `error_for_status`, without the URL.
///
/// TMDB v3 carries its API key in the query string and reqwest's `Display`
/// appends the URL it failed on, so one 404 logs the operator's key. Keeping
/// it a `reqwest::Error` is what lets [`is_http_status`] still read the status.
trait StatusChecked: Sized {
    fn status_checked(self) -> Result<reqwest::Response>;
}

impl StatusChecked for reqwest::Response {
    fn status_checked(self) -> Result<reqwest::Response> {
        self.error_for_status()
            .map_err(|e| anyhow::Error::new(e.without_url()))
    }
}

/// TMDB's and TVDB's names in the credential store. Beside the code that uses
/// them: the store keys rows by (owner, provider, field) and does not care
/// what any of them mean.
pub const TMDB: &str = "tmdb";
pub const TMDB_API_KEY: &str = "api_key";
pub const TVDB: &str = "tvdb";
pub const TVDB_API_KEY: &str = "api_key";
pub const TVDB_PIN: &str = "pin";
pub const FANART: &str = "fanart";
pub const FANART_CLIENT_KEY: &str = "client_key";
pub const THEAUDIODB: &str = "theaudiodb";
pub const THEAUDIODB_API_KEY: &str = "api_key";
/// TheAudioDB publishes `123` as the shared test/free API key.
/// Source: https://www.theaudiodb.com/free_music_api (read 2026-09-03).
pub const THEAUDIODB_FREE_API_KEY: &str = "123";

/// This deployment's TMDB key, or `None` if it has not been configured.
pub async fn tmdb_key(registry: &Registry) -> Result<Option<String>> {
    Ok(registry.hub_credential(TMDB).await?.remove(TMDB_API_KEY))
}

/// This deployment's Fanart.tv personal key, or `None` when artist portraits
/// are not configured. Fanart is auxiliary artwork, not a member of a metadata
/// chain. Fanart's API calls this a `client_key`; an `api_key` is a different,
/// project-level credential.
pub async fn fanart_client_key(registry: &Registry) -> Result<Option<String>> {
    Ok(registry
        .hub_credential(FANART)
        .await?
        .remove(FANART_CLIENT_KEY))
}

/// A configured premium key replaces TheAudioDB's documented `123` free key.
/// Absence is therefore an active free-tier provider, not "unconfigured".
pub async fn theaudiodb_premium_key(registry: &Registry) -> Result<Option<String>> {
    Ok(registry
        .hub_credential(THEAUDIODB)
        .await?
        .remove(THEAUDIODB_API_KEY))
}

/// This deployment's TVDB credentials. The pin is optional — a subscriber
/// has one and nobody else does — so its absence is not "unconfigured".
pub(crate) async fn tvdb_creds(registry: &Registry) -> Result<Option<TvdbCreds>> {
    let mut fields = registry.hub_credential(TVDB).await?;
    Ok(fields.remove(TVDB_API_KEY).map(|key| TvdbCreds {
        key,
        pin: fields.remove(TVDB_PIN),
    }))
}

struct MbReleaseGroup {
    id: String,
    artist_id: String,
    title: String,
    first_release_date: Option<String>,
    genres: Vec<String>,
}

#[derive(Deserialize)]
struct FanartArtist {
    #[serde(default)]
    artistthumb: Vec<FanartImage>,
    #[serde(default)]
    artistbackground: Vec<FanartImage>,
}

#[derive(Deserialize)]
struct FanartImage {
    id: String,
    url: String,
    #[serde(default)]
    likes: String,
}

#[derive(Deserialize)]
struct TheAudioDbResponse {
    artists: Option<Vec<TheAudioDbArtist>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TheAudioDbArtist {
    #[serde(default)]
    str_artist: String,
    #[serde(default, rename = "strMusicBrainzID")]
    str_music_brainz_id: String,
    str_artist_thumb: Option<String>,
    str_artist_fanart: Option<String>,
    str_artist_fanart2: Option<String>,
    str_artist_fanart3: Option<String>,
    str_artist_fanart4: Option<String>,
}

enum TheAudioDbAnswer {
    Missing,
    Artist(TheAudioDbArtist),
    Ambiguous,
}

impl TheAudioDbArtist {
    fn best_image(&self) -> Option<(&str, &str)> {
        [
            ("thumb", self.str_artist_thumb.as_deref()),
            ("fanart", self.str_artist_fanart.as_deref()),
            ("fanart2", self.str_artist_fanart2.as_deref()),
            ("fanart3", self.str_artist_fanart3.as_deref()),
            ("fanart4", self.str_artist_fanart4.as_deref()),
        ]
        .into_iter()
        .find_map(|(kind, url)| url.filter(|url| !url.is_empty()).map(|url| (kind, url)))
    }
}

fn exact_theaudiodb_artist(
    response: TheAudioDbResponse,
    wanted_id: &str,
    wanted_name: &str,
) -> TheAudioDbAnswer {
    let Some(mut artists) = response.artists else {
        return TheAudioDbAnswer::Missing;
    };
    if artists.len() != 1 {
        return if artists.is_empty() {
            TheAudioDbAnswer::Missing
        } else {
            TheAudioDbAnswer::Ambiguous
        };
    }
    let artist = artists.pop().expect("one artist checked above");
    if artist.str_music_brainz_id != wanted_id
        || artist_key(&artist.str_artist) != artist_key(wanted_name)
    {
        return TheAudioDbAnswer::Ambiguous;
    }
    TheAudioDbAnswer::Artist(artist)
}

impl FanartArtist {
    fn best_image(&mut self) -> Option<&FanartImage> {
        fn best(images: &mut [FanartImage]) -> Option<&FanartImage> {
            images.sort_by(|a, b| {
                b.likes
                    .parse::<i64>()
                    .unwrap_or_default()
                    .cmp(&a.likes.parse::<i64>().unwrap_or_default())
                    .then_with(|| a.id.cmp(&b.id))
            });
            images.first()
        }
        // `artistthumb` is already composed for a square card. Fanart has
        // substantially wider coverage in `artistbackground`; the web's
        // square, object-cover frame crops that fallback without changing the
        // API or putting image work on the browse path.
        if let Some(image) = best(&mut self.artistthumb) {
            return Some(image);
        }
        best(&mut self.artistbackground)
    }
}

fn trusted_artist_image(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some_and(|host| {
                host == "fanart.tv"
                    || host.ends_with(".fanart.tv")
                    || host == "theaudiodb.com"
                    || host.ends_with(".theaudiodb.com")
            })
    })
}

/// Resolve one exact artist credit without borrowing title-search folding for
/// identity. MusicBrainz search is deliberately fuzzy; the acceptance check is
/// not. In particular, `One`/`1` and `P!nk`/`Pink` are separate catalogue
/// groups and must not donate identities (and therefore portraits) to each
/// other.
fn exact_artist_credit_id(
    credits: Option<&Vec<serde_json::Value>>,
    wanted_artist: &str,
) -> Option<String> {
    let wanted = artist_key(wanted_artist);
    let matches: BTreeSet<String> = credits
        .into_iter()
        .flatten()
        .filter_map(|credit| {
            let exact = credit["name"]
                .as_str()
                .is_some_and(|name| artist_key(name) == wanted)
                || credit["artist"]["name"]
                    .as_str()
                    .is_some_and(|name| artist_key(name) == wanted);
            exact
                .then(|| credit["artist"]["id"].as_str().map(str::to_string))
                .flatten()
        })
        .collect();
    (matches.len() == 1)
        .then(|| matches.into_iter().next())
        .flatten()
}

/// MusicBrainz artist search is fuzzy even for a quoted query. Accept only a
/// single distinct result whose catalogue name has the exact local artist
/// identity; same-named artists remain unresolved instead of receiving a
/// plausible stranger's portrait.
fn exact_artist_search_id(
    artists: Option<&Vec<serde_json::Value>>,
    wanted_artist: &str,
) -> Option<String> {
    let wanted = artist_key(wanted_artist);
    let matches: BTreeSet<String> = artists
        .into_iter()
        .flatten()
        .filter(|artist| {
            artist["name"]
                .as_str()
                .is_some_and(|name| artist_key(name) == wanted)
        })
        .filter_map(|artist| artist["id"].as_str().map(str::to_string))
        .collect();
    (matches.len() == 1)
        .then(|| matches.into_iter().next())
        .flatten()
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
    catalogue_started: AtomicBool,
    catalogue_import: tokio::sync::OnceCell<()>,
    catalogue_wake: tokio::sync::Notify,
    /// Every provider call goes out through this: pacing and
    /// rate-limit backoff live in `gate.rs`, not at the call sites.
    http: std::sync::Arc<crate::gate::Http>,
    data_dir: std::path::PathBuf,
    anilist: crate::anime::Anilist,
    /// Disconnect epochs. A copied credential is usable only while its epoch
    /// matches. `watch` wakes requests parked in a provider queue immediately;
    /// its value prevents reconnecting from reviving work that still holds the
    /// old credential.
    tmdb_generation: tokio::sync::watch::Sender<u64>,
    tvdb_generation: tokio::sync::watch::Sender<u64>,
    anidb_generation: tokio::sync::watch::Sender<u64>,
    fanart_generation: tokio::sync::watch::Sender<u64>,
    theaudiodb_generation: tokio::sync::watch::Sender<u64>,
    /// Orders credential replacement and disconnect through their database
    /// write plus epoch change. Never held across provider I/O.
    credential_change: tokio::sync::Mutex<()>,
    /// The UDP session, kept for the PROCESS lifetime — not per run.
    /// A login per enrichment run is what got this client banned twice
    /// in one evening; sessions are cheap to hold and expensive to
    /// re-establish.
    anidb: tokio::sync::Mutex<Option<crate::anidb::Anidb>>,
    /// Set when the ACCOUNT changes, so the held session — which belongs to
    /// the previous account, and spends its quota and carries its ban risk —
    /// is dropped before the next run instead of at the next restart.
    anidb_stale: AtomicBool,
    /// TheTVDB's bearer token, fetched on FIRST USE and kept for the
    /// process (it is valid for weeks). Lazy so that a login failure
    /// cannot remove TVDB from a whole run: TMDB is present whenever
    /// its key is set and fails per request, and TVDB behaving
    /// differently made a transient outage indistinguishable from "no
    /// TVDB configured" — including to the selection, which then
    /// stopped counting TVDB work as owed.
    ///
    /// Labelled with a fingerprint of the credential that minted it. A key
    /// rotated because it leaked would otherwise keep working through the
    /// token the old one bought, for weeks, with the new key never
    /// exercised. Labelling it beats invalidating from the admin route,
    /// which would have to reach into this mutex — held across a login that
    /// `gate` can park for as long as TVDB's Retry-After asks, up to an hour.
    tvdb: tokio::sync::Mutex<Option<(String, std::sync::Arc<String>)>>,
    /// The byte plane, for HUB-9: reading a .nfo means leasing it from the
    /// mediahost that holds it. Attached at startup; absent in tests, where
    /// the local provider then simply is not in the chain.
    sessions: std::sync::OnceLock<Arc<crate::sessions::Sessions>>,
    /// Weak to avoid the cycle: Artwork uses this enricher for ordinary
    /// provider posters, while artist enrichment asks Artwork to prewarm the
    /// same durable cache before publishing a portrait.
    artwork: std::sync::OnceLock<std::sync::Weak<crate::artwork::Artwork>>,
}

/// Names the credential a token was minted from, without keeping a second
/// copy of it. Lengths go in with the parts, so a key that ends where a pin
/// begins cannot fingerprint as another pair.
fn tvdb_fingerprint(creds: &TvdbCreds) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update((creds.key.len() as u64).to_le_bytes());
    hasher.update(creds.key.as_bytes());
    match creds.pin.as_deref() {
        Some(pin) => {
            hasher.update([1u8]);
            hasher.update(pin.as_bytes());
        }
        None => hasher.update([0u8]),
    }
    data_encoding::HEXLOWER.encode(&hasher.finalize())
}

/// The cached token, but only for the credential that bought it.
fn cached_token(
    slot: &Option<(String, std::sync::Arc<String>)>,
    want: &str,
) -> Option<std::sync::Arc<String>> {
    slot.as_ref()
        .filter(|(minted_from, _)| minted_from == want)
        .map(|(_, token)| token.clone())
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EnrichStatus {
    pub running: bool,
    pub matched: usize,
    pub weak: usize,
    pub missed: usize,
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

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CatalogCandidate {
    pub id: u64,
    pub provider: &'static str,
    pub title: String,
    #[schema(required)]
    pub original_title: Option<String>,
    #[schema(required)]
    pub overview: Option<String>,
    #[schema(required)]
    pub poster_path: Option<String>,
    #[schema(required)]
    pub vote_average: Option<f64>,
    #[schema(required)]
    pub release_date: Option<String>,
    #[schema(required)]
    pub original_language: Option<String>,
    pub format: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AnilistCandidate {
    pub id: u32,
    pub provider: &'static str,
    #[schema(required)]
    pub title: Option<String>,
    #[schema(required)]
    pub overview: Option<String>,
    #[schema(required)]
    pub poster_path: Option<String>,
    #[schema(required)]
    pub poster_url: Option<String>,
    #[schema(required)]
    pub release_date: Option<String>,
    #[schema(required)]
    pub vote_average: Option<f64>,
    #[schema(required)]
    pub format: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(untagged)]
pub enum ProviderCandidate {
    Catalog(CatalogCandidate),
    Anilist(AnilistCandidate),
}

impl ProviderCandidate {
    fn title(&self) -> Option<&str> {
        match self {
            Self::Catalog(candidate) => Some(&candidate.title),
            Self::Anilist(candidate) => candidate.title.as_deref(),
        }
    }

    fn release_date(&self) -> Option<&str> {
        match self {
            Self::Catalog(candidate) => candidate.release_date.as_deref(),
            Self::Anilist(candidate) => candidate.release_date.as_deref(),
        }
    }

    fn vote_average(&self) -> Option<f64> {
        match self {
            Self::Catalog(candidate) => candidate.vote_average,
            Self::Anilist(candidate) => candidate.vote_average,
        }
    }

    fn is_anilist(&self) -> bool {
        matches!(self, Self::Anilist(_))
    }
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

/// Is this error, anywhere in its chain, an HTTP 404? A mapped id that
/// the provider answers 404 for is an ANSWER — "no such record" (legacy
/// series ids TVDB v4 dropped, movies that changed namespace) — and
/// must record its question and decline terminally, not reschedule
/// forever as if the network had hiccuped.
fn is_http_status(e: &anyhow::Error, wanted: reqwest::StatusCode) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<reqwest::Error>()
            .and_then(reqwest::Error::status)
            .is_some_and(|status| status == wanted)
    })
}

fn is_http_transient(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause.downcast_ref::<reqwest::Error>().is_some_and(|error| {
            error
                .status()
                .is_some_and(|status| status.is_server_error())
                || (error.status().is_none() && !error.is_builder())
        })
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

/// Stable identity for an artist credit, not a fuzzy search key.
///
/// Case, accents and whitespace are typographic differences in tags. Keep
/// punctuation and number spelling, however: `P!nk`, `Pink`, `One` and `1`
/// are all names the catalogue must be able to keep apart. In particular, a
/// punctuation-only artist must remain a usable, non-empty API route key.
pub(crate) fn artist_key(s: &str) -> String {
    use data_encoding::BASE64URL_NOPAD;
    use unicode_normalization::UnicodeNormalization;
    let folded: String = s
        .nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .collect();
    let identity = folded.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("artist-{}", BASE64URL_NOPAD.encode(identity.as_bytes()))
}

#[cfg(test)]
mod artist_key_tests {
    use super::artist_key;

    #[test]
    fn artist_identity_ignores_typography_without_becoming_fuzzy() {
        assert_eq!(artist_key("Beyoncé"), artist_key("  BEYONCE  "));
        assert_ne!(artist_key("One"), artist_key("1"));
        assert_ne!(artist_key("P!nk"), artist_key("Pink"));
        assert_eq!(artist_key("!!!"), "artist-ISEh");
        assert!(
            artist_key("AC/DC")
                .bytes()
                .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') })
        );
    }
}

/// Which TMDB credential style this is: the v3 "API Key" rides the `api_key`
/// query parameter, the v4 "API Read Access Token" is a JWT and rides a Bearer
/// header.
///
/// Trimmed only to LOOK at it. A key is stored exactly as its owner typed it,
/// and a leading space is not ours to reject — but it must not decide the
/// transport, because the fallback puts a long-lived token in a URL, where
/// TMDB's access log and every intermediary between here and there keeps it.
fn is_v4_token(key: &str) -> bool {
    key.trim_start().starts_with("eyJ")
}

impl Enricher {
    pub fn new(data_dir: std::path::PathBuf) -> Self {
        let http = std::sync::Arc::new(crate::gate::Http::new().expect("http client"));
        Self {
            catalogue_started: AtomicBool::new(false),
            catalogue_import: Default::default(),
            catalogue_wake: Default::default(),
            anilist: crate::anime::Anilist::new(http.clone()),
            data_dir,
            http,
            tmdb_generation: tokio::sync::watch::channel(0).0,
            tvdb_generation: tokio::sync::watch::channel(0).0,
            anidb_generation: tokio::sync::watch::channel(0).0,
            fanart_generation: tokio::sync::watch::channel(0).0,
            theaudiodb_generation: tokio::sync::watch::channel(0).0,
            credential_change: Default::default(),
            anidb: Default::default(),
            tvdb: Default::default(),
            anidb_stale: AtomicBool::new(false),
            sessions: Default::default(),
            artwork: Default::default(),
        }
    }

    /// Wire the byte plane in (HUB-9). Without it the local provider is
    /// left out of the chain rather than failing per item.
    pub fn attach_sessions(&self, sessions: Arc<crate::sessions::Sessions>) {
        let _ = self.sessions.set(sessions);
    }

    pub fn attach_artwork(&self, artwork: &Arc<crate::artwork::Artwork>) {
        let _ = self.artwork.set(Arc::downgrade(artwork));
    }

    /// Where anime/AniDB state lives (the api's verify path needs it).
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    pub(crate) async fn changing_credentials(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.credential_change.lock().await
    }

    fn provider_epoch_sender(&self, provider: &str) -> &tokio::sync::watch::Sender<u64> {
        match provider {
            TMDB => &self.tmdb_generation,
            TVDB => &self.tvdb_generation,
            crate::anidb::ANIDB => &self.anidb_generation,
            FANART => &self.fanart_generation,
            THEAUDIODB => &self.theaudiodb_generation,
            _ => unreachable!("unknown credentialed provider"),
        }
    }

    pub(crate) fn provider_lease(&self, provider: &'static str) -> crate::gate::CredentialLease {
        crate::gate::CredentialLease::new(
            provider,
            self.provider_epoch_sender(provider).subscribe(),
        )
    }

    pub(crate) async fn credential_snapshot(
        &self,
        registry: &Registry,
        provider: &'static str,
    ) -> Result<(BTreeMap<String, String>, crate::gate::CredentialLease)> {
        let change = self.changing_credentials().await;
        let fields = registry.hub_credential(provider).await?;
        let lease = self.provider_lease(provider);
        drop(change);
        Ok((fields, lease))
    }

    /// Invalidate credentials already copied into an active enrichment run.
    /// Parked HTTP and AniDB requests wake and stop before their next send.
    pub(crate) fn revoke_provider(&self, provider: &str) {
        self.provider_epoch_sender(provider)
            .send_modify(|generation| *generation += 1);
        if provider == crate::anidb::ANIDB {
            self.anidb_forget();
        }
    }

    async fn search(
        &self,
        key: &str,
        kind: &str,
        title: &str,
        year: Option<i64>,
        lease: &crate::gate::CredentialLease,
    ) -> Result<Vec<Candidate>> {
        let endpoint = if kind == "movie" { "movie" } else { "tv" };
        let mut req = self
            .http
            .get(format!("https://api.themoviedb.org/3/search/{endpoint}"))
            .query(&[("query", title), ("include_adult", "false")]);
        // Both TMDB credential styles work: the v3 "API Key" rides the
        // api_key query param, the v4 "API Read Access Token" (a JWT,
        // starts with "eyJ") rides a Bearer header.
        if is_v4_token(key) {
            req = req.bearer_auth(key);
        } else {
            req = req.query(&[("api_key", key)]);
        }
        if let (Some(y), "movie") = (year, endpoint) {
            req = req.query(&[("year", y.to_string())]);
        }
        let resp = self
            .http
            .send_current(req, lease.clone())
            .await
            .context("tmdb request")?;
        anyhow::ensure!(
            resp.status() != reqwest::StatusCode::UNAUTHORIZED,
            "TMDB rejected the API key"
        );
        let resp = resp.status_checked().context("tmdb response")?;
        Ok(resp
            .json::<SearchResponse>()
            .await
            .context("tmdb json")?
            .results)
    }

    /// The cached bearer token, logging in on first use. Concurrent
    /// callers queue on the mutex, so a fleet of episode tasks starting
    /// together still costs one login.
    pub(crate) async fn tvdb_token(
        &self,
        creds: &TvdbCreds,
        lease: &crate::gate::CredentialLease,
    ) -> Result<std::sync::Arc<String>> {
        let want = tvdb_fingerprint(creds);
        let mut slot = lease.wait(self.tvdb.lock()).await?;
        if let Some(t) = cached_token(&slot, &want) {
            return Ok(t);
        }
        let token = std::sync::Arc::new(
            self.tvdb_login(&creds.key, creds.pin.as_deref(), lease)
                .await?,
        );
        // Safe to store even if the key was replaced while this login was in
        // the air: it is stored under the credential it came from, so the next
        // caller — carrying the key stored now — will not be given it.
        *slot = Some((want, token.clone()));
        Ok(token)
    }

    /// Forget the logged-in AniDB client, so the next run authenticates with
    /// whatever account is configured now. Setting a flag rather than taking
    /// the mutex: a lookup holds it across a UDP round trip, and an admin
    /// saving credentials should not wait on that.
    pub(crate) fn anidb_forget(&self) {
        self.anidb_stale.store(true, Ordering::Release);
    }

    /// TheTVDB v4: login yields a bearer token (valid for weeks; we
    /// fetch one per run).
    async fn tvdb_login(
        &self,
        key: &str,
        pin: Option<&str>,
        lease: &crate::gate::CredentialLease,
    ) -> Result<String> {
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
            .send_current(
                self.http
                    .post("https://api4.thetvdb.com/v4/login")
                    .json(&body),
                lease.clone(),
            )
            .await
            .context("tvdb login request")?
            .status_checked()
            .context("tvdb login rejected (key/pin?)")?;
        Ok(resp
            .json::<LoginResp>()
            .await
            .context("tvdb login json")?
            .data
            .token)
    }

    async fn tvdb_search(
        &self,
        token: &str,
        kind: &str,
        title: &str,
        lease: &crate::gate::CredentialLease,
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
            .send_current(
                self.http
                    .get("https://api4.thetvdb.com/v4/search")
                    .bearer_auth(token)
                    .query(&[("query", title), ("type", media_type), ("limit", "10")]),
                lease.clone(),
            )
            .await
            .context("tvdb search")?
            .status_checked()?
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
        lease: &crate::gate::CredentialLease,
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
        let mut req = self.http.get(format!(
            "https://api.themoviedb.org/3/tv/{show_id}/season/{season}"
        ));
        if is_v4_token(key) {
            req = req.bearer_auth(key);
        } else {
            req = req.query(&[("api_key", key)]);
        }
        let s: Season = self
            .http
            .send_current(req, lease.clone())
            .await?
            .status_checked()?
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
    async fn tmdb_seasons(
        &self,
        key: &str,
        show_id: &str,
        lease: &crate::gate::CredentialLease,
    ) -> Result<Vec<(i64, i64)>> {
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
        if is_v4_token(key) {
            req = req.bearer_auth(key);
        } else {
            req = req.query(&[("api_key", key)]);
        }
        let s: Show = self
            .http
            .send_current(req, lease.clone())
            .await?
            .status_checked()?
            .json()
            .await?;
        Ok(s.seasons
            .into_iter()
            .filter(|s| s.season_number > 0)
            .map(|s| (s.season_number, s.episode_count))
            .collect())
    }

    async fn tvdb_episodes_english_cached(
        &self,
        token: &str,
        series_id: &str,
        order: &str,
        lease: &crate::gate::CredentialLease,
        store: Option<&kahawai_mediadb::Store>,
    ) -> Result<Vec<EpisodeData>> {
        let mut out = self
            .tvdb_episodes_cached(token, series_id, order, None, lease, store)
            .await?;
        let eng = match self
            .tvdb_episodes_cached(token, series_id, order, Some("eng"), lease, store)
            .await
        {
            Ok(episodes) => episodes,
            Err(error) => {
                lease.check()?;
                if store.is_some() {
                    return Err(error);
                }
                Vec::new()
            }
        };
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

    async fn tvdb_episodes_cached(
        &self,
        token: &str,
        series_id: &str,
        order: &str,
        lang: Option<&str>,
        lease: &crate::gate::CredentialLease,
        store: Option<&kahawai_mediadb::Store>,
    ) -> Result<Vec<EpisodeData>> {
        #[derive(Serialize, Deserialize)]
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
        #[derive(Serialize, Deserialize)]
        struct Data {
            #[serde(default)]
            episodes: Vec<Ep>,
        }
        #[derive(Serialize, Deserialize)]
        struct Resp {
            data: Data,
        }
        let mut out = Vec::new();
        for page in 0..20 {
            let question = format!(
                "episodes:{series_id}:{order}:{}:{page}",
                lang.unwrap_or("base")
            );
            let cached = if let Some(store) = store {
                store
                    .recent_cache_answer("tvdb-pages", &question, catalogue::now() - 7 * 24 * 3600)
                    .await?
            } else {
                None
            };
            let r: Resp = if let Some(cached) = cached {
                serde_json::from_str(&cached)?
            } else {
                let resp = self
                .http
                .send_current(
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
                    lease.clone(),
                )
                .await?;
                if resp.status() == reqwest::StatusCode::NOT_FOUND {
                    break;
                }
                let r: Resp = resp.status_checked()?.json().await?;
                if let Some(store) = store {
                    store
                        .put_cache_answer(
                            "tvdb-pages",
                            &question,
                            &serde_json::to_string(&r)?,
                            catalogue::now(),
                        )
                        .await?;
                }
                r
            };
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

    pub(crate) fn request_run(self: &Arc<Self>, registry: Arc<Registry>) {
        self.catalogue_wake.notify_waiters();
        tokio::spawn(async move {
            let _ = registry.catalogue().wake_enrichment(None).await;
        });
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
            .status_checked()?
            .json()
            .await?;
        let groups = resp["release-groups"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let want_title = fold(title);
        for g in &groups {
            let gtitle = g["title"].as_str().unwrap_or_default();
            if fold(gtitle) != want_title {
                continue;
            }
            let artist_id = exact_artist_credit_id(g["artist-credit"].as_array(), artist);
            let Some(artist_id) = artist_id else {
                continue;
            };
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
                artist_id,
                title: gtitle.to_string(),
                first_release_date: g["first-release-date"].as_str().map(str::to_string),
                genres,
            }));
        }
        Ok(None)
    }

    /// Resolve an Album Artist independently when none of its albums matched a
    /// release group. MusicBrainz exposes artist as a first-class searchable
    /// resource; using it here avoids making a compilation's album identity a
    /// prerequisite for its artist portrait.
    /// Source: https://musicbrainz.org/doc/MusicBrainz_API (read 2026-09-03).
    async fn musicbrainz_artist(&self, artist: &str) -> Result<Option<String>> {
        let query = format!("artist:\"{}\"", artist.replace('"', ""));
        let encoded: String = query
            .bytes()
            .flat_map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    vec![b as char]
                }
                _ => format!("%{b:02X}").chars().collect(),
            })
            .collect();
        let url = format!("https://musicbrainz.org/ws/2/artist?query={encoded}&fmt=json&limit=100");
        let response: serde_json::Value = self
            .http
            .send(self.http.get(url))
            .await?
            .status_checked()?
            .json()
            .await?;
        Ok(exact_artist_search_id(
            response["artists"].as_array(),
            artist,
        ))
    }

    async fn fanart_artist(
        &self,
        client_key: &str,
        artist_id: &str,
        lease: crate::gate::CredentialLease,
    ) -> Result<Option<FanartArtist>> {
        let response = self
            .http
            .send_current(self.fanart_artist_request(client_key, artist_id), lease)
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(response.status_checked()?.json().await?))
    }

    fn fanart_artist_request(&self, client_key: &str, artist_id: &str) -> reqwest::RequestBuilder {
        self.http
            // Do not let the provider forward its custom credential header to
            // another origin through a redirect.
            .get_no_redirect(format!(
                "https://webservice.fanart.tv/v3.2/music/{artist_id}"
            ))
            // Fanart.tv API v3.2: personal keys may be used alone in the
            // `client-key` header. Project keys use the distinct `api-key`
            // header. Prefer headers over the equivalent query parameters so
            // a transport error cannot print the secret as part of its URL.
            // Source: https://api.fanart.tv/ (read 2026-09-03).
            .header("client-key", client_key)
    }

    async fn theaudiodb_artist(
        &self,
        api_key: &str,
        premium: bool,
        artist_id: &str,
        artist_name: &str,
        lease: crate::gate::CredentialLease,
    ) -> Result<TheAudioDbAnswer> {
        let request = self.theaudiodb_artist_request(api_key, artist_id)?;
        let response = if premium {
            self.http
                .send_current_at_spacing(request, lease, crate::gate::THEAUDIODB_PREMIUM_SPACING)
                .await?
        } else {
            self.http.send_current(request, lease).await?
        };
        if matches!(
            response.status(),
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
        ) {
            return Ok(TheAudioDbAnswer::Missing);
        }
        let answer = response
            .status_checked()?
            .json::<TheAudioDbResponse>()
            .await?;
        Ok(exact_theaudiodb_artist(answer, artist_id, artist_name))
    }

    fn theaudiodb_artist_request(
        &self,
        api_key: &str,
        artist_id: &str,
    ) -> Result<reqwest::RequestBuilder> {
        // TheAudioDB v1 documents artist-mb.php as the MusicBrainz-ID lookup.
        // Source: https://www.theaudiodb.com/free_music_api
        // (read 2026-09-03).
        let mut url = reqwest::Url::parse("https://www.theaudiodb.com/")?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("TheAudioDB base URL cannot hold path segments"))?
            .extend(["api", "v1", "json"])
            .push(api_key)
            .push("artist-mb.php");
        url.query_pairs_mut().append_pair("i", artist_id);
        Ok(self.http.get_no_redirect(url))
    }

    async fn theaudiodb_credential(
        &self,
        registry: &Registry,
    ) -> Result<(String, bool, crate::gate::CredentialLease)> {
        let (mut fields, lease) = self.credential_snapshot(registry, THEAUDIODB).await?;
        let premium_key = fields
            .remove(THEAUDIODB_API_KEY)
            .filter(|key| !key.is_empty());
        let premium = premium_key.is_some();
        Ok((
            premium_key.unwrap_or_else(|| THEAUDIODB_FREE_API_KEY.to_string()),
            premium,
            lease,
        ))
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
        // Artist-art providers control these absolute URLs. Disable redirects
        // as well as validating the original host, otherwise a trusted-looking
        // asset URL could still bounce the hub into its own network.
        let request = if trusted_artist_image(&url) {
            self.http.get_no_redirect(&url)
        } else {
            self.http.get(&url)
        };
        let resp = self.http.send(request).await?;
        if matches!(
            resp.status(),
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
        ) {
            tracing::debug!(%url, "provider holds no poster here");
            return Ok(None);
        }
        let resp = resp.status_checked()?;
        Ok(Some(resp.bytes().await?.to_vec()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn fanart_prefers_a_square_thumb_then_likes_and_stable_id() {
        let mut answer: FanartArtist = serde_json::from_value(serde_json::json!({
            "name": "Björk",
            "mbid_id": "artist-id",
            "artistthumb": [
                {"id":"20", "url":"https://assets.fanart.tv/two.jpg", "likes":"7"},
                {"id":"10", "url":"https://assets.fanart.tv/one.jpg", "likes":"7"},
                {"id":"30", "url":"https://assets.fanart.tv/three.jpg", "likes":"2"}
            ],
            "artistbackground": [
                {"id":"01", "url":"https://assets.fanart.tv/background.jpg", "likes":"99"}
            ]
        }))
        .unwrap();
        let chosen = answer.best_image().unwrap();
        assert_eq!(chosen.id, "10");
        assert_eq!(chosen.url, "https://assets.fanart.tv/one.jpg");
    }

    #[test]
    fn fanart_background_is_used_when_no_square_thumb_exists() {
        let mut answer: FanartArtist = serde_json::from_value(serde_json::json!({
            "name": "Skinny Puppy",
            "mbid_id": "artist-id",
            "artistbackground": [
                {"id":"20", "url":"https://assets.fanart.tv/two.jpg", "likes":"7"},
                {"id":"10", "url":"https://assets.fanart.tv/one.jpg", "likes":"7"}
            ]
        }))
        .unwrap();
        let chosen = answer.best_image().unwrap();
        assert_eq!(chosen.id, "10");
    }

    #[test]
    fn artist_images_cannot_redirect_prefetch_to_another_origin() {
        assert!(trusted_artist_image(
            "https://assets.fanart.tv/fanart/music/portrait.jpg"
        ));
        assert!(trusted_artist_image(
            "https://r2.theaudiodb.com/images/media/artist/thumb/example.jpg"
        ));
        assert!(!trusted_artist_image(
            "http://assets.fanart.tv/fanart/music/portrait.jpg"
        ));
        assert!(!trusted_artist_image(
            "https://assets.fanart.tv.example/internal"
        ));
        assert!(!trusted_artist_image("http://127.0.0.1:8420/admin"));
    }

    #[test]
    fn fanart_personal_key_uses_the_documented_secret_header() {
        let enricher = Enricher::new(tempfile::tempdir().unwrap().keep());
        let (_, request) = enricher
            .fanart_artist_request("personal-secret", "artist-id")
            .build_split();
        let request = request.unwrap();

        assert_eq!(
            request
                .headers()
                .get("client-key")
                .and_then(|value| value.to_str().ok()),
            Some("personal-secret"),
        );
        assert!(request.url().query().is_none());
        assert!(!request.headers().contains_key("api-key"));
    }

    #[test]
    fn theaudiodb_lookup_uses_mbid_and_encodes_the_key_as_one_path_segment() {
        let enricher = Enricher::new(tempfile::tempdir().unwrap().keep());
        let (_, request) = enricher
            .theaudiodb_artist_request("premium/key", "artist-id")
            .unwrap()
            .build_split();
        let request = request.unwrap();

        assert_eq!(
            request.url().as_str(),
            "https://www.theaudiodb.com/api/v1/json/premium%2Fkey/artist-mb.php?i=artist-id"
        );
    }

    #[test]
    fn theaudiodb_answer_requires_exact_mbid_and_artist_name() {
        let response = serde_json::from_value(serde_json::json!({
            "artists": [{
                "idArtist": "42",
                "strArtist": "Björk",
                "strMusicBrainzID": "artist-id",
                "strArtistThumb": "https://r2.theaudiodb.com/portrait.jpg"
            }]
        }))
        .unwrap();
        let TheAudioDbAnswer::Artist(answer) =
            exact_theaudiodb_artist(response, "artist-id", "Björk")
        else {
            panic!("exact answer rejected");
        };
        assert_eq!(
            answer.best_image(),
            Some(("thumb", "https://r2.theaudiodb.com/portrait.jpg"))
        );

        let mismatch = serde_json::from_value(serde_json::json!({
            "artists": [{
                "idArtist": "42",
                "strArtist": "Bjork Tribute",
                "strMusicBrainzID": "artist-id"
            }]
        }))
        .unwrap();
        assert!(matches!(
            exact_theaudiodb_artist(mismatch, "artist-id", "Björk"),
            TheAudioDbAnswer::Ambiguous
        ));
    }

    #[test]
    fn provider_artist_credit_uses_the_catalogues_strict_identity() {
        let credits = serde_json::json!([
            {"name":"1", "artist":{"id":"number", "name":"1"}},
            {"name":"Pink", "artist":{"id":"plain", "name":"Pink"}},
            {"name":"Beyonce", "artist":{"id":"accent", "name":"Beyonce"}}
        ]);
        let credits = credits.as_array().unwrap();

        assert_eq!(exact_artist_credit_id(Some(credits), "One"), None);
        assert_eq!(exact_artist_credit_id(Some(credits), "P!nk"), None);
        assert_eq!(
            exact_artist_credit_id(Some(credits), "Beyoncé").as_deref(),
            Some("accent")
        );
    }

    #[test]
    fn direct_artist_search_requires_one_exact_catalogue_identity() {
        let unique = serde_json::json!([
            {"id":"wanted", "name":"Modeselektor"},
            {"id":"tribute", "name":"Modeselektor Tribute"}
        ]);
        assert_eq!(
            exact_artist_search_id(unique.as_array(), "MODESELEKTOR").as_deref(),
            Some("wanted")
        );

        let same_named = serde_json::json!([
            {"id":"one", "name":"Sefa"},
            {"id":"two", "name":"SEFA"}
        ]);
        assert_eq!(exact_artist_search_id(same_named.as_array(), "Sefa"), None);
    }

    /// A v4 token is a JWT and goes in a header; a v3 key goes in the query.
    /// The value is stored as typed, so the test for which one it is has to
    /// look past whitespace — otherwise a pasted leading space sends a
    /// long-lived token to TMDB in the URL.
    #[test]
    fn a_pasted_space_does_not_put_a_tmdb_token_in_the_url() {
        assert!(is_v4_token("eyJhbGciOiJIUzI1NiJ9.body.sig"));
        assert!(is_v4_token(" eyJhbGciOiJIUzI1NiJ9.body.sig"));
        assert!(is_v4_token("\n\teyJhbGciOiJIUzI1NiJ9.body.sig"));
        // A v3 key is 32 hex characters and has no business in a header.
        assert!(!is_v4_token("0123456789abcdef0123456789abcdef"));
        assert!(!is_v4_token(" 0123456789abcdef0123456789abcdef"));
    }

    /// A TVDB token lasts weeks, so a key rotated because it leaked would
    /// keep working through a token minted from the old one until the
    /// process restarted — unless the cache says which key bought it.
    #[test]
    fn a_token_is_only_reused_for_the_key_that_minted_it() {
        let old = TvdbCreds {
            key: "leaked".into(),
            pin: None,
        };
        let new = TvdbCreds {
            key: "rotated".into(),
            pin: None,
        };
        let token = std::sync::Arc::new(String::from("bought-with-the-leaked-key"));
        let slot = Some((tvdb_fingerprint(&old), token.clone()));

        assert_eq!(cached_token(&slot, &tvdb_fingerprint(&old)), Some(token));
        assert!(
            cached_token(&slot, &tvdb_fingerprint(&new)).is_none(),
            "the rotated key's requests would go out as the leaked one"
        );
        assert!(cached_token(&None, &tvdb_fingerprint(&old)).is_none());
    }

    #[test]
    fn credential_replacement_invalidates_only_work_holding_that_provider_credential() {
        let enricher = Enricher::new(tempfile::tempdir().unwrap().keep());
        let old_tmdb = enricher.provider_lease(TMDB);
        let tvdb = enricher.provider_lease(TVDB);

        enricher.revoke_provider(TMDB);

        assert!(
            old_tmdb.check().is_err(),
            "TMDB work holding the replaced credential remained live"
        );
        tvdb.check()
            .expect("replacing TMDB credentials invalidated TVDB work");

        let replacement = enricher.provider_lease(TMDB);
        replacement.check().expect("fresh TMDB credential");
        enricher.revoke_provider(TMDB);
        assert!(
            replacement.check().is_err(),
            "a second TMDB replacement revived older work"
        );
    }

    /// The subscriber pin selects the account behind the same key, and an
    /// empty pin is not the absence of one.
    #[test]
    fn the_tvdb_pin_is_part_of_the_credential() {
        let fingerprint = |key: &str, pin: Option<&str>| {
            tvdb_fingerprint(&TvdbCreds {
                key: key.into(),
                pin: pin.map(str::to_owned),
            })
        };
        assert_ne!(fingerprint("key", Some("1234")), fingerprint("key", None));
        assert_ne!(fingerprint("key", Some("")), fingerprint("key", None));
        assert_ne!(
            fingerprint("key", Some("1234")),
            fingerprint("key", Some("5"))
        );
        assert_ne!(fingerprint("ab", Some("c")), fingerprint("a", Some("bc")));
        assert_eq!(
            fingerprint("key", Some("1234")),
            fingerprint("key", Some("1234"))
        );
    }

    /// A lookup holds the AniDB mutex across a UDP round trip, so the account
    /// change flags the session instead of taking it — and the flag must
    /// survive until a run reads it, not be consumed by the admin request.
    #[tokio::test]
    async fn changing_the_anidb_account_drops_the_held_session() {
        let enricher = Enricher::new(tempfile::tempdir().unwrap().keep());
        let held = enricher.anidb.lock().await;

        // Would deadlock if it waited for the mutex; the test would hang
        // rather than fail, which is the failure this is here to prevent.
        enricher.anidb_forget();
        assert!(
            enricher.anidb_stale.load(Ordering::Acquire),
            "the next run would keep querying as the previous account"
        );
        drop(held);
    }

    /// TVDB's pin is optional — a subscriber has one, nobody else does — so
    /// its absence is not "unconfigured", and the key's absence is.
    #[tokio::test]
    async fn tvdb_reads_its_pair_from_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open_in_memory().await.unwrap();
        let credentials = std::sync::Arc::new(
            crate::secrets::Credentials::open(dir.path(), db.clone())
                .await
                .unwrap(),
        );
        let registry = Registry::new(
            db,
            Default::default(),
            kahawai_mediadb::Store::in_memory().await.unwrap(),
        )
        .with_credentials(credentials.clone());
        let stored = |fields: BTreeMap<&'static str, &'static str>| {
            let credentials = credentials.clone();
            async move {
                credentials
                    .set_provider(crate::secrets::HUB, TVDB, &fields)
                    .await
                    .unwrap();
            }
        };

        assert!(
            tvdb_creds(&registry).await.unwrap().is_none(),
            "nothing stored is not configured"
        );

        stored(BTreeMap::from([
            (TVDB_API_KEY, "a-key"),
            (TVDB_PIN, "a-pin"),
        ]))
        .await;
        let creds = tvdb_creds(&registry).await.unwrap().expect("configured");
        assert_eq!(creds.key, "a-key");
        assert_eq!(creds.pin.as_deref(), Some("a-pin"));

        // Saved again without one: the pair moves together, so the pin goes.
        stored(BTreeMap::from([(TVDB_API_KEY, "a-key")])).await;
        let creds = tvdb_creds(&registry)
            .await
            .unwrap()
            .expect("a key without a pin is still an account");
        assert_eq!(creds.pin, None);
    }

    /// Both halves of one error. 401 is the revoked-key case, the likeliest
    /// to leak; 404 is the one `is_http_404` has to keep recognising, and
    /// rewriting the error as a string would quietly lose it.
    #[tokio::test]
    async fn a_refused_call_keeps_its_status_and_loses_its_url() {
        for (code, line) in [
            (404, "404 Not Found"),
            (401, "401 Unauthorized"),
            (403, "403 Forbidden"),
            (500, "500 Internal Server Error"),
        ] {
            let l = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let addr = l.local_addr().unwrap();
            let reply = format!("HTTP/1.1 {line}\r\ncontent-length: 0\r\n\r\n");
            tokio::spawn(async move {
                while let Ok((mut s, _)) = l.accept().await {
                    use tokio::io::AsyncWriteExt;
                    let _ = s.write_all(reply.as_bytes()).await;
                }
            });

            let url = format!("http://{addr}/3/tv/1399?api_key=SECRET-OPERATOR-KEY");
            let e = reqwest::get(&url)
                .await
                .unwrap()
                .status_checked()
                .unwrap_err();
            let shown = format!("{e:#}");
            assert!(!shown.contains("SECRET-OPERATOR-KEY"), "{code}: {shown}");
            assert_eq!(
                is_http_status(&e, reqwest::StatusCode::NOT_FOUND),
                code == 404,
                "{code}: {shown}"
            );
            assert_eq!(
                [
                    reqwest::StatusCode::UNAUTHORIZED,
                    reqwest::StatusCode::FORBIDDEN,
                ]
                .into_iter()
                .any(|status| is_http_status(&e, status)),
                matches!(code, 401 | 403),
                "{code}: {shown}"
            );
            assert_eq!(is_http_transient(&e), code == 500, "{code}: {shown}");
        }
    }

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
    /// TheTVDB record by id — the bridged path, same rule as TMDB's:
    /// used only where a mapping already says which record this is.
    pub(crate) async fn tvdb_details(
        &self,
        token: &str,
        kind: &str,
        tvdb_id: i64,
        lease: &crate::gate::CredentialLease,
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
            .send_current(req, lease.clone())
            .await
            .context("tvdb details")?
            .status_checked()?
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
}
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
    out: &mut [ProviderCandidate],
    query: &str,
    year: Option<i64>,
    anime_first: bool,
) {
    let q = fold(query);
    let key = |candidate: &ProviderCandidate| {
        let title = candidate.title().map(fold).unwrap_or_default();
        let title_score = if title == q {
            0u8
        } else if title.starts_with(&q) {
            1
        } else if title.contains(&q) {
            2
        } else {
            3
        };
        let year_miss = match (
            year,
            candidate.release_date().and_then(|date| date.get(..4)),
        ) {
            (Some(want), Some(got)) => (got.parse::<i64>().ok() != Some(want)) as u8,
            _ => 0,
        };
        // At equal relevance, the provider that owns the item's
        // identity space leads: anilist for anime items, generic otherwise.
        let provider_rank = (candidate.is_anilist() != anime_first) as u8;
        // Rating descending, NaN-safe, as an integer key.
        let rating = -(candidate.vote_average().unwrap_or(0.0) * 10.0) as i64;
        (title_score, year_miss, provider_rank, rating)
    };
    out.sort_by_key(key);
}

#[cfg(test)]
mod candidate_rank_tests {
    use super::{AnilistCandidate, CatalogCandidate, ProviderCandidate, rank_candidates};

    fn candidate(
        provider: &'static str,
        title: &str,
        release_date: &str,
        vote_average: f64,
    ) -> ProviderCandidate {
        if provider == "anilist" {
            ProviderCandidate::Anilist(AnilistCandidate {
                id: 0,
                provider,
                title: Some(title.into()),
                overview: None,
                poster_path: None,
                poster_url: None,
                release_date: Some(release_date.into()),
                vote_average: Some(vote_average),
                format: None,
            })
        } else {
            ProviderCandidate::Catalog(CatalogCandidate {
                id: 0,
                provider,
                title: title.into(),
                original_title: None,
                overview: None,
                poster_path: None,
                vote_average: Some(vote_average),
                release_date: Some(release_date.into()),
                original_language: None,
                format: "Movie",
                poster_url: None,
            })
        }
    }

    #[test]
    fn typed_title_beats_provider_order() {
        let mut candidates = vec![
            candidate("tmdb", "One Day A Letter Arrives", "2015-01-01", 9.0),
            candidate("tmdb", "Kite Liberator", "2008-03-21", 6.0),
            candidate("tmdb", "Kite", "2014-10-09", 5.0),
            candidate("tmdb", "Kite", "1998-02-25", 7.0),
        ];
        rank_candidates(&mut candidates, "Kite", Some(1998), false);
        let titles: Vec<(&str, &str)> = candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.title().unwrap(),
                    &candidate.release_date().unwrap()[..4],
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
        let candidates = || {
            vec![
                candidate("tmdb", "Kite", "2014-10-09", 9.0),
                candidate("anilist", "Kite", "1998-02-25", 7.0),
            ]
        };
        let mut anime = candidates();
        rank_candidates(&mut anime, "Kite", None, true);
        assert!(
            anime[0].is_anilist(),
            "anime item: anilist leads its rating notwithstanding"
        );
        let mut generic = candidates();
        rank_candidates(&mut generic, "Kite", None, false);
        assert!(!generic[0].is_anilist(), "generic item: tmdb leads");
    }
}
