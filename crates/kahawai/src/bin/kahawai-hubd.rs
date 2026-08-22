//! The hub, standalone: no in-process mediahost (deploy a
//! `kahawai-mediahost` beside it, or use the `kahawai` binary's
//! all-in-one), no transcoder — but it does carry the remux worker,
//! because the hub supervises local pipeline workers itself.

use std::path::PathBuf;

use anyhow::Result;

/// What this binary can run, and the doctor rows only it can produce:
/// the OCR tier lives behind the hub crate, so a build without the hub
/// has no way to ask whether Tesseract is usable.
const ROLES: Roles = Roles {
    hub: true,
    mediahost: false,
    transcoder: false,
    local_encode: false,
};

fn ocr_rows() -> Vec<kahawai_media::doctor::Check> {
    #[cfg(feature = "ocr")]
    {
        vec![kahawai_hub::ocr::doctor_check()]
    }
    #[cfg(not(feature = "ocr"))]
    {
        Vec::new()
    }
}
use clap::{Parser, Subcommand};
use kahawai_runtime::{Roles, WorkerArgs};

#[derive(Parser)]
#[command(name = "kahawai-hub", version, about = "Kahawai hub")]
struct Cli {
    /// Path to the TOML config file (see the kahawai binary's help).
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Serve /app/ from this directory instead of the embedded bundle
    /// (see the kahawai binary's help).
    #[arg(long, value_name = "DIR")]
    web_dir: Option<PathBuf>,

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
    /// Create the first administrator through the hub's private local socket.
    InitAdmin,
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
    /// Internal benchmark command retained for AIO child compatibility.
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
    // As `kahawai hub`: the flag only means something to the arm that runs the
    // hub, and discarding it in silence is how somebody spends an afternoon.
    if cli.command.is_some() && cli.web_dir.is_some() {
        anyhow::bail!("--web-dir applies to running the hub, not to its subcommands");
    }
    match cli.command {
        None => {
            kahawai_runtime::startup_checks(&cfg, ROLES, ocr_rows())?;
            kahawai::run_hub(cfg.hub, config_used, cli.web_dir).await
        }
        Some(Cmd::Doctor {
            json,
            calibrate,
            fix,
        }) => kahawai_runtime::doctor(
            &cfg,
            ROLES,
            ocr_rows(),
            json,
            calibrate,
            fix,
            config_used.as_deref(),
        ),
        Some(Cmd::InitAdmin) => kahawai::init_admin(cfg.hub).await,
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
            let m = kahawai_hub::backup::restore(&src, &cfg.hub.data_dir, force).await?;
            println!(
                "restored snapshot from {} into {}",
                m.taken_at,
                cfg.hub.data_dir.display()
            );
            Ok(())
        }
        Some(Cmd::RemuxWorker(w)) => kahawai_runtime::run_remux_worker(&cfg, w),
        Some(Cmd::Benchmark {
            cache,
            only,
            tonemap,
        }) => kahawai_runtime::run_benchmark(&cfg, cache, only, tonemap),
    }
}
