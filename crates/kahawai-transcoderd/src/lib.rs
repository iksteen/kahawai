//! The transcoder, standalone: pipelines and nothing else.
//!
//! Its own package for the same reason as the mediahost — no hub in the
//! dependency graph means no SQLite, no axum, no Tesseract, whatever
//! else the workspace is building.

use anyhow::Result;
use kahawai_runtime::config;

pub async fn run_transcoder(cfg: &config::Config) -> Result<()> {
    kahawai_transcoder::run(
        &cfg.transcoder.hub,
        &cfg.transcoder.state_dir,
        &cfg.transcoder.name,
        cfg.transcoder.max_sessions,
        std::env::current_exe().ok(),
    )
    .await
}
