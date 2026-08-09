//! The transcoder, standalone: encodes and the pipeline worker it
//! spawns — no hub, no mediahost, no SQLite, no Tesseract.

use std::path::PathBuf;

use anyhow::Result;

/// This binary runs one module, so it is judged on one module's rows.
const ROLES: Roles = Roles {
    hub: false,
    mediahost: false,
    transcoder: true,
    local_encode: false,
};
use clap::{Parser, Subcommand};
use kahawai_runtime::{Roles, WorkerArgs};

#[derive(Parser)]
#[command(name = "kahawai-transcoder", version, about = "Kahawai transcoder")]
struct Cli {
    /// Path to the TOML config file (see the kahawai binary's help).
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // one value per process; size is noise
enum Cmd {
    /// Check the environment (OPS-3).
    Doctor {
        #[arg(long)]
        json: bool,
        /// OPS-9: also TIME this box's decoders against the reference
        /// clip. Seconds, not milliseconds — hence opt-in.
        #[arg(long)]
        calibrate: bool,
        /// Write the demotions this box needs into its own config
        /// (implies --calibrate). Additive and idempotent.
        #[arg(long)]
        fix: bool,
    },
    #[command(hide = true)]
    RemuxWorker(WorkerArgs),
    /// HUB-36: measure encoders into a cache file, then exit.
    #[command(hide = true)]
    Benchmark {
        #[arg(long)]
        cache: PathBuf,
        /// Measure only this element (one per process: a crash costs
        /// one measurement, not the rest of the run).
        #[arg(long)]
        only: Option<String>,
        /// Measure the GL tone-map segment.
        #[arg(long)]
        tonemap: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    kahawai_runtime::init_tracing();
    let cli = Cli::parse();
    let (cfg, config_used) = kahawai_runtime::load_config(cli.config.as_deref())?;
    match cli.command {
        None => {
            kahawai_runtime::startup_checks(&cfg, ROLES, Vec::new())?;
            kahawai_transcoderd::run_transcoder(&cfg).await
        }
        Some(Cmd::Doctor {
            json,
            calibrate,
            fix,
        }) => kahawai_runtime::doctor(
            &cfg,
            ROLES,
            Vec::new(),
            json,
            calibrate,
            fix,
            config_used.as_deref(),
        ),
        Some(Cmd::RemuxWorker(w)) => kahawai_runtime::run_remux_worker(&cfg, w),
        Some(Cmd::Benchmark {
            cache,
            only,
            tonemap,
        }) => kahawai_runtime::run_benchmark(&cfg, cache, only, tonemap),
    }
}
