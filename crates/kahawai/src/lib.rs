//! The hub, and the all-in-one that runs a mediahost inside it.
//!
//! Everything here needs the hub crate — and therefore SQLite, axum and
//! the OCR engine — which is why it lives in this package rather than in
//! kahawai-runtime, where the satellites would inherit it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use kahawai_runtime::config;

pub async fn reset_password(cfg: config::HubConfig, username: &str) -> Result<()> {
    let db = kahawai_hub::db::open(&cfg.data_dir).await?;
    eprint!("New password for {username}: ");
    let mut pw = String::new();
    std::io::stdin().read_line(&mut pw)?;
    let pw = pw.trim_end_matches('\n');
    anyhow::ensure!(pw.len() >= 8, "password must be at least 8 characters");
    kahawai_hub::auth::reset_password(&db, username, pw).await?;
    println!("password updated; existing sessions revoked");
    Ok(())
}

pub async fn run_hub(cfg: config::HubConfig, config_path: Option<PathBuf>) -> Result<()> {
    run_hub_inner(cfg, None, config_path).await
}

/// AR-5 all-in-one: the hub plus an IN-PROCESS mediahost — module logic
/// unchanged, transport replaced by channels, byte plane replaced by
/// direct file reads. The satellite listener stays up: external
/// mediahosts/transcoders enroll and dial in exactly as in modular mode.
pub async fn run_all_in_one(cfg: config::Config, config_path: Option<PathBuf>) -> Result<()> {
    anyhow::ensure!(
        !cfg.mediahost.collections.is_empty(),
        "all-in-one needs at least one [[mediahost.collections]] entry"
    );
    run_hub_inner(cfg.hub, Some(cfg.mediahost), config_path).await
}

/// HUB-36: measure this box's encoders in the background and hand the
/// results to the registry, so local execution competes for placement
/// on the same measured footing as the fleet. Cached on disk, keyed by
/// GStreamer version; a drifted measurement simply overwrites it.
fn spawn_local_benchmark(cache: PathBuf, registry: Arc<kahawai_hub::registry::Registry>) {
    const SETTLE: Duration = Duration::from_secs(60);
    const BENCH_BUDGET: Duration = Duration::from_secs(300);
    if let Some(cached) = kahawai_media::bench::load(&cache) {
        registry.set_local_bench(cached);
    }
    tokio::spawn(async move {
        tokio::time::sleep(SETTLE).await;
        // Child process, for the reason pipelines are children: this is
        // GStreamer work, and it killed a satellite outright when it
        // ran in-process (svtav1enc on the J5005, HUB-36).
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        // One child per piece: a crash costs that measurement alone.
        let mut jobs: Vec<Vec<String>> = vec![vec!["--tonemap".into()]];
        jobs.extend(
            kahawai_runtime::benchmark_elements()
                .into_iter()
                .map(|el| vec!["--only".into(), el]),
        );
        for args in jobs {
            let child = tokio::process::Command::new(&exe)
                .arg("benchmark")
                .arg("--cache")
                .arg(&cache)
                .args(&args)
                .kill_on_drop(true)
                .status();
            match tokio::time::timeout(BENCH_BUDGET, child).await {
                Ok(Ok(st)) if st.success() => {}
                other => tracing::warn!(?args, ?other, "benchmark child did not finish cleanly"),
            }
        }
        match kahawai_media::bench::load(&cache) {
            Some(measured) => {
                tracing::info!(
                    encoders = measured.encoders.len(),
                    "local encoder speeds measured (HUB-36)"
                );
                registry.set_local_bench(measured);
            }
            None => tracing::warn!("local benchmark child wrote no usable cache"),
        }
    });
}

async fn run_hub_inner(
    cfg: config::HubConfig,
    local_mediahost: Option<config::MediahostConfig>,
    // The file SIGHUP re-reads (NFR-6). None when defaults were used.
    config_path: Option<PathBuf>,
) -> Result<()> {
    let ca = Arc::new(kahawai_hub::pki::HubCa::load_or_create(
        &kahawai_hub::pki::pki_dir(&cfg.data_dir),
    )?);
    let db = kahawai_hub::db::open(&cfg.data_dir).await?;
    // One-time after migration 0046: point migrated OCR rows at their
    // parent stream rows (idempotent, cheap when nothing is pending).
    kahawai_hub::tracks::backfill_derived_from(&db).await?;
    // The satellites table IS the mTLS allowlist (SEC-5): load it, then
    // the registry keeps it in sync on approve/delete.
    let allowed = kahawai_transport::mtls::AllowedCerts::default();
    let registry = Arc::new(kahawai_hub::registry::Registry::new(
        db.clone(),
        allowed.clone(),
    ));
    let admitted = registry.load_allowlist().await?;
    tracing::info!(admitted, "mTLS allowlist loaded");
    // HUB-36 phase 4: what the fleet has been measured to achieve, so a
    // hub restart does not throw away the learning and start guessing
    // from benchmarks again.
    match registry.load_pace().await {
        Ok(n) => tracing::info!(classes = n, "measured pace loaded"),
        Err(e) => tracing::warn!(error = format!("{e:#}"), "pace table unreadable"),
    }
    // HUB-36: the hub is an executor too (an encode with no fleet stays
    // local), so it measures itself on the same cache-but-verify terms
    // as a satellite — off the startup path, published when it lands.
    spawn_local_benchmark(cfg.data_dir.join("benchmarks.json"), registry.clone());
    let auth = Arc::new(kahawai_hub::auth::Auth::new(db.clone(), &cfg.data_dir).await?);
    let sessions = Arc::new(
        kahawai_hub::sessions::Sessions::with_limits(
            cfg.data_dir.join("sessions"),
            cfg.max_sessions_per_user,
            std::time::Duration::from_secs(90),
        )
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
    let subtitles = Arc::new(
        kahawai_hub::subtitles::Subtitles::new(cfg.data_dir.join("subtitles"))
            .with_provider_config(kahawai_hub::opensubtitles::ProviderConfig {
                api_key: cfg.subtitles.opensubtitles.api_key.clone(),
            }),
    );
    // HUB-32c: idle OCR sweep — every image subtitle track grows a text
    // row eventually, without anyone pressing the button.
    subtitles.spawn_ocr_sweep(registry.clone(), sessions.clone());
    let enricher = Arc::new(kahawai_hub::enrich::Enricher::new(cfg.data_dir.clone()));
    // HUB-9: local .nfo files are read over the byte plane, like artwork.
    enricher.attach_sessions(sessions.clone());
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
        // Same reason as run_mediahost. Ranks are process-global, so in
        // this binary the demotion is shared with the co-hosted
        // transcoder — the lean satellite binaries keep them apart.
        let _ = kahawai_media::demote_elements(&mh.demote_decoders);
        tokio::spawn(async move {
            if let Err(e) =
                kahawai_mediahost::run_local(mh.collections, mh.rescan_minutes, &state_dir, tx, rx)
                    .await
            {
                tracing::error!(error = format!("{e:#}"), "in-process mediahost exited");
            }
        });
        tracing::info!("in-process mediahost started (AR-5)");
    }

    let proxy_trust = Arc::new(
        kahawai_hub::proxy::ProxyTrust::parse(&cfg.trusted_proxies)
            .context("hub.trusted_proxies")?,
    );
    let net = kahawai_hub::api::NetOptions {
        proxy_trust: proxy_trust.clone(),
        cors_origins: cfg.cors_origins.clone(),
        metrics_token: cfg.metrics_token.clone().filter(|t| !t.is_empty()),
    };
    match cfg.metrics_token.as_deref() {
        Some(t) if !t.is_empty() => tracing::info!("/metrics enabled (hub.metrics_token)"),
        _ => tracing::info!("/metrics disabled — set hub.metrics_token to enable scraping"),
    }
    // NFR-6 online reload: SIGHUP re-reads the config file and adopts the
    // settings that can change under a running process. Everything else —
    // listeners, data_dir, cert SANs — is structural: it decides how the
    // sockets were bound or what is already on disk, so it is reported as
    // ignored rather than half-applied.
    {
        let proxy_trust = proxy_trust.clone();
        let config_path = config_path.clone();
        tokio::spawn(async move {
            let Ok(mut hup) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            else {
                return;
            };
            while hup.recv().await.is_some() {
                match config::load(config_path.as_deref()) {
                    Ok((fresh, _)) => match proxy_trust.reload(&fresh.hub.trusted_proxies) {
                        Ok(n) => tracing::info!(
                            trusted_proxies = n,
                            "config reloaded (listeners, data_dir and cert SANs need a restart)"
                        ),
                        Err(e) => tracing::warn!(
                            error = format!("{e:#}"),
                            "config reload rejected; keeping the running settings"
                        ),
                    },
                    Err(e) => tracing::warn!(
                        error = format!("{e:#}"),
                        "config reload failed to parse; keeping the running settings"
                    ),
                }
            }
        });
    }
    let api = kahawai_hub::api::router(
        registry.clone(),
        auth,
        sessions.clone(),
        Arc::new(svc.clone()),
        subtitles.clone(),
        artwork,
        enricher.clone(),
        net,
    );
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
