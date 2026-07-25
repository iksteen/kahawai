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
//!   and suppresses all contact until it lapses. The client identity is the registered `kahawai`
//! app; the account is the admin's (settings). Optionally encrypts the
//! session with the profile's UDP API key (AES-128-ECB per spec).

use std::time::Duration;

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use anyhow::{bail, Context, Result};
use md5::{Digest, Md5};

const SERVER: &str = "api.anidb.net:9000";
const CLIENT: &str = "kahawai";
const CLIENT_VER: u32 = 1;
const PROTOVER: u32 = 3;
/// AniDB demands ≥2 s between packets; be a little politer.
const PACKET_SPACING: Duration = Duration::from_millis(2200);
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
    tag_seq: u32,
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
            if let Some(t) = self.last_send {
                let since = t.elapsed();
                let wait = PACKET_SPACING * (attempt + 1);
                if since < wait {
                    tokio::time::sleep(wait - since).await;
                }
            }
            self.last_send = Some(tokio::time::Instant::now());
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
    use aes::cipher::generic_array::GenericArray;
    if encrypt {
        let pad = 16 - (data.len() % 16);
        let mut buf = data.to_vec();
        buf.extend(std::iter::repeat_n(pad as u8, pad));
        for chunk in buf.chunks_mut(16) {
            cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
        }
        Ok(buf)
    } else {
        anyhow::ensure!(data.len() % 16 == 0 && !data.is_empty(), "bad ciphertext length");
        let mut buf = data.to_vec();
        for chunk in buf.chunks_mut(16) {
            cipher.decrypt_block(GenericArray::from_mut_slice(chunk));
        }
        let pad = *buf.last().unwrap() as usize;
        anyhow::ensure!(pad >= 1 && pad <= 16 && pad <= buf.len(), "bad padding");
        buf.truncate(buf.len() - pad);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
