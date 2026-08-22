//! Per-provider request queues for outbound HTTP (HUB-7).
//!
//! Every metadata provider throttles and several ban outright. Pacing
//! that lives at the call site is pacing the next provider forgets —
//! which is how this deployment collected an AniDB ban — so every
//! provider request goes through [`Http::send`], which gives it
//!
//!   * a queue per PROVIDER HOST: one request in flight at a time,
//!     spaced by that provider's documented limit (an unknown host
//!     gets a deliberately slow default; a provider nobody thought
//!     about is the dangerous case),
//!   * a `429`/`503` honoured as silence for that provider alone —
//!     `Retry-After` when offered — rather than a retry loop that
//!     walks straight into the ban.
//!
//! The queues are process-wide because the limits are: providers count
//! requests per IP/key, not per struct.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::sync::Mutex;
use tokio::time::Instant;

/// MusicBrainz rejects anonymous/library user agents outright and
/// requires "application name/version ( contact )" — every provider
/// gets the same honest identification.
const UA: &str = concat!(
    "kahawai/",
    env!("CARGO_PKG_VERSION"),
    " ( https://github.com/iksteen/kahawai )"
);

/// Fallback when a 429/503 carries no usable `Retry-After`.
const DEFAULT_PENALTY: Duration = Duration::from_secs(60);
/// Never park a provider longer than this, whatever it asks for.
const MAX_PENALTY: Duration = Duration::from_secs(3600);

/// Minimum gap between two requests to one provider host, from each
/// provider's own documentation. Sources are named because these
/// numbers move (AniList's has been "temporary" for years) and the
/// next person needs to know what to re-check.
fn spacing(host: &str) -> Duration {
    let ms = match host {
        // MusicBrainz: "on average 1 request per second" per source
        // IP, enforced by rejecting everything above it with 503.
        // Cover Art Archive is the same project, same courtesy.
        "musicbrainz.org" | "coverartarchive.org" => 1100,
        // AniList: nominally 90/min, but the API has been in a
        // "degraded state ... limited to 30 requests per minute" for
        // years. Pace for the limit that actually 429s.
        "graphql.anilist.co" => 2100,
        // AniDB HTTP API: "no more than one page every two seconds",
        // and a ban only decays after ~24 h of silence.
        "anidb.net" | "api.anidb.net" => 2200,
        // OpenSubtitles.com: 1 request per second on the standard
        // tier; /login is capped harder still, and 429s there are
        // common enough that their docs call out retry-with-pause.
        "api.opensubtitles.com" => 1100,
        // TMDB retired its published limit (was 40/10 s) and now asks
        // only that clients stay near 40/s and respect a 429.
        "api.themoviedb.org" => 60,
        // TheTVDB publishes no number; be polite and heed the 429.
        "api4.thetvdb.com" => 200,
        // Plain CDNs and static dumps: no per-client limit.
        "image.tmdb.org" | "artworks.thetvdb.com" | "raw.githubusercontent.com" => 0,
        // An unpaced host is a provider nobody has read the terms of.
        _ => 500,
    };
    Duration::from_millis(ms)
}

/// The per-host queues. Process-wide: see the module note.
fn queues() -> &'static Mutex<HashMap<String, Arc<Mutex<Instant>>>> {
    static Q: OnceLock<Mutex<HashMap<String, Arc<Mutex<Instant>>>>> = OnceLock::new();
    Q.get_or_init(Default::default)
}

/// A paced HTTP client. Build requests with [`Http::get`]/[`Http::post`]
/// and send them with [`Http::send`] — there is deliberately no unpaced
/// way out.
pub struct Http {
    client: reqwest::Client,
}

impl Http {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder().user_agent(UA).build()?,
        })
    }

    pub fn get(&self, url: impl reqwest::IntoUrl) -> reqwest::RequestBuilder {
        self.client.get(url)
    }

    pub fn post(&self, url: impl reqwest::IntoUrl) -> reqwest::RequestBuilder {
        self.client.post(url)
    }

    pub fn request(
        &self,
        method: reqwest::Method,
        url: impl reqwest::IntoUrl,
    ) -> reqwest::RequestBuilder {
        self.client.request(method, url)
    }

    /// Send `req` through its provider's queue. A rate-limit reply is
    /// an error for this caller — skip the item, it comes round again
    /// next run — and silence for that provider until it says it will
    /// listen again.
    pub async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let (client, req) = req.build_split();
        let req = req.context("building provider request")?;
        let host = req
            .url()
            .host_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let queue = {
            let mut queues = queues().lock().await;
            queues
                .entry(host.clone())
                .or_insert_with(|| Arc::new(Mutex::new(Instant::now())))
                .clone()
        };
        // Held across the send: one request in flight per provider is
        // what a rate limit actually counts.
        // ponytail: this serializes bulk artwork off a CDN too; give
        // the zero-spacing hosts a bypass if that ever measures slow.
        let mut next = queue.lock().await;
        tokio::time::sleep_until(*next).await;
        let resp = client.execute(req).await;
        *next = Instant::now() + spacing(&host);
        // A timeout or refused connection carries the URL it was reaching for,
        // and `providers::reschedule` writes that message into the database.
        let resp = resp
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("{host} request failed"))?;
        if matches!(resp.status().as_u16(), 429 | 503) {
            // MusicBrainz answers 503 with `Retry-After: 0` — "just slow
            // down" — which read literally parks for nothing and the next
            // paced request eats another 503 (39 in a row, 2026-07-28).
            // A zero header is no usable header.
            let penalty = retry_after(&resp)
                .filter(|d| !d.is_zero())
                .unwrap_or(DEFAULT_PENALTY)
                .min(MAX_PENALTY);
            *next = Instant::now() + penalty;
            tracing::warn!(host, status = %resp.status(), secs = penalty.as_secs(),
                "provider rate-limited us; going quiet");
            bail!(
                "{host} rate-limited us ({}); quiet for {}s",
                resp.status(),
                penalty.as_secs()
            );
        }
        Ok(resp)
    }
}

/// `Retry-After` in seconds. The HTTP-date form falls back to the
/// default penalty — no provider we talk to uses it.
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let v = resp.headers().get(reqwest::header::RETRY_AFTER)?;
    v.to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// Answers every connection with one canned response. `ip` picks
    /// the loopback address, and so the queue this server lands in:
    /// queues are keyed by host, because that is the unit a provider
    /// counts (api.anidb.net:9000 and :9001 are one budget).
    async fn server(ip: &str, response: &'static str) -> String {
        let l = tokio::net::TcpListener::bind((ip, 0)).await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let _ = s.write_all(response.as_bytes()).await;
                    let _ = s.flush().await;
                });
            }
        });
        format!("http://{addr}/")
    }

    async fn parked_for(url: &str) -> Duration {
        let host = reqwest::Url::parse(url)
            .unwrap()
            .host_str()
            .unwrap()
            .to_string();
        let q = queues().lock().await.get(&host).unwrap().clone();
        let at = *q.lock().await;
        at.saturating_duration_since(Instant::now())
    }

    /// A refused connection must not carry the URL: the key rides in it, and
    /// `providers::reschedule` puts this message in the database.
    #[tokio::test]
    async fn a_transport_failure_does_not_carry_the_url() {
        // Bound then dropped, so the port answers nothing.
        let dead = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let http = Http::new().unwrap();
        let url = format!("http://{dead}/3/tv/1399?api_key=SECRET-OPERATOR-KEY");
        let e = http.send(http.get(&url)).await.unwrap_err();
        let shown = format!("{e:#}");
        assert!(!shown.contains("SECRET-OPERATOR-KEY"), "{shown}");
    }

    #[tokio::test]
    async fn paces_and_backs_off() {
        let http = Http::new().unwrap();
        let ok = server(
            "127.0.0.1",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi",
        )
        .await;
        let start = Instant::now();
        for _ in 0..2 {
            http.send(http.get(&ok)).await.unwrap();
        }
        // The second request waits out the unknown-host default.
        assert!(
            start.elapsed() >= Duration::from_millis(450),
            "requests were not paced"
        );

        // A 429 is an error, and parks that provider for what it asked.
        let busy = server(
            "127.0.0.2",
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 90\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert!(http.send(http.get(&busy)).await.is_err());
        assert!(
            parked_for(&busy).await > Duration::from_secs(80),
            "Retry-After ignored"
        );
        // ...and only that provider: the healthy one is free again.
        assert!(parked_for(&ok).await < Duration::from_secs(1));

        // MusicBrainz's `Retry-After: 0` must fall back to the default
        // penalty, not park for nothing and eat the next 503 too.
        let shrug = server(
            "127.0.0.3",
            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert!(http.send(http.get(&shrug)).await.is_err());
        assert!(
            parked_for(&shrug).await > Duration::from_secs(30),
            "zero Retry-After must still park"
        );
    }
}
