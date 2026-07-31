//! The transcoder, standalone: encodes and the pipeline worker it
//! spawns — no hub, no mediahost, no SQLite, no Tesseract.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use kahawai::WorkerArgs;

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
    },
    #[command(hide = true)]
    RemuxWorker(WorkerArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    kahawai::init_tracing();
    let cli = Cli::parse();
    let (cfg, _) = kahawai::load_config(cli.config.as_deref())?;
    match cli.command {
        None => {
            kahawai::startup_checks(&cfg)?;
            kahawai::run_transcoder(&cfg).await
        }
        Some(Cmd::Doctor { json }) => kahawai::doctor(&cfg, json),
        Some(Cmd::RemuxWorker(w)) => kahawai::run_remux_worker(&cfg, w),
    }
}
