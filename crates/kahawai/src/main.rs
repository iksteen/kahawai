use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

mod config;

#[derive(Parser)]
#[command(name = "kahawai", version, about = "Self-hosted media streaming server")]
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
    RemuxWorker {
        socket: PathBuf,
        out_dir: PathBuf,
        size: u64,
        #[arg(long, default_value = "off")]
        video: String,
        #[arg(long, default_value = "off")]
        audio: String,
    },
}

#[derive(Subcommand)]
enum HubCmd {
    /// Overwrite a user's password (reads the new password from stdin).
    ResetPassword { username: String },
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
    let (cfg, config_used) = config::load(cli.config.as_deref())?;
    match &config_used {
        Some(p) => tracing::info!(config = %p.display(), "loaded config"),
        None => tracing::info!("no config file found; using built-in defaults"),
    }

    match &cli.command {
        Cmd::Hub { cmd: None } | Cmd::Mediahost | Cmd::Transcoder => startup_checks(&cfg)?,
        _ => {}
    }
    match cli.command {
        Cmd::Hub { cmd: None } => run_hub(cfg.hub).await,
        Cmd::Hub { cmd: Some(HubCmd::ResetPassword { username }) } => {
            reset_password(cfg.hub, &username).await
        }
        Cmd::Mediahost => run_mediahost(cfg.mediahost).await,
        Cmd::Doctor { json } => doctor(&cfg, json),
        Cmd::RemuxWorker { socket, out_dir, size, video, audio } => {
            // Blocking by design: this process exists only for the pipeline.
            kahawai_media::worker::run(
                &socket,
                &out_dir,
                size,
                kahawai_media::worker::parse_mode(&video),
                kahawai_media::worker::parse_mode(&audio),
            )
        }
        Cmd::Transcoder => {
            kahawai_transcoder::run(
                &cfg.transcoder.hub,
                &cfg.transcoder.state_dir,
                &cfg.transcoder.name,
                cfg.transcoder.max_sessions,
            )
            .await
        }
        Cmd::AllInOne => {
            anyhow::bail!("not implemented yet — hub, mediahost and transcoder run separately")
        }
    }
}

/// Environment checks (OPS-3): shared GStreamer inventory plus per-module
/// filesystem and clock checks from the loaded config.
fn doctor_checks(cfg: &config::Config) -> Vec<kahawai_media::doctor::Check> {
    use kahawai_media::doctor::Check;
    let mut checks = kahawai_media::doctor::gstreamer_checks();

    // Clock sanity: satellites on RTC-less boxes boot in the past (OPS-4).
    let year_2025 = 1735689600;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    checks.push(if now > year_2025 {
        Check::ok("system clock", "sane")
    } else {
        Check::fail("system clock", "before 2025 — fix NTP or certificates will fail", true)
    });

    let dir_check = |name: &str, dir: &std::path::Path, must_write: bool| {
        let meta = std::fs::metadata(dir);
        match meta {
            Ok(m) if m.is_dir() => {
                if !must_write || !m.permissions().readonly() {
                    Check::ok(name, dir.display().to_string())
                } else {
                    Check::fail(name, format!("{} is read-only", dir.display()), true)
                }
            }
            _ if must_write => {
                // Created recursively on first run — fine as long as the
                // nearest existing ancestor is a directory.
                let ancestor_ok = dir
                    .ancestors()
                    .find(|a| a.exists())
                    .is_some_and(|a| a.is_dir());
                if ancestor_ok {
                    Check::ok(name, format!("{} (will be created)", dir.display()))
                } else {
                    Check::fail(name, format!("{} unusable", dir.display()), true)
                }
            }
            _ => Check::warn(name, format!("{} does not exist", dir.display())),
        }
    };
    checks.push(dir_check("hub data dir", &cfg.hub.data_dir, true));
    checks.push(dir_check("mediahost state dir", &cfg.mediahost.state_dir, true));
    for c in &cfg.mediahost.collections {
        for root in &c.roots {
            checks.push(dir_check(&format!("collection \"{}\" root", c.name), root, false));
        }
    }

    // Hardware acceleration is optional but worth surfacing.
    checks.push(if std::path::Path::new("/dev/dri").exists() {
        kahawai_media::doctor::Check::ok("/dev/dri", "present (VA-API possible)")
    } else {
        kahawai_media::doctor::Check::warn("/dev/dri", "absent — hardware acceleration unavailable")
    });
    checks
}

fn doctor(cfg: &config::Config, json: bool) -> Result<()> {
    use kahawai_media::doctor::Status;
    let checks = doctor_checks(cfg);
    if json {
        println!("{}", serde_json::to_string_pretty(&checks)?);
    } else {
        for c in &checks {
            let tag = match c.status {
                Status::Ok => "OK  ",
                Status::Warn => "WARN",
                Status::Fail => "FAIL",
            };
            println!("{tag} {:<28} {}", c.name, c.detail);
        }
    }
    if kahawai_media::doctor::has_essential_failure(&checks) {
        anyhow::bail!("essential checks failed");
    }
    Ok(())
}

/// Startup gate: log warnings, abort on essential failures (OPS-3).
fn startup_checks(cfg: &config::Config) -> Result<()> {
    use kahawai_media::doctor::Status;
    let checks = doctor_checks(cfg);
    for c in &checks {
        match c.status {
            Status::Ok => {}
            Status::Warn => tracing::warn!(check = %c.name, "{}", c.detail),
            Status::Fail => tracing::error!(check = %c.name, "{}", c.detail),
        }
    }
    if kahawai_media::doctor::has_essential_failure(&checks) {
        anyhow::bail!("environment not usable — run `kahawai doctor` for details");
    }
    Ok(())
}

async fn reset_password(cfg: config::HubConfig, username: &str) -> Result<()> {
    let db = kahawai_hub::db::open(&cfg.data_dir).await?;
    eprint!("New password for {username}: ");
    let mut pw = String::new();
    std::io::stdin().read_line(&mut pw)?;
    let pw = pw.trim_end_matches ('\n');
    anyhow::ensure!(pw.len() >= 8, "password must be at least 8 characters");
    kahawai_hub::auth::reset_password(&db, username, pw).await?;
    println!("password updated; existing sessions revoked");
    Ok(())
}

async fn run_hub(cfg: config::HubConfig) -> Result<()> {
    let ca = Arc::new(kahawai_hub::pki::HubCa::load_or_create(
        &kahawai_hub::pki::pki_dir(&cfg.data_dir),
    )?);
    let db = kahawai_hub::db::open(&cfg.data_dir).await?;
    // The satellites table IS the mTLS allowlist (SEC-5): load it, then
    // the registry keeps it in sync on approve/delete.
    let allowed = kahawai_transport::mtls::AllowedCerts::default();
    let registry = Arc::new(kahawai_hub::registry::Registry::new(db.clone(), allowed.clone()));
    let admitted = registry.load_allowlist().await?;
    tracing::info!(admitted, "mTLS allowlist loaded");
    let auth = Arc::new(kahawai_hub::auth::Auth::new(db.clone(), &cfg.data_dir).await?);
    let sessions = Arc::new(
        kahawai_hub::sessions::Sessions::new(cfg.data_dir.join("sessions"))
            // Pipelines run in a supervised child of this same binary
            // (hidden `remux-worker` subcommand): a GStreamer crash kills
            // one session, never the hub (§1.1).
            .with_worker_exe(std::env::current_exe().ok()),
    );
    sessions.spawn_janitor();

    let (cert_pem, key_pem) = ca.issue_server_cert(&cfg.hostnames)?;
    let tls = kahawai_transport::mtls::mtls_server_config(
        &cert_pem,
        &key_pem,
        ca.ca_cert_pem(),
        allowed.clone(),
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
    let api = kahawai_hub::api::router(registry.clone(), auth, sessions.clone(), Arc::new(svc.clone()));
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
        .add_service(
            kahawai_hub::link_service::MediahostLinkService::new(registry.clone(), sessions)
                .into_server(),
        )
        .add_service(
            kahawai_hub::transcoder_link::TranscoderLinkService::new(registry).into_server(),
        )
        .serve_with_incoming(kahawai_transport::tls::tls_incoming(listener, tls))
        .await
        .context("satellite listener failed")
}

async fn run_mediahost(cfg: config::MediahostConfig) -> Result<()> {
    kahawai_mediahost::run(&cfg.hub, &cfg.state_dir, &cfg.name, cfg.collections).await
}
