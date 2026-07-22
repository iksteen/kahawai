use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod config;

#[derive(Parser)]
#[command(name = "kahawai", version, about = "Self-hosted media streaming server")]
struct Cli {
    /// Path to the TOML config file (env overrides: KAHAWAI_<SECTION>__<KEY>).
    #[arg(short, long, global = true, default_value = "kahawai.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run hub, mediahost, and transcoder in a single process.
    AllInOne,
    /// Run the hub (the module clients talk to).
    Hub,
    /// Run a mediahost (announces collections from local disks).
    Mediahost,
    /// Run a transcoder.
    Transcoder,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let cfg = config::load(&cli.config)?;

    match cli.command {
        Cmd::Hub => run_hub(cfg.hub),
        Cmd::AllInOne | Cmd::Mediahost | Cmd::Transcoder => {
            anyhow::bail!("not implemented yet — only `kahawai hub` bootstraps so far")
        }
    }
}

fn run_hub(cfg: config::HubConfig) -> Result<()> {
    let ca = kahawai_hub::pki::HubCa::load_or_create(&kahawai_hub::pki::pki_dir(&cfg.data_dir))?;
    tracing::info!(
        bind = %cfg.bind,
        data_dir = %cfg.data_dir.display(),
        ca_fingerprint = ca.ca_fingerprint(),
        "hub bootstrapped"
    );
    tracing::warn!("hub server not implemented yet; exiting after CA bootstrap");
    Ok(())
}
