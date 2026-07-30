//! AniDB UDP client (HUB-29/30 gold path): FILE lookup by size + ED2K
//! gives the exact anime/episode/group for a file — above every
//! name-based heuristic.
//!
//! AniDB is aggressively rate-limited and ban-happy, so this client is
//! built around never asking twice: ≥2 s between packets and callers
//! cache every answer. Two disciplines matter more than the rest, and
//! both are enforced here rather than left to callers:
//!
//! * **One session, reused across runs AND restarts** — a fresh AUTH per
//!   enrichment run got us throttle-banned twice in one evening (37
//!   logins in a day). AniDB identifies a session by client IP *and
//!   port*, so the local UDP port is persisted alongside the key and
//!   rebound on startup; the session then survives a restart (until
//!   AniDB expires it for inactivity, ~35 min).
//! * **A 555 stops everything** — AniDB bans have no fixed duration:
//!   they decay after ~24 h ONLY if the client stops calling, and every
//!   attempt while banned extends them. So a ban is recorded on disk
//!   and suppresses all contact until it lapses.
//!
//! The client identity is the registered `kahawai` app; the account is
//! the admin's (settings). Optionally encrypts the session with the
//! profile's UDP API key (AES-128-ECB per spec).

use std::time::Duration;

use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use anyhow::{bail, Context, Result};
use md5::{Digest, Md5};

const SERVER: &str = "api.anidb.net:9000";
const CLIENT: &str = "kahawai";
const CLIENT_VER: u32 = 1;
const PROTOVER: u32 = 3;
/// AniDB's flood rule has TWO halves and both are mandatory:
///
///   short term — never more than 0.5 packets/s (one per 2 s);
///   long term  — never more than one packet per 4 s "over an extended
///                amount of time", with enforcement starting after the
///                first 5 packets.
///
/// A flat 2 s spacing satisfies only the first half, and a bulk
/// identification run then sits at double the sustained rate for
/// however long it takes — which is precisely how this deployment
/// earned its bans. So: a 5-packet burst allowance (the server's own
/// grace) draining into one packet per 4 s.
const PACKET_SPACING: Duration = Duration::from_millis(2200);
const SUSTAINED_SPACING: Duration = Duration::from_millis(4200);
const BURST_PACKETS: f64 = 5.0;
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to stay silent after a 555. AniDB documents no duration
/// (bans decay only while the client is quiet), so this is the
/// conservative reading of "come back tomorrow".
const BAN_COOLDOWN: Duration = Duration::from_secs(24 * 3600);

pub const USER_SETTING: &str = "anidb.username";
pub const PASS_SETTING: &str = "anidb.password";
pub const APIKEY_SETTING: &str = "anidb.udp_api_key";

/// An exact file identification.
#[derive(Debug, Clone)]
pub struct FileHit {
    pub fid: u64,
    pub aid: u32,
    pub eid: u32,
    pub gid: u32,
    /// AniDB episode number: "1", "01", "S1" (special), "C1", "T1", …
    pub epno: String,
    pub group_name: String,
}

/// What survives a restart: the session key, the local UDP port it is
/// bound to (AniDB keys sessions by IP+port, so the same port must be
/// rebound for the key to mean anything), and when a ban lapses.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct State {
    #[serde(default)]
    session: String,
    /// Local UDP port the session belongs to; 0 = none stored.
    #[serde(default)]
    port: u16,
    /// Unix seconds; contact is refused until then.
    #[serde(default)]
    banned_until: i64,
}

fn state_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("anime").join("anidb-session.json")
}

fn load_state(data_dir: &std::path::Path) -> State {
    std::fs::read(state_path(data_dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_state(data_dir: &std::path::Path, st: &State) {
    let path = state_path(data_dir);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(bytes) = serde_json::to_vec(st) {
        let _ = std::fs::write(path, bytes);
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Seconds left on a recorded ban, if any — checked BEFORE opening a
/// socket so a banned hub never touches AniDB at all.
pub fn ban_remaining(data_dir: &std::path::Path) -> Option<i64> {
    let left = load_state(data_dir).banned_until - now_unix();
    (left > 0).then_some(left)
}

pub struct Anidb {
    socket: tokio::net::UdpSocket,
    data_dir: std::path::PathBuf,
    session: String,
    /// AES-128 key when the session is encrypted (API key configured).
    cipher: Option<aes::Aes128>,
    last_send: Option<tokio::time::Instant>,
    bucket: Bucket,
    tag_seq: u32,
}

/// The long-term half of the flood rule: packets refilling at one per
/// [`SUSTAINED_SPACING`], never banking more than [`BURST_PACKETS`].
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    at: Option<tokio::time::Instant>,
}

impl Bucket {
    fn new() -> Self {
        Self { tokens: BURST_PACKETS, at: None }
    }

    /// Spend one packet, returning how long the caller must wait first.
    fn take(&mut self, now: tokio::time::Instant) -> Duration {
        let per = SUSTAINED_SPACING.as_secs_f64();
        if let Some(t) = self.at {
            self.tokens = (self.tokens + now.saturating_duration_since(t).as_secs_f64() / per)
                .min(BURST_PACKETS);
        }
        let wait = if self.tokens < 1.0 {
            Duration::from_secs_f64((1.0 - self.tokens) * per)
        } else {
            Duration::ZERO
        };
        self.tokens = self.tokens.max(1.0) - 1.0;
        // Bill the wait forward, so it isn't also counted as refill.
        self.at = Some(now + wait);
        wait
    }
}

/// How long to hold before the next packet: BOTH halves of AniDB's flood
/// rule, whichever binds. `bucket` is the sustained rate, `last_send` the
/// short-term gap (widened on a retry re-send).
///
/// Separated from the sleeping on purpose — the spacing is the thing that
/// earned us a ban, and it cannot be measured from live traffic: reply
/// timestamps float with round-trip time, so a correct 2.2 s send gap can
/// read as 2.16 s between answers.
fn pace_delay(
    bucket: &mut Bucket,
    last_send: Option<tokio::time::Instant>,
    retry: u32,
    now: tokio::time::Instant,
) -> Duration {
    // Sleeping for the bucket first and re-measuring afterwards, as this
    // used to, is the same as taking the larger of the two.
    let sustained = bucket.take(now);
    let short = last_send.map_or(Duration::ZERO, |t| {
        (PACKET_SPACING * (retry + 1)).saturating_sub(now.saturating_duration_since(t))
    });
    sustained.max(short)
}

impl Anidb {
    /// Login. `api_key` (profile "UDP API key") upgrades to an
    /// encrypted session first.
    pub async fn login(
        data_dir: &std::path::Path,
        user: &str,
        pass: &str,
        api_key: Option<&str>,
    ) -> Result<Self> {
        // Refuse to speak at all while a ban is on record: contact is
        // what keeps a ban alive.
        if let Some(left) = ban_remaining(data_dir) {
            bail!(
                "anidb banned us; staying silent for another {} h (contact would extend it)",
                (left + 3599) / 3600
            );
        }
        // Rebind the port the stored session belongs to. If it is taken
        // (another instance, or the OS still holding it), fall back to
        // an ephemeral port — the session is then unusable and we
        // authenticate as normal.
        let st = load_state(data_dir);
        let socket = match st.port {
            0 => tokio::net::UdpSocket::bind("0.0.0.0:0").await?,
            p => match tokio::net::UdpSocket::bind(("0.0.0.0", p)).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(port = p, error = %e, "stored anidb port unavailable");
                    tokio::net::UdpSocket::bind("0.0.0.0:0").await?
                }
            },
        };
        let bound_port = socket.local_addr().map(|a| a.port()).unwrap_or(0);
        socket.connect(SERVER).await.context("resolving api.anidb.net")?;
        let mut client = Self {
            socket,
            data_dir: data_dir.to_path_buf(),
            session: String::new(),
            cipher: None,
            last_send: None,
            bucket: Bucket::new(),
            tag_seq: 0,
        };


        if let Some(key) = api_key.filter(|k| !k.is_empty()) {
            let (code, rest) =
                client.command(&format!("ENCRYPT user={user}&type=1")).await?;
            match code {
                209 => {}
                309 => bail!(
                    "AniDB profile has no UDP API key defined — set one under \
                     Settings → Account on anidb.net, or clear the key here"
                ),
                555 => {
                    client.record_ban(&rest);
                    bail!("anidb: BANNED — staying silent for 24 h: {rest}")
                }
                other => bail!("ENCRYPT refused: {other} {rest}"),
            }
            let salt = rest.split_whitespace().next().unwrap_or_default();
            let digest = Md5::digest(format!("{key}{salt}").as_bytes());
            client.cipher = Some(aes::Aes128::new_from_slice(&digest).unwrap());
        }

        // Resume before authenticating — but only if we got the same
        // port back, since the key is meaningless from another one.
        // Must come AFTER ENCRYPT: an encrypted session can only be
        // probed over an encrypted channel.
        if !st.session.is_empty() && st.port != 0 && st.port == bound_port {
            client.session = st.session;
            match client.command(&format!("UPTIME s={}", client.session)).await {
                Ok((208, _)) => {
                    tracing::info!(port = bound_port, "anidb session resumed (no re-auth)");
                    return Ok(client);
                }
                Ok((555, rest)) => {
                    client.record_ban(&rest);
                    bail!("anidb: BANNED — staying silent for 24 h: {rest}");
                }
                // 501/506 = expired (35 min idle) or otherwise stale.
                Ok((code, _)) => {
                    tracing::debug!(code, "stored anidb session stale; authenticating");
                    client.session.clear();
                }
                Err(e) => {
                    tracing::debug!(error = format!("{e:#}"), "session probe failed; authenticating");
                    client.session.clear();
                }
            }
        }

        let (code, rest) = client
            .command(&format!(
                "AUTH user={user}&pass={pass}&protover={PROTOVER}&client={CLIENT}&clientver={CLIENT_VER}&enc=UTF8"
            ))
            .await?;
        match code {
            200 | 201 => {
                client.session =
                    rest.split_whitespace().next().unwrap_or_default().to_string();
                anyhow::ensure!(!client.session.is_empty(), "no session key in AUTH reply");
                save_state(
                    data_dir,
                    &State {
                        session: client.session.clone(),
                        port: bound_port,
                        banned_until: 0,
                    },
                );
                if code == 201 {
                    tracing::info!("anidb: a newer client version is available");
                }
                tracing::info!("anidb session established");
                Ok(client)
            }
            500 => bail!("anidb login failed (check username/password)"),
            503 | 504 => bail!("anidb rejected the client registration: {code} {rest}"),
            555 => {
                client.record_ban(&rest);
                bail!("anidb: BANNED — staying silent for 24 h: {rest}")
            }
            other => bail!("anidb AUTH unexpected: {other} {rest}"),
        }
    }

    /// Exact lookup. Ok(None) = AniDB doesn't know this file (320).
    pub async fn file_by_ed2k(&mut self, size: u64, ed2k_hex: &str) -> Result<Option<FileHit>> {
        // fmask: aid|eid|gid; amask: epno|group name.
        let (code, rest) = self
            .command(&format!(
                "FILE size={size}&ed2k={ed2k_hex}&fmask=70000000&amask=00008080&s={}",
                self.session
            ))
            .await?;
        match code {
            220 => {
                let fields: Vec<&str> = rest
                    .lines()
                    .nth(1)
                    .context("FILE reply missing data line")?
                    .split('|')
                    .collect();
                // fid|aid|eid|gid|epno|group name
                anyhow::ensure!(fields.len() >= 6, "unexpected FILE fields: {fields:?}");
                Ok(Some(FileHit {
                    fid: fields[0].parse().unwrap_or(0),
                    aid: fields[1].parse().context("bad aid")?,
                    eid: fields[2].parse().unwrap_or(0),
                    gid: fields[3].parse().unwrap_or(0),
                    epno: fields[4].to_string(),
                    group_name: fields[5].to_string(),
                }))
            }
            320 => Ok(None),
            501 | 506 => {
                let st = load_state(&self.data_dir);
                save_state(&self.data_dir, &State { session: String::new(), ..st });
                bail!("anidb session lost: {code}")
            }
            555 => {
                self.record_ban(&rest);
                bail!("anidb: BANNED — staying silent for 24 h: {rest}")
            }
            other => bail!("anidb FILE unexpected: {other} {rest}"),
        }
    }

    /// Remember a ban and drop the session, so nothing touches AniDB
    /// until it lapses (every attempt would extend it).
    fn record_ban(&mut self, detail: &str) {
        tracing::error!(detail, "anidb BANNED — suppressing all contact for 24 h");
        self.session.clear();
        let port = load_state(&self.data_dir).port;
        save_state(
            &self.data_dir,
            &State {
                session: String::new(),
                port,
                banned_until: now_unix() + BAN_COOLDOWN.as_secs() as i64,
            },
        );
    }

    /// Ends the run WITHOUT logging out: the session key stays valid
    /// server-side and the next run resumes it. LOGOUT would force the
    /// next run to AUTH again — the very pattern that got us banned.
    pub async fn finish(self) {}

    /// Wait until both halves of the flood rule allow another packet.
    /// `retry` widens the short-term gap for a timeout re-send.
    async fn pace(&mut self, retry: u32) {
        let owed = pace_delay(&mut self.bucket, self.last_send, retry, tokio::time::Instant::now());
        if !owed.is_zero() {
            tokio::time::sleep(owed).await;
        }
        self.last_send = Some(tokio::time::Instant::now());
    }

    /// One request/response with spacing, tagging, optional encryption,
    /// and a single timeout retry.
    async fn command(&mut self, cmd: &str) -> Result<(u16, String)> {
        self.tag_seq += 1;
        let tag = format!("k{}", self.tag_seq);
        let full = format!("{cmd}&tag={tag}");
        let payload = match &self.cipher {
            Some(c) => aes_ecb(c, full.as_bytes(), true)?,
            None => full.clone().into_bytes(),
        };

        for attempt in 0..2 {
            self.pace(attempt).await;
            self.socket.send(&payload).await?;

            let mut buf = vec![0u8; 4096];
            match tokio::time::timeout(REPLY_TIMEOUT, self.socket.recv(&mut buf)).await {
                Ok(Ok(n)) => {
                    // The server answers in PLAINTEXT when it could not
                    // decrypt us (wrong UDP API key) and for some raw
                    // errors — fall back so the real message surfaces.
                    let raw = match &self.cipher {
                        Some(c) => aes_ecb(c, &buf[..n], false)
                            .unwrap_or_else(|_| buf[..n].to_vec()),
                        None => buf[..n].to_vec(),
                    };
                    let text = String::from_utf8_lossy(&raw).to_string();
                    // "tag code message…"; untagged replies are server-
                    // level errors (can't decrypt, banned, …).
                    let rest = match text.strip_prefix(&format!("{tag} ")) {
                        Some(r) => r,
                        None if text.chars().take(3).all(|c| c.is_ascii_digit()) => &text,
                        None if attempt == 0 => continue,
                        None => bail!("anidb reply tag mismatch: {text}"),
                    };
                    let (code_s, msg) = rest.split_once(' ').unwrap_or((rest, ""));
                    let code: u16 =
                        code_s.trim().parse().with_context(|| format!("unparseable reply: {text}"))?;
                    if code == 598 && self.cipher.is_some() {
                        bail!(
                            "AniDB could not decrypt our packets — the UDP API key \
                             does not match the one in your AniDB profile"
                        );
                    }
                    return Ok((code, msg.to_string()));
                }
                _ if attempt == 0 => continue, // one retry, doubled spacing
                _ => bail!("anidb did not reply"),
            }
        }
        unreachable!()
    }
}

/// AES-128-ECB with PKCS#7, as the UDP API's ENCRYPT specifies.
fn aes_ecb(cipher: &aes::Aes128, data: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    if encrypt {
        let pad = 16 - (data.len() % 16);
        let mut buf = data.to_vec();
        buf.extend(std::iter::repeat_n(pad as u8, pad));
        for chunk in buf.chunks_exact_mut(16) {
            cipher.encrypt_block(chunk.try_into().expect("16-byte block"));
        }
        Ok(buf)
    } else {
        anyhow::ensure!(data.len().is_multiple_of(16) && !data.is_empty(), "bad ciphertext length");
        let mut buf = data.to_vec();
        for chunk in buf.chunks_exact_mut(16) {
            cipher.decrypt_block(chunk.try_into().expect("16-byte block"));
        }
        let pad = *buf.last().unwrap() as usize;
        anyhow::ensure!((1..=16).contains(&pad) && pad <= buf.len(), "bad padding");
        buf.truncate(buf.len() - pad);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every gap between SENDS, over a run long enough to drain the
    /// burst. This is what live traffic cannot tell us: replies float
    /// with round-trip time, so the 2.16 s observed between two answers
    /// says nothing about whether the packets left 2.2 s apart.
    #[test]
    fn every_send_gap_honours_both_halves_of_the_flood_rule() {
        let mut bucket = Bucket::new();
        let mut last: Option<tokio::time::Instant> = None;
        let mut now = tokio::time::Instant::now();
        let start = now;
        let mut gaps = Vec::new();
        for _ in 0..50 {
            now += pace_delay(&mut bucket, last, 0, now); // the caller sleeps
            if let Some(prev) = last {
                gaps.push((now - prev).as_secs_f64());
            }
            last = Some(now); // and sends
        }
        let worst = gaps.iter().cloned().fold(f64::INFINITY, f64::min);
        assert!(
            worst >= PACKET_SPACING.as_secs_f64() - 1e-9,
            "two packets left {worst:.3}s apart, inside the {:?} floor",
            PACKET_SPACING
        );
        // ...and the run as a whole stays under the sustained rate.
        let elapsed = (now - start).as_secs_f64();
        let floor = (50.0 - BURST_PACKETS) * SUSTAINED_SPACING.as_secs_f64();
        assert!(elapsed >= floor - 0.01, "50 packets in {elapsed:.1}s, floor {floor:.1}s");
    }

    /// A timed-out packet is re-sent no sooner than a widened gap — the
    /// re-send is the case where a naive client hammers hardest.
    #[test]
    fn a_retry_widens_the_short_term_gap() {
        let mut bucket = Bucket::new();
        let now = tokio::time::Instant::now();
        // Burst spent, so only the short-term half can bind.
        for _ in 0..BURST_PACKETS as usize {
            bucket.take(now);
        }
        let just_sent = Some(now);
        for retry in 0..3u32 {
            let mut b = Bucket::new();
            let owed = pace_delay(&mut b, just_sent, retry, now);
            assert_eq!(
                owed,
                PACKET_SPACING * (retry + 1),
                "retry {retry} should wait {} x the base gap",
                retry + 1
            );
        }
    }

    /// What a BULK run leans on, which is where this went wrong: the
    /// burst and the refill cap are covered by
    /// [`bucket_allows_a_burst_then_the_sustained_rate`], but neither
    /// says anything about a long pass. The ban came from exactly that —
    /// a run that honoured the short gap between packets and still sat
    /// above the sustained rate for its whole duration.
    ///
    /// Arithmetic, so it is proved here rather than by sending traffic:
    /// a live run long enough to exhaust the burst allowance would be
    /// the very pattern being guarded against.
    #[test]
    fn a_long_run_can_never_outpace_the_sustained_rate() {
        let base = tokio::time::Instant::now();
        let mut b = Bucket::new();
        let mut now = base;
        const N: usize = 200;
        for _ in 0..N {
            now += b.take(now); // the caller sleeps for what it is told
        }
        let elapsed = (now - base).as_secs_f64();
        let floor = (N as f64 - BURST_PACKETS) * SUSTAINED_SPACING.as_secs_f64();
        assert!(
            elapsed >= floor - 0.01,
            "{N} packets in {elapsed:.1}s, under the {floor:.1}s the rule requires"
        );
        // And no slower than it requires — over-throttling a 900-file
        // pass by one interval each is an hour nobody asked for.
        assert!(
            elapsed <= floor + SUSTAINED_SPACING.as_secs_f64(),
            "over-throttled: {elapsed:.1}s against a {floor:.1}s floor"
        );
    }

    /// The ban gate must refuse BEFORE any socket work — contact is
    /// what extends a ban, so this is the whole point of the feature.
    #[tokio::test]
    async fn a_recorded_ban_blocks_login_without_contact() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ban_remaining(dir.path()).is_none(), "clean state is not banned");

        save_state(dir.path(), &State { banned_until: now_unix() + 3600, ..Default::default() });
        let left = ban_remaining(dir.path()).expect("ban recorded");
        assert!(left > 3500 && left <= 3600, "{left}");

        // Unroutable server would hang/fail if we actually spoke; the
        // gate returns immediately instead.
        let err = match Anidb::login(dir.path(), "u", "p", None).await {
            Ok(_) => panic!("login must be refused while banned"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("staying silent"), "{err}");
    }

    #[test]
    fn a_lapsed_ban_is_no_longer_enforced() {
        let dir = tempfile::tempdir().unwrap();
        save_state(dir.path(), &State { banned_until: now_unix() - 1, ..Default::default() });
        assert!(ban_remaining(dir.path()).is_none());
    }

    /// A session is only resumable from the port it was opened on
    /// (AniDB keys sessions by IP+port), so both travel together.
    #[test]
    fn session_and_port_persist_together() {
        let dir = tempfile::tempdir().unwrap();
        save_state(
            dir.path(),
            &State { session: "key".into(), port: 45678, banned_until: 0 },
        );
        let st = load_state(dir.path());
        assert_eq!((st.session.as_str(), st.port), ("key", 45678));
    }

    /// The long-term rule is the one a bulk run breaks: five packets
    /// go out on the burst allowance, and after that the bucket holds
    /// the run to one packet per four seconds.
    #[test]
    fn bucket_allows_a_burst_then_the_sustained_rate() {
        let t0 = tokio::time::Instant::now();
        let mut b = Bucket::new();
        for i in 0..BURST_PACKETS as u32 {
            assert_eq!(b.take(t0), Duration::ZERO, "burst packet {i} was delayed");
        }
        // Sixth packet in the same instant has to wait out a refill.
        let wait = b.take(t0);
        assert!(
            wait >= SUSTAINED_SPACING - Duration::from_millis(1) && wait <= SUSTAINED_SPACING,
            "expected a ~{SUSTAINED_SPACING:?} wait, got {wait:?}"
        );
        // Idling banks packets again, but never more than the burst.
        let mut b = Bucket::new();
        b.take(t0);
        assert_eq!(b.take(t0 + SUSTAINED_SPACING), Duration::ZERO);
        let mut b = Bucket::new();
        for _ in 0..BURST_PACKETS as u32 {
            b.take(t0);
        }
        let idled = t0 + SUSTAINED_SPACING * 100;
        for _ in 0..BURST_PACKETS as u32 {
            assert_eq!(b.take(idled), Duration::ZERO, "idling should refill the burst");
        }
        assert!(b.take(idled) > Duration::ZERO, "but never beyond it");
    }

    #[test]
    fn aes_ecb_roundtrip() {
        let key = Md5::digest(b"apikeysalt");
        let cipher = aes::Aes128::new_from_slice(&key).unwrap();
        for msg in ["", "x", "exactly sixteen!", "AUTH user=a&pass=b&protover=3"] {
            let enc = aes_ecb(&cipher, msg.as_bytes(), true).unwrap();
            assert_eq!(enc.len() % 16, 0);
            let dec = aes_ecb(&cipher, &enc, false).unwrap();
            assert_eq!(dec, msg.as_bytes());
        }
    }
}
