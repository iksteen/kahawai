//! The everything binary: all modules, every subcommand, all-in-one —
//! the dev-box workhorse and the compatibility surface for existing
//! scripts. Deployments that want lean per-module binaries build
//! `kahawai-hub` / `kahawai-mediahost` / `kahawai-transcoder` instead.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use kahawai::WorkerArgs;

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
    },
    /// Internal: per-session pipeline worker, spawned by the hub (§1.1
    /// crash isolation). Reads source bytes from the parent's socket.
    #[command(hide = true)]
    RemuxWorker(WorkerArgs),
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
    kahawai::init_tracing();
    let cli = Cli::parse();
    let (cfg, config_used) = kahawai::load_config(cli.config.as_deref())?;

    match &cli.command {
        Cmd::Hub { cmd: None } | Cmd::Mediahost | Cmd::Transcoder | Cmd::AllInOne => {
            kahawai::startup_checks(&cfg)?
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
        Cmd::Mediahost => kahawai::run_mediahost(cfg.mediahost).await,
        Cmd::Doctor { json } => kahawai::doctor(&cfg, json),
        Cmd::RemuxWorker(w) => kahawai::run_remux_worker(&cfg, w),
        Cmd::Transcoder => kahawai::run_transcoder(&cfg).await,
        Cmd::AllInOne => kahawai::run_all_in_one(cfg, config_used).await,
    }
}
