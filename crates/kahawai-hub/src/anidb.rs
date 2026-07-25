//! AniDB UDP client (HUB-29/30 gold path): FILE lookup by size + ED2K
//! gives the exact anime/episode/group for a file — above every
//! name-based heuristic.
//!
//! AniDB is aggressively rate-limited and ban-happy, so this client is
//! built around never asking twice: one session per enrichment run,
//! ≥2 s between packets, exponential backoff on trouble, and callers
//! cache every answer. The client identity is the registered `kahawai`
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

pub struct Anidb {
    socket: tokio::net::UdpSocket,
    session: String,
    /// AES-128 key when the session is encrypted (API key configured).
    cipher: Option<aes::Aes128>,
    last_send: Option<tokio::time::Instant>,
    tag_seq: u32,
}

impl Anidb {
    /// Login. `api_key` (profile "UDP API key") upgrades to an
    /// encrypted session first.
    pub async fn login(user: &str, pass: &str, api_key: Option<&str>) -> Result<Self> {
        let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
        socket.connect(SERVER).await.context("resolving api.anidb.net")?;
        let mut client =
            Self { socket, session: String::new(), cipher: None, last_send: None, tag_seq: 0 };

        if let Some(key) = api_key.filter(|k| !k.is_empty()) {
            let (code, rest) =
                client.command(&format!("ENCRYPT user={user}&type=1")).await?;
            match code {
                209 => {}
                309 => bail!(
                    "AniDB profile has no UDP API key defined — set one under \
                     Settings → Account on anidb.net, or clear the key here"
                ),
                other => bail!("ENCRYPT refused: {other} {rest}"),
            }
            let salt = rest.split_whitespace().next().unwrap_or_default();
            let digest = Md5::digest(format!("{key}{salt}").as_bytes());
            client.cipher = Some(aes::Aes128::new_from_slice(&digest).unwrap());
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
                if code == 201 {
                    tracing::info!("anidb: a newer client version is available");
                }
                tracing::info!("anidb session established");
                Ok(client)
            }
            500 => bail!("anidb login failed (check username/password)"),
            503 | 504 => bail!("anidb rejected the client registration: {code} {rest}"),
            555 => bail!("anidb: BANNED — stop and wait: {rest}"),
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
            501 | 506 => bail!("anidb session lost: {code}"),
            555 => bail!("anidb: BANNED — stop and wait: {rest}"),
            other => bail!("anidb FILE unexpected: {other} {rest}"),
        }
    }

    pub async fn logout(mut self) {
        if !self.session.is_empty() {
            let cmd = format!("LOGOUT s={}", self.session);
            let _ = self.command(&cmd).await;
        }
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
