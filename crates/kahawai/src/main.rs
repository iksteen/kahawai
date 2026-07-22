use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let cfg = config::load(&cli.config)?;

    match cli.command {
        Cmd::Hub => run_hub(cfg.hub).await,
        Cmd::Mediahost => run_mediahost(cfg.mediahost).await,
        Cmd::AllInOne | Cmd::Transcoder => {
            anyhow::bail!("not implemented yet — `kahawai hub` and `kahawai mediahost` work so far")
        }
    }
}

async fn run_hub(cfg: config::HubConfig) -> Result<()> {
    let ca = Arc::new(kahawai_hub::pki::HubCa::load_or_create(
        &kahawai_hub::pki::pki_dir(&cfg.data_dir),
    )?);
    let db = kahawai_hub::db::open(&cfg.data_dir).await?;
    let registry = Arc::new(kahawai_hub::registry::Registry::new(db.clone()));

    // Revocations persist across restarts (SEC-6).
    let revoked = kahawai_transport::mtls::RevocationList::default();
    let fps: Vec<String> = sqlx::query_scalar("SELECT fingerprint FROM revoked_certs")
        .fetch_all(&db)
        .await?;
    for fp in fps {
        revoked.revoke(&fp);
    }

    let (cert_pem, key_pem) = ca.issue_server_cert(&cfg.hostnames)?;
    let tls = kahawai_transport::mtls::mtls_server_config(
        &cert_pem,
        &key_pem,
        ca.ca_cert_pem(),
        revoked.clone(),
    )?;

    let svc = kahawai_hub::enrollment_service::EnrollmentService::new(
        ca.clone(),
        registry.clone(),
        Duration::from_secs(cfg.enrollment_ttl_minutes * 60),
        cfg.satellite_cert_days,
    );

    // SEC-3 CLI approval: type a satellite's console code + Enter.
    let approver = svc.clone();
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        eprintln!("Type an enrollment code + Enter to approve a satellite.");
        while let Ok(Some(line)) = lines.next_line().await {
            let code = line.trim();
            if code.is_empty() {
                continue;
            }
            match approver.approve(code).await {
                Ok(summary) => eprintln!("approved: {summary}"),
                Err(e) => eprintln!("rejected: {e}"),
            }
        }
    });

    // Client API (browse). No auth yet — keep it on loopback (see config).
    let api_listener = tokio::net::TcpListener::bind(cfg.bind)
        .await
        .with_context(|| format!("binding client API on {}", cfg.bind))?;
    let api = kahawai_hub::api::router(registry.clone());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(api_listener, api).await {
            tracing::error!(error = %e, "client API server failed");
        }
    });

    let listener = tokio::net::TcpListener::bind(cfg.satellite_bind)
        .await
        .with_context(|| format!("binding satellite listener on {}", cfg.satellite_bind))?;
    tracing::info!(
        bind = %cfg.bind,
        satellite_bind = %cfg.satellite_bind,
        ca_fingerprint = ca.ca_fingerprint(),
        "hub up"
    );

    tonic::transport::Server::builder()
        .add_service(svc.into_server())
        .add_service(kahawai_hub::link_service::MediahostLinkService::new(registry).into_server())
        .serve_with_incoming(kahawai_transport::tls::tls_incoming(listener, tls))
        .await
        .context("satellite listener failed")
}

async fn run_mediahost(cfg: config::MediahostConfig) -> Result<()> {
    kahawai_mediahost::run(&cfg.hub, &cfg.state_dir, &cfg.name, cfg.collections).await
}
