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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubConfig {
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self { bind: default_bind(), data_dir: default_data_dir() }
    }
}

fn default_bind() -> SocketAddr {
    "0.0.0.0:8420".parse().unwrap()
}

fn default_data_dir() -> PathBuf {
    "/var/lib/kahawai".into()
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
        assert_eq!(cfg.hub.bind, default_bind());
        assert_eq!(cfg.hub.data_dir, default_data_dir());
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
