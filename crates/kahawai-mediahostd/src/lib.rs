//! The mediahost, standalone: scanning, hashing, serving and extraction.
//!
//! Its own package so that no build can widen it — the hub is not a
//! dependency, so cargo cannot unify one in.

use anyhow::Result;
use kahawai_runtime::config;

pub async fn run_mediahost(cfg: config::MediahostConfig) -> Result<()> {
    let hubs = cfg
        .effective_hubs()
        .into_iter()
        .map(|hub| kahawai_mediahost::HubTarget {
            id: hub.id,
            address: hub.address,
            collections: hub.collections,
            legacy_identity: hub.legacy_identity,
        })
        .collect();
    kahawai_mediahost::run_multi(
        &cfg.state_dir,
        &cfg.name,
        cfg.collections,
        cfg.rescan_minutes,
        hubs,
        cfg.detect_segments,
    )
    .await
}
