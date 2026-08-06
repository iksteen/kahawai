//! The mediahost, standalone: scanning, hashing, serving and extraction.
//!
//! Its own package so that no build can widen it — the hub is not a
//! dependency, so cargo cannot unify one in.

use anyhow::Result;
use kahawai_runtime::config;

pub async fn run_mediahost(cfg: config::MediahostConfig) -> Result<()> {
    // Before any discovery runs: what the scan records is whatever
    // decoder GStreamer autoplugs, so this list is what keeps the
    // library's view of a stream from being narrower than playback's.
    kahawai_media::demote_elements(&cfg.demote_decoders)?;
    kahawai_mediahost::run(
        &cfg.hub,
        &cfg.state_dir,
        &cfg.name,
        cfg.collections,
        cfg.rescan_minutes,
    )
    .await
}
