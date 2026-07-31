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
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    kahawai::init_tracing();
    let cli = Cli::parse();
    let (cfg, _) = kahawai::load_config(cli.config.as_deref())?;
    match cli.command {
        None => {
            kahawai::startup_checks(&cfg)?;
            kahawai::run_mediahost(cfg.mediahost).await
        }
        Some(Cmd::Doctor { json }) => kahawai::doctor(&cfg, json),
    }
}
