//! Single TOML config file with environment overrides (NFR-6).
//! `KAHAWAI_HUB__DATA_DIR=/x` overrides `[hub] data_dir`.
//!
//! Path conventions: running as a system user (root, or no $HOME) uses
//! /var/lib and ./kahawai.toml; otherwise XDG — config at
//! `$XDG_CONFIG_HOME/kahawai/kahawai.toml`, data under `$XDG_DATA_HOME`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use figment::Figment;
use figment::providers::{Env, Format, Toml};
use serde::Deserialize;

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

fn is_system_user() -> bool {
    #[cfg(test)]
    let root = std::env::var("KAHAWAI_TEST_EUID")
        .map(|euid| euid == "0")
        .unwrap_or_else(|_| {
            // SAFETY: geteuid has no preconditions and cannot fail.
            unsafe { libc::geteuid() == 0 }
        });
    #[cfg(not(test))]
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
        xdg_dir("XDG_DATA_HOME", ".local/share")
            .unwrap()
            .join("kahawai")
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
    xdg_dir("XDG_CONFIG_HOME", ".config")
        .unwrap()
        .join("kahawai/kahawai.toml")
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Process-global GStreamer policy. When absent, decoder demotions are
    /// inherited from the legacy role sections and merged.
    #[serde(default)]
    pub gstreamer: Option<GstreamerConfig>,
    #[serde(default)]
    pub all_in_one: AllInOneConfig,
    #[serde(default)]
    pub hub: HubConfig,
    #[serde(default)]
    pub mediahost: MediahostConfig,
    #[serde(default)]
    pub transcoder: TranscoderConfig,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct GstreamerConfig {
    pub demote_decoders: Vec<String>,
}

impl Config {
    /// Decoder ranks are process-global. An explicit `[gstreamer]` section is
    /// authoritative (including an empty list); legacy configs inherit the
    /// union of the mediahost and transcoder lists in stable order.
    pub fn effective_decoder_demotions(&self) -> Vec<String> {
        let configured: Box<dyn Iterator<Item = &String> + '_> = match &self.gstreamer {
            Some(global) => Box::new(global.demote_decoders.iter()),
            None => Box::new(
                self.mediahost
                    .demote_decoders
                    .iter()
                    .chain(self.transcoder.demote_decoders.iter()),
            ),
        };
        let mut seen = std::collections::HashSet::new();
        configured
            .filter(|name| seen.insert((*name).clone()))
            .cloned()
            .collect()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AllInOneConfig {
    /// Let the hub's supervised worker encode when no suitable external
    /// transcoder is available. Remuxing remains available when this is off.
    pub transcoder: bool,
}

impl Default for AllInOneConfig {
    fn default() -> Self {
        Self { transcoder: true }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HubConfig {
    /// Client API listener. Defaults to loopback until authentication
    /// (HUB-10/11) lands — override deliberately if you must.
    pub bind: SocketAddr,
    /// Canonical browser origin. Unset preserves direct loopback operation.
    pub public_url: Option<String>,
    /// Trusted-local first-run browser listener. It exists only until the
    /// initial administrator is committed and must remain loopback-only.
    pub setup_bind: SocketAddr,
    /// Satellite listener: enrollment + (later) mTLS control/byte plane.
    pub satellite_bind: SocketAddr,
    pub data_dir: PathBuf,
    /// Hostnames/IPs put in the hub's server-cert SANs.
    pub hostnames: Vec<String>,
    pub satellite_cert_days: u32,
    pub enrollment_ttl_minutes: u64,
    /// Concurrent playback sessions ONE account may hold. Four covers a
    /// household's devices; raise it for a shared or kiosk account. The
    /// hub's own capacity is far higher — this is an per-account guard,
    /// not a capacity limit (NFR-1).
    pub max_sessions_per_user: usize,
    /// OPS-8: peers allowed to speak for clients via X-Forwarded-For.
    /// Exact IPs ("192.168.0.5") or CIDR ranges ("172.16.0.0/12" for a
    /// docker/traefik bridge). Empty = headers ignored.
    pub trusted_proxies: Vec<String>,
    /// OPS-8: CORS allowlist for third-party web clients — exact
    /// origins, or a single "*". Empty = same-origin only.
    pub cors_origins: Vec<String>,
    #[serde(default)]
    pub subtitles: SubtitlesConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SubtitlesConfig {
    pub opensubtitles: OpenSubtitlesConfig,
}

/// HUB-21. The feature is always on and needs no configuration — the
/// binary ships kahawai's registered application key. The only thing
/// worth putting in a config file is a deployment's OWN key (rate-limit
/// isolation, or if the embedded one is ever revoked); the account that
/// raises the download entitlement belongs in the admin page, which is
/// where credentials are entered.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OpenSubtitlesConfig {
    /// Overrides the embedded application key when non-empty. Also
    /// settable from the admin page; this wins.
    pub api_key: String,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8420".parse().unwrap(),
            public_url: None,
            setup_bind: "127.0.0.1:8422".parse().unwrap(),
            satellite_bind: "0.0.0.0:8421".parse().unwrap(),
            data_dir: default_hub_data_dir(),
            hostnames: vec!["localhost".into()],
            satellite_cert_days: 90,
            enrollment_ttl_minutes: 15,
            max_sessions_per_user: 4,
            trusted_proxies: Vec::new(),
            cors_origins: Vec::new(),
            subtitles: SubtitlesConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MediahostConfig {
    /// Legacy single-hub satellite address. It remains the implicit `default`
    /// hub and keeps using identity files directly in `state_dir`.
    pub hub: Option<String>,
    /// Additional independently enrolled hubs. Omitted collection filters mean
    /// every configured collection.
    pub hubs: Vec<MediahostHubConfig>,
    pub state_dir: PathBuf,
    pub name: String,
    pub collections: Vec<kahawai_core::media::CollectionConfig>,
    /// Expensive segment discovery is local policy in protocol 4.
    pub detect_segments: bool,
    /// Backup sweep interval (minutes; 0 disables). The primary change
    /// detector is the filesystem watcher — which network mounts like
    /// sshfs can't serve, so the sweep catches what inotify can't see.
    pub rescan_minutes: u64,
    /// Process-local media work admission and storage-domain overrides.
    pub scheduler: kahawai_core::media::MediahostSchedulerConfig,
    /// Legacy decoder-demotion input. When `[gstreamer]` is absent this is
    /// merged with `[transcoder].demote_decoders`; an explicit global section
    /// overrides both. Kept so deployed configs cut over without changing
    /// decoder choice.
    ///
    /// Discovery records whatever decoder GStreamer autoplugs, so `dtsdec`
    /// (lossy core only) must not file DTS-HD MA 7.1 as 5.1.
    pub demote_decoders: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediahostHubConfig {
    pub id: String,
    pub address: String,
    /// `None` means all; an explicit empty set is rejected as a likely config
    /// mistake rather than maintaining a connection that can expose nothing.
    #[serde(default)]
    pub collections: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct EffectiveMediahostHub {
    pub id: String,
    pub address: String,
    pub collections: Vec<String>,
    pub legacy_identity: bool,
}

impl MediahostConfig {
    pub fn effective_hubs(&self) -> Vec<EffectiveMediahostHub> {
        let all = || self.collections.iter().map(|c| c.name.clone()).collect();
        let mut hubs = Vec::new();
        if let Some(address) = &self.hub {
            hubs.push(EffectiveMediahostHub {
                id: "default".into(),
                address: address.clone(),
                collections: all(),
                legacy_identity: true,
            });
        }
        hubs.extend(self.hubs.iter().map(|hub| EffectiveMediahostHub {
            id: hub.id.clone(),
            address: hub.address.clone(),
            collections: hub.collections.clone().unwrap_or_else(&all),
            legacy_identity: false,
        }));
        if hubs.is_empty() {
            hubs.push(EffectiveMediahostHub {
                id: "default".into(),
                address: "localhost:8421".into(),
                collections: all(),
                legacy_identity: true,
            });
        }
        hubs
    }
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
    /// Legacy decoder-demotion input. Merged with the mediahost list only when
    /// `[gstreamer]` is absent. The calibration case is hardware whose decode
    /// path is pathologically slow (Gemini Lake `vah265dec` measured at 6 fps
    /// where `avdec_h265` does 121); encode preference is unaffected.
    pub demote_decoders: Vec<String>,
    /// TC-6 CPU share: niceness applied by each pipeline worker to
    /// itself at startup. 0 = leave it alone, which is what every
    /// deployment did before this existed.
    ///
    /// Positive only in practice — lowering niceness needs privileges we
    /// do not have and the worker logs the refusal rather than failing
    /// the session. The case this is for is `all-in-one`, where a
    /// software encode competes with the hub that has to serve the very
    /// stream it is producing; on a dedicated transcoder, transcoding is
    /// the job and there is nothing to yield to.
    ///
    /// Read by the WORKER out of this section whoever spawned it, the
    /// same way `demote_decoders` is: the worker is the thing that
    /// encodes, whether a transcoder daemon or the hub's own remux
    /// started it, so on all-in-one this governs the hub's workers too.
    pub worker_nice: i32,
    /// TC-6 CPU share: thread ceiling for SOFTWARE video encoders
    /// (x264enc, x265enc, svtav1enc, av1enc, rav1enc, openh264enc).
    /// 0 = the encoder's own default, which is "as many as this box
    /// has". Hardware encoders are untouched: their concurrency lives
    /// in the driver and is not ours to set.
    ///
    /// A ceiling on threads is not the same thing as a share of a CPU —
    /// it is the part of one a process can grant itself. The other part
    /// is a cgroup, which belongs to whatever supervises this process;
    /// `docs/kahawai-deployment.md` has the systemd form.
    pub worker_threads: u32,
}

impl Default for TranscoderConfig {
    fn default() -> Self {
        Self {
            hub: "localhost:8421".into(),
            state_dir: default_state_dir("kahawai-transcoder"),
            name: "transcoder".into(),
            max_sessions: 2,
            demote_decoders: Vec::new(),
            worker_nice: 0,
            worker_threads: 0,
        }
    }
}

impl Default for MediahostConfig {
    fn default() -> Self {
        Self {
            hub: None,
            hubs: Vec::new(),
            state_dir: default_mediahost_state_dir(),
            name: "mediahost".into(),
            collections: Vec::new(),
            detect_segments: true,
            rescan_minutes: 60,
            scheduler: Default::default(),
            demote_decoders: Vec::new(),
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
    let mut cfg: Config = Figment::new()
        .merge(Toml::file(&path))
        // Only KAHAWAI_<SECTION>__<KEY> shapes are config; other
        // KAHAWAI_* vars (worker knobs like KAHAWAI_PACE_WINDOW_MS)
        // must not crash the loader as unknown fields.
        .merge(
            Env::prefixed("KAHAWAI_")
                .filter(|k| k.as_str().contains('.') || k.as_str().contains("__"))
                .split("__"),
        )
        .extract()
        .with_context(|| format!("loading config from {}", path.display()))?;
    let base = match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(parent) if parent.is_absolute() => parent.to_path_buf(),
        Some(parent) => std::env::current_dir()?.join(parent),
        None => std::env::current_dir()?,
    };
    validate_hub_binds(&cfg.hub)?;
    normalize_and_validate_collections(&mut cfg.mediahost.collections, &base)?;
    normalize_and_validate_scheduler(&mut cfg.mediahost, &base)?;
    validate_mediahost_hubs(&cfg.mediahost)?;
    Ok((cfg, used))
}

fn normalize_and_validate_scheduler(cfg: &mut MediahostConfig, config_dir: &Path) -> Result<()> {
    if cfg.scheduler.cpu_slots == 0 {
        bail!("mediahost.scheduler.cpu_slots must be at least 1");
    }
    let configured: std::collections::HashSet<std::path::PathBuf> = cfg
        .collections
        .iter()
        .flat_map(|collection| collection.roots.iter().cloned())
        .collect();
    let mut names = std::collections::HashSet::new();
    let mut roots = std::collections::HashSet::new();
    for domain in &mut cfg.scheduler.io_domains {
        if domain.name.trim().is_empty() || !names.insert(domain.name.clone()) {
            bail!(
                "mediahost.scheduler.io_domains names must be non-empty and unique: {:?}",
                domain.name
            );
        }
        if domain.max_concurrent == 0 {
            bail!(
                "mediahost.scheduler.io_domains {:?} max_concurrent must be at least 1",
                domain.name
            );
        }
        if domain.roots.is_empty() {
            bail!(
                "mediahost.scheduler.io_domains {:?} must name at least one collection root",
                domain.name
            );
        }
        for root in &mut domain.roots {
            *root = kahawai_core::media::normalize_root_path(root, config_dir)
                .map_err(anyhow::Error::msg)?;
            if !configured.contains(root) {
                bail!(
                    "mediahost.scheduler.io_domains {:?} names unknown collection root {}",
                    domain.name,
                    root.display()
                );
            }
            if !roots.insert(root.clone()) {
                bail!(
                    "mediahost scheduler root {} occurs in more than one io_domain",
                    root.display()
                );
            }
        }
    }
    Ok(())
}

fn validate_mediahost_hubs(cfg: &MediahostConfig) -> Result<()> {
    let collections: std::collections::HashSet<&str> =
        cfg.collections.iter().map(|c| c.name.as_str()).collect();
    let mut ids = std::collections::HashSet::new();
    let mut addresses = std::collections::HashSet::new();
    if let Some(address) = &cfg.hub {
        if address.trim().is_empty() {
            bail!("mediahost.hub must not be empty");
        }
        ids.insert("default".to_string());
        addresses.insert(address.clone());
    }
    for hub in &cfg.hubs {
        if hub.id == "default" {
            bail!("mediahost hub id 'default' is reserved for mediahost.hub");
        }
        if hub.id.is_empty()
            || !hub
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            bail!(
                "mediahost hub id {:?} must contain only letters, digits, '-' or '_'",
                hub.id
            );
        }
        if !ids.insert(hub.id.clone()) {
            bail!("duplicate mediahost hub id {:?}", hub.id);
        }
        if hub.address.trim().is_empty() || !addresses.insert(hub.address.clone()) {
            bail!("duplicate or empty mediahost hub address {:?}", hub.address);
        }
        if let Some(selected) = &hub.collections {
            if selected.is_empty() {
                bail!(
                    "mediahost hub {:?} has an empty collections list; omit it for all",
                    hub.id
                );
            }
            let mut seen = std::collections::HashSet::new();
            for collection in selected {
                if !collections.contains(collection.as_str()) {
                    bail!(
                        "mediahost hub {:?} selects unknown collection {:?}",
                        hub.id,
                        collection
                    );
                }
                if !seen.insert(collection) {
                    bail!(
                        "mediahost hub {:?} selects collection {:?} twice",
                        hub.id,
                        collection
                    );
                }
            }
        }
    }
    Ok(())
}

/// Validate listener separation by port rather than exact socket address.
/// Wildcards overlap concrete addresses, and loopback aliases that bind
/// separately on Linux are not portable to macOS.
pub fn validate_hub_binds(cfg: &HubConfig) -> Result<()> {
    if !cfg.setup_bind.ip().is_loopback() {
        bail!("hub.setup_bind must be a loopback address");
    }
    if cfg.setup_bind.port() == cfg.bind.port() {
        bail!("hub.setup_bind must use a different port from hub.bind");
    }
    if cfg.setup_bind.port() == cfg.satellite_bind.port() {
        bail!("hub.setup_bind must use a different port from hub.satellite_bind");
    }
    Ok(())
}

const MEDIA_TYPES: &[&str] = &["movies", "series", "anime", "music"];

/// Normalize source namespaces before any watcher/scanner starts. This is
/// lexical by design: an unavailable mount or changed symlink target must not
/// change durable root identity.
fn normalize_and_validate_collections(
    collections: &mut [kahawai_core::media::CollectionConfig],
    config_dir: &Path,
) -> Result<()> {
    let mut names = std::collections::HashSet::new();
    for collection in collections {
        if collection.name.is_empty() {
            bail!("mediahost collection name must not be empty");
        }
        if !names.insert(collection.name.clone()) {
            bail!("duplicate mediahost collection name {:?}", collection.name);
        }
        if !MEDIA_TYPES.contains(&collection.media_type.as_str()) {
            bail!(
                "collection {:?} has unsupported media_type {:?}; expected one of {}",
                collection.name,
                collection.media_type,
                MEDIA_TYPES.join(", ")
            );
        }
        if collection.roots.is_empty() {
            bail!(
                "collection {:?} must have at least one root",
                collection.name
            );
        }

        for root in &mut collection.roots {
            let absolute = if root.is_absolute() {
                root.clone()
            } else {
                config_dir.join(&*root)
            };
            *root = kahawai_core::media::normalize_root_path(&absolute, config_dir)
                .map_err(anyhow::Error::msg)?;
        }

        for (i, root) in collection.roots.iter().enumerate() {
            for other in &collection.roots[..i] {
                if root == other {
                    bail!(
                        "collection {:?} configures duplicate root {}",
                        collection.name,
                        root.display()
                    );
                }
                if root.starts_with(other) || other.starts_with(root) {
                    bail!(
                        "collection {:?} has overlapping roots {} and {}",
                        collection.name,
                        other.display(),
                        root.display()
                    );
                }
            }
        }

        let mut tokens = std::collections::HashMap::<String, &Path>::new();
        for root in &collection.roots {
            let token = kahawai_core::media::root_token(root);
            if let Some(other) = tokens.insert(token.clone(), root)
                && other.as_os_str() != root.as_os_str()
            {
                bail!(
                    "root token collision {token}: {} and {}",
                    other.display(),
                    root.display()
                );
            }
        }
    }
    Ok(())
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
            // Explicitly model an ordinary user so this remains hermetic
            // when the test suite itself runs as root in the release image.
            jail.set_env("KAHAWAI_TEST_EUID", "1000");
            jail.set_env("HOME", "/home/test");
            jail.set_env("XDG_DATA_HOME", "");
            jail.set_env("XDG_CONFIG_HOME", "");
            let (cfg, used) = load(None).unwrap();
            assert!(used.is_none());
            assert_eq!(cfg.hub.bind, "127.0.0.1:8420".parse().unwrap());
            assert_eq!(cfg.hub.setup_bind, "127.0.0.1:8422".parse().unwrap());
            assert_eq!(cfg.hub.satellite_bind, "0.0.0.0:8421".parse().unwrap());
            assert!(cfg.hub.public_url.is_none());
            assert_eq!(
                cfg.hub.data_dir,
                PathBuf::from("/home/test/.local/share/kahawai")
            );
            assert_eq!(
                cfg.mediahost.state_dir,
                PathBuf::from("/home/test/.local/share/kahawai-mediahost")
            );
            assert_eq!(cfg.mediahost.effective_hubs()[0].address, "localhost:8421");
            assert!(cfg.all_in_one.transcoder);
            Ok(())
        });
    }

    #[test]
    fn all_in_one_transcoder_can_be_disabled() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("kahawai.toml", "[all_in_one]\ntranscoder = false\n")?;
            let (cfg, _) = load(Some(Path::new("kahawai.toml"))).unwrap();
            assert!(!cfg.all_in_one.transcoder);
            Ok(())
        });
    }

    #[test]
    fn absent_global_demotions_merge_legacy_roles_stably() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "kahawai.toml",
                "[mediahost]\ndemote_decoders = [\"dtsdec\", \"shared\"]\n\
                 [transcoder]\ndemote_decoders = [\"vah265dec\", \"shared\"]\n",
            )?;
            let (cfg, _) = load(Some(Path::new("kahawai.toml"))).unwrap();
            assert_eq!(
                cfg.effective_decoder_demotions(),
                ["dtsdec", "shared", "vah265dec"]
            );
            Ok(())
        });
    }

    #[test]
    fn explicit_global_demotions_are_authoritative_even_when_empty() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "kahawai.toml",
                "[gstreamer]\ndemote_decoders = []\n\
                 [transcoder]\ndemote_decoders = [\"vah265dec\"]\n",
            )?;
            let (cfg, _) = load(Some(Path::new("kahawai.toml"))).unwrap();
            assert!(cfg.effective_decoder_demotions().is_empty());
            Ok(())
        });
    }
    #[test]
    fn public_url_is_optional_raw_hub_config() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "kahawai.toml",
                "[hub]\npublic_url = \"https://Kahawai.EXAMPLE:443\"",
            )?;
            let (cfg, _) = load(Some(Path::new("kahawai.toml"))).unwrap();
            assert_eq!(
                cfg.hub.public_url.as_deref(),
                Some("https://Kahawai.EXAMPLE:443")
            );
            Ok(())
        });
    }

    #[test]
    fn setup_listener_must_be_port_distinct_and_loopback_only() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("kahawai.toml", "[hub]\nsetup_bind = \"0.0.0.0:8422\"")?;
            let error = load(Some(Path::new("kahawai.toml"))).unwrap_err();
            assert!(error.to_string().contains("loopback"), "{error}");

            // A wildcard public listener overlaps the concrete loopback bind.
            jail.create_file(
                "kahawai.toml",
                "[hub]\nbind = \"0.0.0.0:8422\"\nsetup_bind = \"127.0.0.1:8422\"",
            )?;
            let error = load(Some(Path::new("kahawai.toml"))).unwrap_err();
            assert!(error.to_string().contains("different port"), "{error}");

            // Distinct loopback aliases work on Linux but are not portable.
            jail.create_file(
                "kahawai.toml",
                "[hub]\nbind = \"127.0.0.2:8422\"\nsetup_bind = \"127.0.0.1:8422\"",
            )?;
            let error = load(Some(Path::new("kahawai.toml"))).unwrap_err();
            assert!(error.to_string().contains("different port"), "{error}");

            // The setup listener must not overlap the satellite wildcard either.
            jail.create_file(
                "kahawai.toml",
                "[hub]\nsetup_bind = \"127.0.0.1:8422\"\nsatellite_bind = \"0.0.0.0:8422\"",
            )?;
            let error = load(Some(Path::new("kahawai.toml"))).unwrap_err();
            assert!(error.to_string().contains("satellite_bind"), "{error}");
            Ok(())
        });
    }

    #[test]
    fn xdg_env_overrides_data_home() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("KAHAWAI_TEST_EUID", "1000");
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

    #[test]
    fn collection_roots_are_lexical_and_relative_to_the_config() {
        figment::Jail::expect_with(|jail| {
            jail.create_dir("conf")?;
            jail.create_file(
                "conf/kahawai.toml",
                "[[mediahost.collections]]\nname='movies'\nmedia_type='movies'\nroots=['../media/./films/../movies/']\n",
            )?;
            let (cfg, _) = load(Some(Path::new("conf/kahawai.toml"))).unwrap();
            assert_eq!(
                cfg.mediahost.collections[0].roots,
                vec![jail.directory().join("media/movies")]
            );
            Ok(())
        });
    }

    #[test]
    fn invalid_collection_names_types_and_overlaps_are_rejected() {
        figment::Jail::expect_with(|jail| {
            for (needle, body) in [
                (
                    "duplicate mediahost collection name",
                    "[[mediahost.collections]]\nname='x'\nmedia_type='movies'\nroots=['/a']\n[[mediahost.collections]]\nname='x'\nmedia_type='series'\nroots=['/b']\n",
                ),
                (
                    "unsupported media_type",
                    "[[mediahost.collections]]\nname='x'\nmedia_type='books'\nroots=['/a']\n",
                ),
                (
                    "overlap",
                    "[[mediahost.collections]]\nname='x'\nmedia_type='movies'\nroots=['/a','/a/sub']\n",
                ),
            ] {
                jail.create_file("kahawai.toml", body)?;
                let error = load(Some(Path::new("kahawai.toml"))).unwrap_err();
                assert!(error.to_string().contains(needle), "{error:#}");
            }
            Ok(())
        });
    }

    #[test]
    fn roots_may_overlap_across_separate_collections() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "kahawai.toml",
                "[[mediahost.collections]]\nname='a'\nmedia_type='movies'\nroots=['/media']\n[[mediahost.collections]]\nname='b'\nmedia_type='series'\nroots=['/media/series']\n",
            )?;
            load(Some(Path::new("kahawai.toml"))).unwrap();
            Ok(())
        });
    }

    #[test]
    fn scheduler_overrides_normalize_and_group_configured_roots() {
        figment::Jail::expect_with(|jail| {
            jail.create_dir("media")?;
            jail.create_dir("archive")?;
            jail.create_file(
                "kahawai.toml",
                "[mediahost.scheduler]\ncpu_slots=2\n\
                 [[mediahost.scheduler.io_domains]]\nname='nas'\nmax_concurrent=1\nroots=['media','archive']\n\
                 [[mediahost.collections]]\nname='movies'\nmedia_type='movies'\nroots=['media','archive']\n",
            )?;
            let (cfg, _) = load(Some(Path::new("kahawai.toml"))).unwrap();
            assert_eq!(cfg.mediahost.scheduler.cpu_slots, 2);
            assert_eq!(
                cfg.mediahost.scheduler.io_domains[0].roots,
                [
                    jail.directory().join("media"),
                    jail.directory().join("archive")
                ]
            );
            Ok(())
        });
    }

    #[test]
    fn scheduler_rejects_zero_capacity_unknown_and_duplicate_roots() {
        figment::Jail::expect_with(|jail| {
            jail.create_dir("media")?;
            for (needle, scheduler) in [
                ("cpu_slots", "[mediahost.scheduler]\ncpu_slots=0\n"),
                (
                    "unknown collection root",
                    "[[mediahost.scheduler.io_domains]]\nname='nas'\nroots=['elsewhere']\n",
                ),
                (
                    "more than one io_domain",
                    "[[mediahost.scheduler.io_domains]]\nname='a'\nroots=['media']\n\
                     [[mediahost.scheduler.io_domains]]\nname='b'\nroots=['media']\n",
                ),
            ] {
                jail.create_file(
                    "kahawai.toml",
                    &format!(
                        "{scheduler}[[mediahost.collections]]\nname='movies'\nmedia_type='movies'\nroots=['media']\n"
                    ),
                )?;
                let error = load(Some(Path::new("kahawai.toml"))).unwrap_err();
                assert!(error.to_string().contains(needle), "{error:#}");
            }
            Ok(())
        });
    }

    #[test]
    fn mediahost_hubs_filter_collections_and_legacy_hub_coexists() {
        figment::Jail::expect_with(|jail| {
            jail.create_dir("media")?;
            jail.create_file(
                "kahawai.toml",
                "[mediahost]\nhub='old:8421'\n\
                 [[mediahost.hubs]]\nid='family'\naddress='new:8421'\ncollections=['movies']\n\
                 [[mediahost.collections]]\nname='movies'\nmedia_type='movies'\nroots=['media']\n",
            )?;
            let (cfg, _) = load(Some(Path::new("kahawai.toml"))).unwrap();
            let hubs = cfg.mediahost.effective_hubs();
            assert_eq!(hubs.len(), 2);
            assert_eq!(hubs[0].id, "default");
            assert!(hubs[0].legacy_identity);
            assert_eq!(hubs[1].collections, ["movies"]);
            assert!(!hubs[1].legacy_identity);
            Ok(())
        });
    }

    #[test]
    fn named_hubs_do_not_add_an_implicit_localhost() {
        figment::Jail::expect_with(|jail| {
            jail.create_dir("media")?;
            jail.create_file(
                "kahawai.toml",
                "[[mediahost.hubs]]\nid='only'\naddress='hub:8421'\n\
                 [[mediahost.collections]]\nname='movies'\nmedia_type='movies'\nroots=['media']\n",
            )?;
            let (cfg, _) = load(Some(Path::new("kahawai.toml"))).unwrap();
            let hubs = cfg.mediahost.effective_hubs();
            assert_eq!(hubs.len(), 1);
            assert_eq!(hubs[0].id, "only");
            Ok(())
        });
    }
}
