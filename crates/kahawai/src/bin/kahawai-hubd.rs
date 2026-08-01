//! The hub, standalone: no in-process mediahost (deploy a
//! `kahawai-mediahost` beside it, or use the `kahawai` binary's
//! all-in-one), no transcoder — but it does carry the remux worker,
//! because the hub supervises local pipeline workers itself.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use kahawai::WorkerArgs;

#[derive(Parser)]
#[command(name = "kahawai-hub", version, about = "Kahawai hub")]
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
    /// Overwrite a user's password (reads the new password from stdin).
    ResetPassword { username: String },
    /// Snapshot the hub (OPS-5) without stopping it.
    Backup { dest: PathBuf },
    /// Restore a snapshot into this hub's data dir. Stop the hub first.
    Restore {
        src: PathBuf,
        #[arg(long)]
        force: bool,
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
    kahawai::init_tracing();
    let cli = Cli::parse();
    let (cfg, config_used) = kahawai::load_config(cli.config.as_deref())?;
    match cli.command {
        None => {
            kahawai::startup_checks(&cfg)?;
            kahawai::run_hub(cfg.hub, config_used).await
        }
        Some(Cmd::Doctor { json }) => kahawai::doctor(&cfg, json),
        Some(Cmd::ResetPassword { username }) => kahawai::reset_password(cfg.hub, &username).await,
        Some(Cmd::Backup { dest }) => {
            let m = kahawai_hub::backup::backup(&cfg.hub.data_dir, config_used.as_deref(), &dest)
                .await?;
            println!(
                "snapshot written to {} ({:.1} MB)",
                dest.display(),
                m.db_bytes as f64 / 1e6
            );
            Ok(())
        }
        Some(Cmd::Restore { src, force }) => {
            let m = kahawai_hub::backup::restore(&src, &cfg.hub.data_dir, force)?;
            println!(
                "restored snapshot from {} into {}",
                m.taken_at,
                cfg.hub.data_dir.display()
            );
            Ok(())
        }
        Some(Cmd::RemuxWorker(w)) => kahawai::run_remux_worker(&cfg, w),
        Some(Cmd::Benchmark {
            cache,
            only,
            tonemap,
        }) => kahawai::run_benchmark(&cfg, cache, only, tonemap),
    }
}
