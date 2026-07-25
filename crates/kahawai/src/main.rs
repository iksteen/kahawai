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
        #[arg(long, default_value_t = 0)]
        audio_track: usize,
        #[arg(long, default_value_t = 0)]
        video_track: usize,
        #[arg(long, default_value_t = 0)]
        start_ms: u64,
        #[arg(long)]
        sink: Option<String>,
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
        Cmd::Hub { cmd: None } | Cmd::Mediahost | Cmd::Transcoder | Cmd::AllInOne => {
            startup_checks(&cfg)?
        }
        _ => {}
    }
    match cli.command {
        Cmd::Hub { cmd: None } => run_hub(cfg.hub).await,
        Cmd::Hub { cmd: Some(HubCmd::ResetPassword { username }) } => {
            reset_password(cfg.hub, &username).await
        }
        Cmd::Mediahost => run_mediahost(cfg.mediahost).await,
        Cmd::Doctor { json } => doctor(&cfg, json),
        Cmd::RemuxWorker { socket, out_dir, size, video, audio, audio_track, video_track, start_ms, sink } => {
            // Blocking by design: this process exists only for the pipeline.
            kahawai_media::demote_elements(&cfg.transcoder.demote_decoders)?;
            kahawai_media::worker::run(
                &socket,
                &out_dir,
                size,
                kahawai_media::worker::parse_mode(&video),
                kahawai_media::worker::parse_mode(&audio),
                audio_track,
                video_track,
                start_ms,
                sink.as_deref(),
            )
        }
        Cmd::Transcoder => {
            kahawai_transcoder::run(
                &cfg.transcoder.hub,
                &cfg.transcoder.state_dir,
                &cfg.transcoder.name,
                cfg.transcoder.max_sessions,
                std::env::current_exe().ok(),
            )
            .await
        }
        Cmd::AllInOne => run_all_in_one(cfg).await,
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
    // Linux-only: DRI render nodes are how VA-API reaches the GPU, and
    // the classic failure is a service user missing the render/video
    // group — the encoder dry-run then fails with no hint why. Other
    // platforms (VideoToolbox, NVENC-on-Windows) have no device node;
    // the dry-run-verified encoder rows are the whole story there.
    #[cfg(target_os = "linux")]
    checks.push({
        use kahawai_media::doctor::Check;
        let nodes: Vec<std::path::PathBuf> = std::fs::read_dir("/dev/dri")
            .map(|rd| {
                rd.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("renderD"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if nodes.is_empty() {
            Check::warn("/dev/dri", "no render nodes — VA-API/GPU encode unavailable")
        } else if let Some(node) = nodes.iter().find(|n| {
            std::fs::OpenOptions::new().read(true).write(true).open(n).is_ok()
        }) {
            Check::ok("/dev/dri", format!("{} accessible", node.display()))
        } else {
            Check::warn(
                "/dev/dri",
                "render nodes exist but are not accessible — add this user to the render/video group",
            )
        }
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
    run_hub_inner(cfg, None).await
}

/// AR-5 all-in-one: the hub plus an IN-PROCESS mediahost — module logic
/// unchanged, transport replaced by channels, byte plane replaced by
/// direct file reads. The satellite listener stays up: external
/// mediahosts/transcoders enroll and dial in exactly as in modular mode.
async fn run_all_in_one(cfg: config::Config) -> Result<()> {
    anyhow::ensure!(
        !cfg.mediahost.collections.is_empty(),
        "all-in-one needs at least one [[mediahost.collections]] entry"
    );
    run_hub_inner(cfg.hub, Some(cfg.mediahost)).await
}

async fn run_hub_inner(
    cfg: config::HubConfig,
    local_mediahost: Option<config::MediahostConfig>,
) -> Result<()> {
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
    sessions.attach_registry(registry.clone());

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
    let subtitles =
        Arc::new(kahawai_hub::subtitles::Subtitles::new(cfg.data_dir.join("subtitles")));
    let enricher = Arc::new(kahawai_hub::enrich::Enricher::new(cfg.data_dir.clone()));
    let artwork = Arc::new(kahawai_hub::artwork::Artwork::new(
        cfg.data_dir.join("artwork"),
        enricher.clone(),
    ));
    // Enrich whatever resolution has produced since last time.
    {
        let enricher = enricher.clone();
        let registry = registry.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(15)).await;
            match enricher.run_once(&registry).await {
                Ok(_) => {}
                Err(e) => tracing::debug!(error = format!("{e:#}"), "startup enrichment skipped"),
            }
        });
    }
    if let Some(mh) = local_mediahost {
        const LOCAL_ID: &str = "local";
        registry.ensure_local_satellite(LOCAL_ID, &mh.name).await?;
        let (tx, rx) = kahawai_hub::link_service::local_link(
            registry.clone(),
            subtitles.clone(),
            enricher.clone(),
            LOCAL_ID,
            &mh.name,
        );
        let cols = mh.collections.clone();
        sessions.set_local_source(LOCAL_ID, move |collection, path| {
            kahawai_mediahost::serve::resolve_rel(&cols, collection, path)
        });
        let state_dir = mh.state_dir.clone();
        tokio::spawn(async move {
            if let Err(e) = kahawai_mediahost::run_local(
                mh.collections,
                mh.rescan_minutes,
                &state_dir,
                tx,
                rx,
            )
            .await
            {
                tracing::error!(error = format!("{e:#}"), "in-process mediahost exited");
            }
        });
        tracing::info!("in-process mediahost started (AR-5)");
    }

    let net = kahawai_hub::api::NetOptions {
        proxy_trust: kahawai_hub::proxy::ProxyTrust::parse(&cfg.trusted_proxies)
            .context("hub.trusted_proxies")?,
        cors_origins: cfg.cors_origins.clone(),
    };
    let api = kahawai_hub::api::router(registry.clone(), auth, sessions.clone(), Arc::new(svc.clone()), subtitles.clone(), artwork, enricher.clone(), net);
    tokio::spawn(async move {
        let api = api.into_make_service_with_connect_info::<std::net::SocketAddr>();
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
            kahawai_hub::renewal_service::RenewalService::new(
                ca.clone(),
                registry.clone(),
                cfg.satellite_cert_days,
            )
            .into_server(),
        )
        .add_service(
            kahawai_hub::link_service::MediahostLinkService::new(
                registry.clone(),
                sessions.clone(),
                subtitles.clone(),
                enricher.clone(),
            )
            .into_server(),
        )
        .add_service(
            kahawai_hub::transcoder_link::TranscoderLinkService::new(registry, sessions)
                .into_server(),
        )
        .serve_with_incoming(kahawai_transport::tls::tls_incoming(listener, tls))
        .await
        .context("satellite listener failed")
}

async fn run_mediahost(cfg: config::MediahostConfig) -> Result<()> {
    kahawai_mediahost::run(&cfg.hub, &cfg.state_dir, &cfg.name, cfg.collections, cfg.rescan_minutes)
        .await
}
