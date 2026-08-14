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

pub async fn init_admin(cfg: config::HubConfig) -> Result<()> {
    #[cfg(not(unix))]
    anyhow::bail!("initial-admin CLI currently requires a Unix local socket");
    #[cfg(unix)]
    {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let socket = bootstrap_socket_path(&cfg.data_dir);
        let mut stream = tokio::net::UnixStream::connect(&socket)
            .await
            .with_context(|| {
                format!(
                    "connecting to {} (start the hub in first-run mode first)",
                    socket.display()
                )
            })?;
        eprint!("Admin username: ");
        let mut username = String::new();
        std::io::stdin().read_line(&mut username)?;
        let password = rpassword::prompt_password("Admin password: ")?;
        let confirm = rpassword::prompt_password("Confirm password: ")?;
        anyhow::ensure!(password == confirm, "passwords do not match");
        let request = serde_json::json!({
            "username": username.trim(),
            "password": password,
        });
        stream.write_all(request.to_string().as_bytes()).await?;
        stream.write_all(b"\n").await?;
        let mut response = String::new();
        tokio::io::BufReader::new(stream)
            .read_line(&mut response)
            .await?;
        let response: serde_json::Value = serde_json::from_str(&response)
            .context("invalid response from the hub bootstrap socket")?;
        anyhow::ensure!(
            response["ok"].as_bool() == Some(true),
            "{}",
            response["error"].as_str().unwrap_or("setup failed")
        );
        println!("initial administrator created; sign in through the normal hub URL");
        Ok(())
    }
}

#[cfg(unix)]
fn bootstrap_control_dir(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("control")
}

#[cfg(unix)]
fn bootstrap_socket_path(data_dir: &std::path::Path) -> PathBuf {
    bootstrap_control_dir(data_dir).join("bootstrap.sock")
}

#[cfg(unix)]
async fn bind_bootstrap_socket(
    data_dir: &std::path::Path,
) -> Result<(tokio::net::UnixListener, PathBuf)> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    std::fs::create_dir_all(data_dir)?;
    let control_dir = bootstrap_control_dir(data_dir);
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    if let Err(e) = builder.create(&control_dir)
        && e.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(e.into());
    }
    let metadata = std::fs::symlink_metadata(&control_dir)?;
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "bootstrap control path {} is not a directory",
        control_dir.display()
    );
    // Existing installations may have created this directory with a broader
    // mode. Restrict it before bind: unlike chmodding the socket afterward,
    // this leaves no interval in which another local user can connect.
    std::fs::set_permissions(&control_dir, std::fs::Permissions::from_mode(0o700))?;

    let path = bootstrap_socket_path(data_dir);
    if path.exists() {
        if tokio::net::UnixStream::connect(&path).await.is_ok() {
            anyhow::bail!("bootstrap socket {} is already in use", path.display());
        }
        std::fs::remove_file(&path)?;
    }
    let listener = tokio::net::UnixListener::bind(&path)?;
    // Defense in depth. The mode transition is safe because the containing
    // directory was atomically created (or restricted) to 0700 before bind.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok((listener, path))
}

#[cfg(unix)]
async fn serve_bootstrap_socket(
    listener: tokio::net::UnixListener,
    path: PathBuf,
    auth: Arc<kahawai_hub::auth::Auth>,
) {
    loop {
        tokio::select! {
            _ = auth.wait_setup_complete() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let auth = auth.clone();
                    tokio::spawn(async move {
                        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
                        let (read, mut write) = stream.into_split();
                        let mut line = String::new();
                        let result = tokio::io::BufReader::new(read)
                            .take(16 * 1024)
                            .read_line(&mut line)
                            .await
                            .map_err(anyhow::Error::from)
                            .and_then(|n| {
                                anyhow::ensure!(n > 0 && n < 16 * 1024, "invalid setup request");
                                let body: serde_json::Value = serde_json::from_str(&line)?;
                                let username = body["username"].as_str().context("username required")?;
                                let password = body["password"].as_str().context("password required")?;
                                Ok((username.to_owned(), password.to_owned()))
                            });
                        let response = match result {
                            Ok((username, password)) => match auth.complete_setup(&username, &password).await {
                                Ok(_) => serde_json::json!({"ok": true}),
                                Err(e) => {
                                    if let kahawai_hub::auth::CompleteSetupError::Internal(source) = &e {
                                        tracing::error!(
                                            error = format!("{source:#}"),
                                            "initial-admin setup failed"
                                        );
                                    }
                                    serde_json::json!({"ok": false, "error": e.to_string()})
                                }
                            },
                            Err(e) => serde_json::json!({"ok": false, "error": e.to_string()}),
                        };
                        let _ = write.write_all(response.to_string().as_bytes()).await;
                        let _ = write.write_all(b"\n").await;
                    });
                }
                Err(e) => {
                    tracing::error!(error = %e, "bootstrap socket failed");
                    break;
                }
            }
        }
    }
    drop(listener);
    let control_dir = path.parent().map(std::path::Path::to_path_buf);
    let _ = std::fs::remove_file(path);
    if let Some(control_dir) = control_dir {
        let _ = std::fs::remove_dir(control_dir);
    }
}

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
    run_hub_inner(cfg, None, false, config_path).await
}

/// AR-5 all-in-one: the hub plus an IN-PROCESS mediahost — module logic
/// unchanged, transport replaced by channels, byte plane replaced by
/// direct file reads. The satellite listener stays up: external
/// mediahosts/transcoders enroll and dial in exactly as in modular mode.
pub async fn run_all_in_one(cfg: config::Config, config_path: Option<PathBuf>) -> Result<()> {
    // An empty in-process mediahost is useful when this process supplies
    // the hub (and optionally local transcoding) while collections live on
    // external mediahosts. The empty engine stays connected and idle.
    run_hub_inner(
        cfg.hub,
        Some(cfg.mediahost),
        cfg.all_in_one.transcoder,
        config_path,
    )
    .await
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
    local_transcoder: bool,
    // The file SIGHUP re-reads (NFR-6). None when defaults were used.
    config_path: Option<PathBuf>,
) -> Result<()> {
    config::validate_hub_binds(&cfg)?;
    let ca = Arc::new(kahawai_hub::pki::HubCa::load_or_create(
        &kahawai_hub::pki::pki_dir(&cfg.data_dir),
    )?);
    let db = kahawai_hub::db::open(&cfg.data_dir).await?;
    // The satellites table IS the mTLS allowlist (SEC-5): load it, then
    // the registry keeps it in sync on approve/delete.
    let allowed = kahawai_transport::mtls::AllowedCerts::default();
    let registry = Arc::new(
        kahawai_hub::registry::Registry::new(db.clone(), allowed.clone())
            .with_local_video_executor(local_transcoder),
    );
    let admitted = registry.load_allowlist().await?;
    tracing::info!(admitted, "mTLS allowlist loaded");
    // HUB-36 phase 4: what the fleet has been measured to achieve, so a
    // hub restart does not throw away the learning and start guessing
    // from benchmarks again.
    match registry.load_pace().await {
        Ok(n) => tracing::info!(classes = n, "measured pace loaded"),
        Err(e) => tracing::warn!(error = format!("{e:#}"), "pace table unreadable"),
    }
    if local_transcoder {
        // HUB-36: AIO's full local transcoder competes on the same measured
        // footing as satellites. Plain hub never enters this branch: its
        // local worker stops at remux and audio-only transcode.
        spawn_local_benchmark(cfg.data_dir.join("benchmarks.json"), registry.clone());
    } else {
        tracing::info!("local video transcoder disabled; hub retains remux and audio transcode");
    }
    let auth = Arc::new(kahawai_hub::auth::Auth::new(db.clone(), &cfg.data_dir).await?);
    let setup_url = format!("http://{}", cfg.setup_bind);
    if auth.setup_required() {
        let setup_listener = tokio::net::TcpListener::bind(cfg.setup_bind)
            .await
            .with_context(|| format!("binding local setup UI on {}", cfg.setup_bind))?;
        let setup_api = kahawai_hub::api::setup_router(auth.clone());
        let setup_done = auth.clone();
        tokio::spawn(async move {
            if let Err(e) = axum::serve(setup_listener, setup_api)
                .with_graceful_shutdown(async move { setup_done.wait_setup_complete().await })
                .await
            {
                tracing::error!(error = %e, "local setup UI failed");
            }
        });
        #[cfg(unix)]
        {
            let (listener, path) = bind_bootstrap_socket(&cfg.data_dir).await?;
            tokio::spawn(serve_bootstrap_socket(listener, path, auth.clone()));
        }
        println!(
            "\n  First run: open {setup_url} on the hub, use an SSH tunnel,\n  or run `kahawai hub init-admin`.\n"
        );
    } else {
        #[cfg(unix)]
        {
            let control_dir = bootstrap_control_dir(&cfg.data_dir);
            let _ = std::fs::remove_file(bootstrap_socket_path(&cfg.data_dir));
            let _ = std::fs::remove_dir(control_dir);
            // Clean up a stale pre-control-directory socket after an upgrade.
            let _ = std::fs::remove_file(cfg.data_dir.join("bootstrap.sock"));
        }
    }
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
    #[cfg(feature = "ocr")]
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
        sessions.set_local_source(LOCAL_ID, move |collection, root_token, path| {
            kahawai_mediahost::serve::resolve_rel(&cols, collection, root_token, path)
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
        setup_url: Some(setup_url),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn unused_loopback_addr() -> std::net::SocketAddr {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
    }

    #[tokio::test]
    async fn directly_constructed_config_rejects_listener_port_overlap() {
        let mut cfg = config::Config::default();
        let port = unused_loopback_addr().port();
        cfg.hub.bind = format!("0.0.0.0:{port}").parse().unwrap();
        cfg.hub.setup_bind = format!("127.0.0.1:{port}").parse().unwrap();
        let error = run_all_in_one(cfg, None).await.unwrap_err();
        assert!(error.to_string().contains("different port"), "{error:#}");
    }

    #[tokio::test]
    async fn all_in_one_starts_without_local_collections_or_transcoding() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config::Config::default();
        cfg.hub.bind = unused_loopback_addr();
        cfg.hub.setup_bind = unused_loopback_addr();
        cfg.hub.satellite_bind = unused_loopback_addr();
        cfg.hub.data_dir = dir.path().join("hub");
        cfg.mediahost.state_dir = dir.path().join("mediahost");
        cfg.all_in_one.transcoder = false;
        assert!(cfg.mediahost.collections.is_empty());

        let api_addr = cfg.hub.bind;
        let setup_addr = cfg.hub.setup_bind;
        let server = tokio::spawn(run_all_in_one(cfg, None));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if server.is_finished() {
                let result = server.await.unwrap();
                panic!("all-in-one exited before serving: {result:?}");
            }
            if tokio::net::TcpStream::connect(api_addr).await.is_ok() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "all-in-one did not start its client API"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let body = r#"{"username":"browser","password":"hunter22222"}"#;
        let mut local = tokio::net::TcpStream::connect(setup_addr).await.unwrap();
        local
            .write_all(
                format!(
                    "POST /api/v1/setup HTTP/1.1\r\nHost: {setup_addr}\r\nOrigin: http://{setup_addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        local.read_to_end(&mut response).await.unwrap();
        assert!(
            String::from_utf8_lossy(&response).starts_with("HTTP/1.1 204"),
            "{}",
            String::from_utf8_lossy(&response)
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::net::TcpStream::connect(setup_addr).await.is_ok() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "setup UI stayed open"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(tokio::net::TcpStream::connect(api_addr).await.is_ok());

        server.abort();
        let _ = server.await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn private_socket_creates_one_admin_and_then_disappears() {
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let dir = tempfile::tempdir().unwrap();
        let db = kahawai_hub::db::open(dir.path()).await.unwrap();
        let auth = Arc::new(
            kahawai_hub::auth::Auth::new(db.clone(), dir.path())
                .await
                .unwrap(),
        );
        let control_dir = bootstrap_control_dir(dir.path());
        std::fs::create_dir(&control_dir).unwrap();
        std::fs::set_permissions(&control_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let (listener, path) = bind_bootstrap_socket(dir.path()).await.unwrap();
        assert_eq!(path, control_dir.join("bootstrap.sock"));
        assert_eq!(
            std::fs::metadata(&control_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let task = tokio::spawn(serve_bootstrap_socket(listener, path.clone(), auth));
        let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        stream
            .write_all(b"{\"username\":\"local\",\"password\":\"hunter22222\"}\n")
            .await
            .unwrap();
        let mut response = String::new();
        tokio::io::BufReader::new(stream)
            .read_line(&mut response)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response).unwrap()["ok"],
            true
        );
        task.await.unwrap();
        assert!(!path.exists());
        assert!(!control_dir.exists());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users WHERE is_admin=1")
                .fetch_one(&db)
                .await
                .unwrap(),
            1
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bootstrap_control_directory_must_not_be_a_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        symlink(elsewhere.path(), bootstrap_control_dir(dir.path())).unwrap();
        let error = bind_bootstrap_socket(dir.path()).await.unwrap_err();
        assert!(
            error.to_string().contains("is not a directory"),
            "{error:#}"
        );
        assert!(!elsewhere.path().join("bootstrap.sock").exists());
    }
}
