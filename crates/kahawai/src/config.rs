//! Single TOML config file with environment overrides (NFR-6).
//! `KAHAWAI_HUB__DATA_DIR=/x` overrides `[hub] data_dir`.
//!
//! Path conventions: running as a system user (root, or no $HOME) uses
//! /var/lib and ./kahawai.toml; otherwise XDG — config at
//! `$XDG_CONFIG_HOME/kahawai/kahawai.toml`, data under `$XDG_DATA_HOME`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use figment::providers::{Env, Format, Toml};
use figment::Figment;
use serde::Deserialize;

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

fn is_system_user() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let root = unsafe { libc::geteuid() } == 0;
    root || home().is_none()
}

fn xdg_dir(env_var: &str, home_fallback: &str) -> Option<PathBuf> {
    std::env::var_os(env_var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(home_fallback)))
}

fn default_hub_data_dir() -> PathBuf {
    if is_system_user() {
        "/var/lib/kahawai".into()
    } else {
        xdg_dir("XDG_DATA_HOME", ".local/share").unwrap().join("kahawai")
    }
}

fn default_mediahost_state_dir() -> PathBuf {
    default_state_dir("kahawai-mediahost")
}

fn default_state_dir(name: &str) -> PathBuf {
    if is_system_user() {
        PathBuf::from("/var/lib").join(name)
    } else {
        xdg_dir("XDG_DATA_HOME", ".local/share").unwrap().join(name)
    }
}

/// Default config location: ./kahawai.toml if present (dev convenience),
/// else the XDG config path for non-system users.
fn default_config_path() -> PathBuf {
    let cwd = PathBuf::from("kahawai.toml");
    if cwd.exists() || is_system_user() {
        return cwd;
    }
    xdg_dir("XDG_CONFIG_HOME", ".config").unwrap().join("kahawai/kahawai.toml")
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub hub: HubConfig,
    #[serde(default)]
    pub mediahost: MediahostConfig,
    #[serde(default)]
    pub transcoder: TranscoderConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HubConfig {
    /// Client API listener. Defaults to loopback until authentication
    /// (HUB-10/11) lands — override deliberately if you must.
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
            bind: "127.0.0.1:8420".parse().unwrap(),
            satellite_bind: "0.0.0.0:8421".parse().unwrap(),
            data_dir: default_hub_data_dir(),
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TranscoderConfig {
    /// Hub satellite address, `host:port`.
    pub hub: String,
    pub state_dir: PathBuf,
    pub name: String,
    /// Concurrent encode sessions this box offers (TC-6).
    pub max_sessions: u32,
}

impl Default for TranscoderConfig {
    fn default() -> Self {
        Self {
            hub: "localhost:8421".into(),
            state_dir: default_state_dir("kahawai-transcoder"),
            name: "transcoder".into(),
            max_sessions: 2,
        }
    }
}

impl Default for MediahostConfig {
    fn default() -> Self {
        Self {
            hub: "localhost:8421".into(),
            state_dir: default_mediahost_state_dir(),
            name: "mediahost".into(),
            collections: Vec::new(),
        }
    }
}

/// Load config. An explicitly passed path must exist; the default path is
/// optional. Returns the file actually used (`None` = built-in defaults).
pub fn load(explicit: Option<&Path>) -> Result<(Config, Option<PathBuf>)> {
    let path = match explicit {
        Some(p) => {
            if !p.exists() {
                bail!("config file not found: {}", p.display());
            }
            p.to_path_buf()
        }
        None => default_config_path(),
    };
    let used = path.exists().then(|| path.clone());
    let cfg = Figment::new()
        .merge(Toml::file(&path))
        // Only KAHAWAI_<SECTION>__<KEY> shapes are config; other
        // KAHAWAI_* vars (worker knobs like KAHAWAI_PACE_WINDOW_MS)
        // must not crash the loader as unknown fields.
        .merge(Env::prefixed("KAHAWAI_").filter(|k| k.as_str().contains('.') || k.as_str().contains("__")).split("__"))
        .extract()
        .with_context(|| format!("loading config from {}", path.display()))?;
    Ok((cfg, used))
}

#[cfg(test)]
// figment::Jail closures return figment's large error type; not our code.
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;

    #[test]
    fn explicit_missing_config_errors() {
        let err = load(Some(Path::new("/nonexistent/kahawai.toml"))).unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[test]
    fn defaults_without_file() {
        figment::Jail::expect_with(|jail| {
            // No kahawai.toml in the jail cwd; XDG resolution applies.
            jail.set_env("HOME", "/home/test");
            jail.set_env("XDG_DATA_HOME", "");
            jail.set_env("XDG_CONFIG_HOME", "");
            let (cfg, used) = load(None).unwrap();
            assert!(used.is_none());
            assert_eq!(cfg.hub.bind, "127.0.0.1:8420".parse().unwrap());
            assert_eq!(cfg.hub.satellite_bind, "0.0.0.0:8421".parse().unwrap());
            assert_eq!(cfg.hub.data_dir, PathBuf::from("/home/test/.local/share/kahawai"));
            assert_eq!(
                cfg.mediahost.state_dir,
                PathBuf::from("/home/test/.local/share/kahawai-mediahost")
            );
            assert_eq!(cfg.mediahost.hub, "localhost:8421");
            Ok(())
        });
    }

    #[test]
    fn xdg_env_overrides_data_home() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("HOME", "/home/test");
            jail.set_env("XDG_DATA_HOME", "/custom/data");
            let (cfg, _) = load(None).unwrap();
            assert_eq!(cfg.hub.data_dir, PathBuf::from("/custom/data/kahawai"));
            Ok(())
        });
    }

    #[test]
    fn cwd_config_wins_over_xdg() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("HOME", "/home/test");
            jail.create_file("kahawai.toml", "[hub]\nbind = \"127.0.0.1:9000\"")?;
            let (cfg, used) = load(None).unwrap();
            assert_eq!(used, Some(PathBuf::from("kahawai.toml")));
            assert_eq!(cfg.hub.bind, "127.0.0.1:9000".parse().unwrap());
            Ok(())
        });
    }

    #[test]
    fn toml_and_env_override() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("kahawai.toml", "[hub]\nbind = \"127.0.0.1:9000\"")?;
            jail.set_env("KAHAWAI_HUB__DATA_DIR", "/tmp/kh");
            let (cfg, used) = load(Some(Path::new("kahawai.toml"))).unwrap();
            assert!(used.is_some());
            assert_eq!(cfg.hub.bind, "127.0.0.1:9000".parse().unwrap());
            assert_eq!(cfg.hub.data_dir, PathBuf::from("/tmp/kh"));
            Ok(())
        });
    }
}
