//! The mediahost, standalone: scanning, hashing, serving and extraction.
//!
//! Its own package so that no build can widen it — the hub is not a
//! dependency, so cargo cannot unify one in.

use anyhow::Result;
use kahawai_runtime::config;

pub async fn run_mediahost(cfg: config::MediahostConfig) -> Result<()> {
    kahawai_mediahost::run(
        &cfg.hub,
        &cfg.state_dir,
        &cfg.name,
        cfg.collections,
        cfg.rescan_minutes,
    )
    .await
}
