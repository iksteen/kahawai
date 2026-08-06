//! The everything binary: all modules, every subcommand, all-in-one —
//! the dev-box workhorse and the compatibility surface for existing
//! scripts. Deployments that want lean per-module binaries build
//! `kahawai-hub` / `kahawai-mediahost` / `kahawai-transcoder` instead.

use std::path::PathBuf;

use anyhow::Result;

/// What this binary can run, and the doctor rows only it can produce:
/// the OCR tier lives behind the hub crate, so a build without the hub
/// has no way to ask whether Tesseract is usable.
const ROLES: Roles = Roles::all();

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
#[command(
    name = "kahawai",
    version,
    about = "Self-hosted media streaming server"
)]
struct Cli {
    /// Path to the TOML config file. Default: ./kahawai.toml, else
    /// $XDG_CONFIG_HOME/kahawai/kahawai.toml for non-system users.
    /// Env overrides: KAHAWAI_<SECTION>__<KEY>.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // one value per process; size is noise
enum Cmd {
    /// Run hub, mediahost, and transcoder in a single process.
    AllInOne,
    /// Run the hub (the module clients talk to).
    Hub {
        #[command(subcommand)]
        cmd: Option<HubCmd>,
    },
    /// Run a mediahost (announces collections from local disks).
    Mediahost,
    /// Run a transcoder.
    Transcoder,
    /// Check the environment: GStreamer inventory, directories, clock (OPS-3).
    Doctor {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
        /// OPS-9: also TIME this box's decoders against the reference
        /// clip, which the other checks are too cheap to do. Seconds,
        /// not milliseconds — hence opt-in.
        #[arg(long)]
        calibrate: bool,
        /// Write the demotions this box needs into its own config
        /// (implies --calibrate). Additive and idempotent: it never
        /// removes an entry a human put there.
        #[arg(long)]
        fix: bool,
    },
    /// Internal: per-session pipeline worker, spawned by the hub (§1.1
    /// crash isolation). Reads source bytes from the parent's socket.
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

#[derive(Subcommand)]
enum HubCmd {
    /// Overwrite a user's password (reads the new password from stdin).
    ResetPassword { username: String },
    /// Snapshot the hub (OPS-5) — database, PKI, subtitles, config —
    /// without stopping it. Image and provider caches are left out: a
    /// running hub fetches those again.
    Backup { dest: PathBuf },
    /// Restore a snapshot into this hub's data dir. Stop the hub first.
    Restore {
        src: PathBuf,
        /// Replace an existing database.
        #[arg(long)]
        force: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    kahawai_runtime::init_tracing();
    let cli = Cli::parse();
    let (cfg, config_used) = kahawai_runtime::load_config(cli.config.as_deref())?;

    match &cli.command {
        Cmd::Hub { cmd: None } | Cmd::Mediahost | Cmd::Transcoder | Cmd::AllInOne => {
            kahawai_runtime::startup_checks(&cfg, ROLES, ocr_rows())?
        }
        _ => {}
    }
    match cli.command {
        Cmd::Hub { cmd: None } => kahawai::run_hub(cfg.hub, config_used).await,
        Cmd::Hub {
            cmd: Some(HubCmd::ResetPassword { username }),
        } => kahawai::reset_password(cfg.hub, &username).await,
        Cmd::Hub {
            cmd: Some(HubCmd::Backup { dest }),
        } => {
            let m = kahawai_hub::backup::backup(&cfg.hub.data_dir, config_used.as_deref(), &dest)
                .await?;
            println!(
                "snapshot written to {}\n  database   {:.1} MB\n  subtitles  {} files, {:.1} MB\n  pki        {}\n  config     {}",
                dest.display(),
                m.db_bytes as f64 / 1e6,
                m.subtitle_files,
                m.subtitle_bytes as f64 / 1e6,
                if m.has_pki { "included" } else { "absent" },
                if m.has_config { "included" } else { "absent" },
            );
            Ok(())
        }
        Cmd::Hub {
            cmd: Some(HubCmd::Restore { src, force }),
        } => {
            let m = kahawai_hub::backup::restore(&src, &cfg.hub.data_dir, force)?;
            println!(
                "restored a snapshot taken at {} (kahawai {}) into {}",
                m.taken_at,
                m.kahawai_version,
                cfg.hub.data_dir.display()
            );
            Ok(())
        }
        Cmd::Mediahost => kahawai_mediahostd::run_mediahost(cfg.mediahost).await,
        Cmd::Doctor {
            json,
            calibrate,
            fix,
        } => kahawai_runtime::doctor(
            &cfg,
            ROLES,
            ocr_rows(),
            json,
            calibrate,
            fix,
            config_used.as_deref(),
        ),
        Cmd::RemuxWorker(w) => kahawai_runtime::run_remux_worker(&cfg, w),
        Cmd::Benchmark {
            cache,
            only,
            tonemap,
        } => kahawai_runtime::run_benchmark(&cfg, cache, only, tonemap),
        Cmd::Transcoder => kahawai_transcoderd::run_transcoder(&cfg).await,
        Cmd::AllInOne => kahawai::run_all_in_one(cfg, config_used).await,
    }
}
