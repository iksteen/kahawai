//! The mediahost, standalone: scanning, hashing, serving and
//! extraction — no hub, no transcoder, no GUI-sized dependency tree.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "kahawai-mediahost", version, about = "Kahawai mediahost")]
struct Cli {
    /// Path to the TOML config file (see the kahawai binary's help).
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
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
}

#[tokio::main]
async fn main() -> Result<()> {
    kahawai::init_tracing();
    let cli = Cli::parse();
    let (cfg, config_used) = kahawai::load_config(cli.config.as_deref())?;
    match cli.command {
        None => {
            kahawai::startup_checks(&cfg)?;
            kahawai::run_mediahost(cfg.mediahost).await
        }
        Some(Cmd::Doctor {
            json,
            calibrate,
            fix,
        }) => kahawai::doctor(&cfg, json, calibrate, fix, config_used.as_deref()),
    }
}
