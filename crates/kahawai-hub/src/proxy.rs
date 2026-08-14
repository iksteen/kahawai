//! OPS-8: reverse-proxy trust. The hub only believes X-Forwarded-For
//! when the TCP peer is a configured proxy — otherwise a client could
//! spoof its way out of per-IP throttling (OPS-2).

use std::net::IpAddr;
use std::str::FromStr;

use anyhow::{Context, Result};

/// Which peers are proxies. Entries are exact IPs ("192.168.0.5") or
/// CIDR ranges ("172.16.0.0/12" — docker/traefik bridges whose proxy
/// address changes per restart). Empty = trust nobody (default).
/// The list is behind a lock so a running hub can be handed a new one
/// (NFR-6 online reload) without rebuilding the router: every reader
/// holds the same `Arc<ProxyTrust>` and sees the swap on its next call.
#[derive(Default)]
pub struct ProxyTrust {
    nets: std::sync::RwLock<Vec<ipnet::IpNet>>,
}

impl ProxyTrust {
    pub fn parse(entries: &[String]) -> Result<Self> {
        let mut nets = Vec::with_capacity(entries.len());
        for e in entries {
            let net = if let Ok(ip) = IpAddr::from_str(e) {
                ipnet::IpNet::from(ip) // bare IP → /32 or /128
            } else {
                ipnet::IpNet::from_str(e)
                    .with_context(|| format!("bad trusted_proxies entry {e:?} (IP or CIDR)"))?
            };
            nets.push(net);
        }
        Ok(Self {
            nets: std::sync::RwLock::new(nets),
        })
    }

    /// Adopt a new list in place. Rejects the whole set on a bad entry —
    /// a reload that half-applies is worse than one that refuses, because
    /// the operator would have to guess which half took.
    pub fn reload(&self, entries: &[String]) -> Result<usize> {
        let parsed = Self::parse(entries)?;
        let new = parsed.nets.into_inner().unwrap_or_default();
        let n = new.len();
        *self.nets.write().unwrap() = new;
        Ok(n)
    }

    pub fn trusts(&self, ip: IpAddr) -> bool {
        self.nets.read().unwrap().iter().any(|n| n.contains(&ip))
    }
    /// Canonical scheme/authority supplied by a trusted reverse proxy.
    /// Both headers must be valid; otherwise the caller falls back to Host.
    pub fn forwarded_origin(
        &self,
        peer: Option<IpAddr>,
        proto: Option<&str>,
        host: Option<&str>,
    ) -> Option<String> {
        let peer = peer?;
        if !self.trusts(peer) {
            return None;
        }
        let proto = proto?.rsplit(',').next()?.trim().to_ascii_lowercase();
        if !matches!(proto.as_str(), "http" | "https") {
            return None;
        }
        let host = host?.rsplit(',').next()?.trim();
        if host.contains('@') {
            return None;
        }
        let authority = axum::http::uri::Authority::from_str(host).ok()?;
        Some(format!("{proto}://{authority}"))
    }

    /// The real client address: the TCP peer, unless the peer is a
    /// trusted proxy — then walk X-Forwarded-For right to left and take
    /// the first entry that is NOT itself a trusted proxy (entries left
    /// of that are client-supplied and unverifiable).
    pub fn client_ip(&self, peer: Option<IpAddr>, xff: Option<&str>) -> Option<IpAddr> {
        let peer = peer?;
        if !self.trusts(peer) {
            return Some(peer);
        }
        let Some(xff) = xff else { return Some(peer) };
        let mut leftmost = None;
        for hop in xff.rsplit(',') {
            let Ok(ip) = IpAddr::from_str(hop.trim()) else {
                return Some(peer); // garbled header: fall back to the peer
            };
            if !self.trusts(ip) {
                return Some(ip);
            }
            leftmost = Some(ip);
        }
        // Every hop trusted. With a blanket like 0.0.0.0/0 that is the
        // NORMAL case (nothing can be untrusted), so the leftmost entry
        // — the origin as the first proxy saw it — is the best answer;
        // for genuine proxy-to-proxy traffic it is a proxy IP, which is
        // harmless for throttling.
        Some(leftmost.unwrap_or(peer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn untrusted_peer_cannot_spoof() {
        let t = ProxyTrust::parse(&["127.0.0.1".into()]).unwrap();
        // Direct client sends a forged XFF: ignored, peer wins.
        assert_eq!(
            t.client_ip(Some(ip("203.0.113.9")), Some("10.0.0.1")),
            Some(ip("203.0.113.9"))
        );
    }

    #[test]
    fn trusted_proxy_reveals_client() {
        let t = ProxyTrust::parse(&["127.0.0.1".into()]).unwrap();
        assert_eq!(
            t.client_ip(Some(ip("127.0.0.1")), Some("203.0.113.9")),
            Some(ip("203.0.113.9"))
        );
    }

    #[test]
    fn cidr_chain_walks_to_rightmost_untrusted() {
        // docker bridge range + a spoofed left entry from the client.
        let t = ProxyTrust::parse(&["172.16.0.0/12".into()]).unwrap();
        assert_eq!(
            t.client_ip(
                Some(ip("172.18.0.2")),
                Some("10.9.9.9, 203.0.113.9, 172.18.0.3"),
            ),
            Some(ip("203.0.113.9")) // spoofed 10.9.9.9 is ignored
        );
    }

    #[test]
    fn garbled_header_falls_back_to_peer() {
        let t = ProxyTrust::parse(&["127.0.0.1".into()]).unwrap();
        assert_eq!(
            t.client_ip(Some(ip("127.0.0.1")), Some("not-an-ip")),
            Some(ip("127.0.0.1"))
        );
    }

    #[test]
    fn empty_config_trusts_nobody() {
        let t = ProxyTrust::default();
        assert_eq!(
            t.client_ip(Some(ip("127.0.0.1")), Some("203.0.113.9")),
            Some(ip("127.0.0.1"))
        );
    }

    #[test]
    fn blanket_trust_uses_leftmost_origin() {
        // 0.0.0.0/0: every hop is "trusted", so the walk must not
        // degenerate to the peer — the origin is the leftmost entry.
        let t = ProxyTrust::parse(&["0.0.0.0/0".into()]).unwrap();
        assert_eq!(
            t.client_ip(Some(ip("172.18.0.2")), Some("203.0.113.9, 172.18.0.3")),
            Some(ip("203.0.113.9"))
        );
        // No header at all still falls back to the peer.
        assert_eq!(
            t.client_ip(Some(ip("172.18.0.2")), None),
            Some(ip("172.18.0.2"))
        );
    }

    #[test]
    fn forwarded_origin_uses_only_trusted_rightmost_values() {
        let t = ProxyTrust::parse(&["127.0.0.1".into()]).unwrap();
        assert_eq!(
            t.forwarded_origin(
                Some(ip("127.0.0.1")),
                Some("http, https"),
                Some("spoofed.example, Public.EXAMPLE:443"),
            )
            .as_deref(),
            Some("https://Public.EXAMPLE:443")
        );
        assert_eq!(
            t.forwarded_origin(
                Some(ip("203.0.113.9")),
                Some("https"),
                Some("public.example"),
            ),
            None
        );
        assert_eq!(
            t.forwarded_origin(
                Some(ip("127.0.0.1")),
                Some("javascript"),
                Some("example.com")
            ),
            None
        );
        assert_eq!(
            t.forwarded_origin(Some(ip("127.0.0.1")), Some("https"), Some("bad host")),
            None
        );
        assert_eq!(
            t.forwarded_origin(None, Some("https"), Some("public.example")),
            None
        );
    }

    #[test]
    fn bad_entry_rejected() {
        assert!(ProxyTrust::parse(&["not-a-net".into()]).is_err());
    }
}
