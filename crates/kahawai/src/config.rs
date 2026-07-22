//! Single TOML config file with environment overrides (NFR-6).
//! `KAHAWAI_HUB__DATA_DIR=/x` overrides `[hub] data_dir`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use figment::providers::{Env, Format, Toml};
use figment::Figment;
use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub hub: HubConfig,
    #[serde(default)]
    pub mediahost: MediahostConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HubConfig {
    /// Client API listener (not served yet).
    pub bind: SocketAddr,
    /// Satellite listener: enrollment + (later) mTLS control/byte plane.
    pub satellite_bind: SocketAddr,
    pub data_dir: PathBuf,
    /// Hostnames/IPs put in the hub's server-cert SANs.
    pub hostnames: Vec<String>,
    pub satellite_cert_days: u32,
    pub enrollment_ttl_minutes: u64,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8420".parse().unwrap(),
            satellite_bind: "0.0.0.0:8421".parse().unwrap(),
            data_dir: "/var/lib/kahawai".into(),
            hostnames: vec!["localhost".into()],
            satellite_cert_days: 90,
            enrollment_ttl_minutes: 15,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MediahostConfig {
    /// Hub satellite address, `host:port`.
    pub hub: String,
    pub state_dir: PathBuf,
    pub name: String,
    pub collections: Vec<kahawai_mediahost::scan::CollectionConfig>,
}

impl Default for MediahostConfig {
    fn default() -> Self {
        Self {
            hub: "localhost:8421".into(),
            state_dir: "/var/lib/kahawai-mediahost".into(),
            name: "mediahost".into(),
            collections: Vec::new(),
        }
    }
}

pub fn load(path: &Path) -> Result<Config> {
    Figment::new()
        .merge(Toml::file(path))
        .merge(Env::prefixed("KAHAWAI_").split("__"))
        .extract()
        .with_context(|| format!("loading config from {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_without_file() {
        let cfg = load(Path::new("/nonexistent/kahawai.toml")).unwrap();
        assert_eq!(cfg.hub.bind, "0.0.0.0:8420".parse().unwrap());
        assert_eq!(cfg.hub.satellite_bind, "0.0.0.0:8421".parse().unwrap());
        assert_eq!(cfg.hub.data_dir, PathBuf::from("/var/lib/kahawai"));
        assert_eq!(cfg.mediahost.hub, "localhost:8421");
    }

    #[test]
    fn toml_and_env_override() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("kahawai.toml", "[hub]\nbind = \"127.0.0.1:9000\"")?;
            jail.set_env("KAHAWAI_HUB__DATA_DIR", "/tmp/kh");
            let cfg = load(Path::new("kahawai.toml")).unwrap();
            assert_eq!(cfg.hub.bind, "127.0.0.1:9000".parse().unwrap());
            assert_eq!(cfg.hub.data_dir, PathBuf::from("/tmp/kh"));
            Ok(())
        });
    }
}
