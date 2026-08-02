//! Shared runtime for the kahawai binaries. One package, four bins —
//! `kahawai` (everything, incl. all-in-one), `kahawai-hub`,
//! `kahawai-mediahost`, `kahawai-transcoder` — each built with only the
//! features (and therefore dependencies) its module needs: a satellite
//! binary carries no SQLite, no axum, no Tesseract. The `ocr` feature
//! is a leftover licensing switch in name only (subtile-ocr was never
//! linked); it now just gates the Tesseract linkage for hub builds.

use std::path::PathBuf;
#[cfg(feature = "hub")]
use std::sync::Arc;
#[cfg(feature = "hub")]
use std::time::Duration;

#[cfg(any(feature = "hub", feature = "transcoder"))]
use anyhow::Context;
use anyhow::Result;

pub mod calibrate;
pub mod config;

pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
}

/// Load config and log where it came from — every binary starts here.
pub fn load_config(path: Option<&std::path::Path>) -> Result<(config::Config, Option<PathBuf>)> {
    let (cfg, used) = config::load(path)?;
    match &used {
        Some(p) => tracing::info!(config = %p.display(), "loaded config"),
        None => tracing::info!("no config file found; using built-in defaults"),
    }
    Ok((cfg, used))
}

/// The per-session pipeline worker (§1.1 crash isolation), spawned by
/// the hub and the transcoder as `<current_exe> remux-worker ...` — so
/// every binary that supervises sessions carries this arm.
#[cfg(any(feature = "hub", feature = "transcoder"))]
#[derive(clap::Args)]
pub struct WorkerArgs {
    pub socket: PathBuf,
    pub out_dir: PathBuf,
    pub size: u64,
    #[arg(long, default_value = "off")]
    pub video: String,
    #[arg(long, default_value = "off")]
    pub audio: String,
    #[arg(long, default_value_t = 0)]
    pub audio_track: usize,
    #[arg(long, default_value_t = 0)]
    pub video_track: usize,
    #[arg(long, default_value_t = 0)]
    pub start_ms: u64,
    #[arg(long)]
    pub sink: Option<String>,
    /// Additional parts of a split source, in timeline order, as
    /// `<socket>:<size>`. The positional socket/size is part one.
    #[arg(long = "part")]
    pub parts: Vec<String>,
    /// HUB-15 encode parameters; absent = historical fixed values.
    #[arg(long)]
    pub video_kbps: Option<u32>,
    #[arg(long)]
    pub max_height: Option<u32>,
    #[arg(long)]
    pub max_channels: Option<u32>,
    /// HUB-15a: tone-map HDR to SDR in the video encode chain.
    #[arg(long)]
    pub tone_map: bool,
    /// HUB-32b: burn this image subtitle track (e{n}) into the picture.
    #[arg(long)]
    pub burn_sub: Option<usize>,
    /// Display sets to burn, extracted by the mediahost. Present for
    /// every dispatched session — a worker cannot walk the source
    /// index itself (every read crosses the byte plane).
    #[arg(long)]
    pub burn_sets: Option<PathBuf>,
    /// HUB-15b encode targets + segment container; unknown values fall
    /// back to the legacy h264/aac/ts.
    #[arg(long, default_value = "h264")]
    pub video_codec: String,
    #[arg(long, default_value = "aac")]
    pub audio_codec: String,
    #[arg(long, default_value = "ts")]
    pub container: String,
}

/// HUB-36: measure this box's encoders and write the benchmark cache,
/// then exit. Runs as a CHILD PROCESS for the same reason pipelines do
/// (§1.1): this is GStreamer work, and GStreamer work takes processes
/// down. Measured live — svtav1enc on the J5005 killed the transcoder
/// outright mid-benchmark, silently, taking a serving satellite with
/// it. A dead child is a missing measurement; a dead satellite is an
/// outage.
#[cfg(any(feature = "hub", feature = "transcoder"))]
pub fn run_benchmark(
    cfg: &config::Config,
    cache: PathBuf,
    only: Option<String>,
    tonemap: bool,
) -> Result<()> {
    kahawai_media::demote_elements(&cfg.transcoder.demote_decoders)?;
    let elements: Vec<&str> = match &only {
        // One element per process: a segfault costs that measurement,
        // never the ones after it.
        Some(el) => vec![el.as_str()],
        None => [
            kahawai_media::remux::h264_encoder(),
            kahawai_media::remux::hevc_encoder(),
            kahawai_media::remux::av1_encoder(),
        ]
        .into_iter()
        .flatten()
        .collect(),
    };
    kahawai_media::bench::measure_into(&elements, tonemap, &cache);
    Ok(())
}

/// The elements a benchmark run would measure, in order — the parent
/// needs the list to spawn one child each.
#[cfg(any(feature = "hub", feature = "transcoder"))]
pub fn benchmark_elements() -> Vec<String> {
    [
        kahawai_media::remux::h264_encoder(),
        kahawai_media::remux::hevc_encoder(),
        kahawai_media::remux::av1_encoder(),
    ]
    .into_iter()
    .flatten()
    .map(str::to_string)
    .collect()
}

#[cfg(any(feature = "hub", feature = "transcoder"))]
pub fn run_remux_worker(cfg: &config::Config, w: WorkerArgs) -> Result<()> {
    // Die WITH the supervisor: kill_on_drop only fires inside a
    // living parent, so a hub/transcoder restart used to orphan
    // pipeline workers indefinitely (one survived three days).
    // PDEATHSIG is the kernel's guarantee; the getppid check
    // closes the race where the parent died before prctl ran.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        if libc::getppid() == 1 {
            anyhow::bail!("supervisor already gone; not starting");
        }
    }
    // Blocking by design: this process exists only for the pipeline.
    kahawai_media::demote_elements(&cfg.transcoder.demote_decoders)?;
    let mut all = vec![(w.socket, w.size)];
    for p in &w.parts {
        let (sock, sz) = p.rsplit_once(':').context("--part wants <socket>:<size>")?;
        all.push((PathBuf::from(sock), sz.parse().context("--part size")?));
    }
    let plan = kahawai_media::remux::RemuxPlan {
        video: kahawai_media::worker::parse_mode(&w.video),
        audio: kahawai_media::worker::parse_mode(&w.audio),
        audio_track: w.audio_track,
        video_track: w.video_track,
        video_kbps: w.video_kbps,
        max_height: w.max_height,
        max_channels: w.max_channels,
        tone_map: w.tone_map,
        burn_subtitle: w.burn_sub,
        video_codec: kahawai_media::remux::VideoTarget::from_str(&w.video_codec),
        audio_codec: kahawai_media::remux::AudioTarget::from_str(&w.audio_codec),
        segment_format: kahawai_media::remux::SegmentFormat::from_str(&w.container),
    };
    kahawai_media::worker::run_parts(
        &all,
        &w.out_dir,
        plan,
        w.start_ms,
        w.sink.as_deref(),
        w.burn_sets.as_deref(),
    )
}

#[cfg(feature = "transcoder")]
pub async fn run_transcoder(cfg: &config::Config) -> Result<()> {
    kahawai_transcoder::run(
        &cfg.transcoder.hub,
        &cfg.transcoder.state_dir,
        &cfg.transcoder.name,
        cfg.transcoder.max_sessions,
        std::env::current_exe().ok(),
    )
    .await
}

/// Environment checks (OPS-3): shared GStreamer inventory plus per-module
/// filesystem and clock checks from the loaded config.
pub fn doctor_checks(cfg: &config::Config) -> Vec<kahawai_media::doctor::Check> {
    use kahawai_media::doctor::Check;
    // The workers apply these before building pipelines, so the doctor
    // must too — otherwise it reports the ranks of a registry no session
    // actually uses (and flags a shadow the config already demoted).
    let _ = kahawai_media::demote_elements(&cfg.transcoder.demote_decoders);
    // HUB-36: whichever role this box plays, its benchmark cache lives
    // beside that role's state; show measured speeds when they exist.
    let bench_cache = [
        cfg.hub.data_dir.join("benchmarks.json"),
        cfg.transcoder.state_dir.join("benchmarks.json"),
    ]
    .into_iter()
    .find(|p| p.exists());
    let mut checks = kahawai_media::doctor::gstreamer_checks(bench_cache.as_deref());

    // Clock sanity: satellites on RTC-less boxes boot in the past (OPS-4).
    let year_2025 = 1735689600;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    checks.push(if now > year_2025 {
        Check::ok("system clock", "sane")
    } else {
        Check::fail(
            "system clock",
            "before 2025 — fix NTP or certificates will fail",
            true,
        )
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
    // Per-module rows only where the module can actually run in this
    // binary — a transcoder build has no hub data dir to fret about.
    // HUB-32c: Tesseract is a runtime dependency of the OCR tier;
    // absence degrades (tier skipped) but must be visible.
    #[cfg(feature = "ocr")]
    checks.push(kahawai_hub::ocr::doctor_check());
    #[cfg(feature = "hub")]
    checks.push(dir_check("hub data dir", &cfg.hub.data_dir, true));
    #[cfg(feature = "mediahost")]
    {
        checks.push(dir_check(
            "mediahost state dir",
            &cfg.mediahost.state_dir,
            true,
        ));
        for c in &cfg.mediahost.collections {
            for root in &c.roots {
                checks.push(dir_check(
                    &format!("collection \"{}\" root", c.name),
                    root,
                    false,
                ));
            }
        }
    }
    #[cfg(not(all(feature = "hub", feature = "mediahost")))]
    let _ = &dir_check;

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

/// OPS-9: the calibration pass, which is TIMED and therefore belongs
/// only here. `startup_checks` shares `doctor_checks` with this command
/// and must stay instant — a boot that spends seconds decoding to warn
/// nobody is reading is the thing the requirement complains about, not
/// a fix for it.
pub fn doctor(
    cfg: &config::Config,
    json: bool,
    calibrate: bool,
    fix: bool,
    config_path: Option<&std::path::Path>,
) -> Result<()> {
    use kahawai_media::doctor::Status;
    let mut checks = doctor_checks(cfg);
    // --fix implies the measurement: writing a demotion nobody measured
    // is exactly the guesswork this replaces.
    let calibration = (calibrate || fix).then(kahawai_media::doctor::calibrate);
    if let Some(c) = &calibration {
        checks.extend(c.checks.iter().cloned());
    }
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
    if let Some(calibration) = calibration.filter(|_| fix) {
        let Some(path) = config_path else {
            anyhow::bail!(
                "--fix needs a config file to write; this box is running on defaults \
                 (create one, or pass --config)"
            );
        };
        if calibration.demote.is_empty() {
            println!("\nnothing to fix: this box's decoder ranks are already right");
        } else {
            let written = calibrate::apply(path, &calibration.demote)?;
            if written.is_empty() {
                println!(
                    "\n{} already says everything this box needs",
                    path.display()
                );
            } else {
                println!("\n{}:", path.display());
                for w in &written {
                    println!(
                        "  [{}] demote_decoders += {}  ({})",
                        w.section, w.element, w.why
                    );
                }
                println!("restart this box's modules to apply.");
            }
        }
    }
    if kahawai_media::doctor::has_essential_failure(&checks) {
        anyhow::bail!("essential checks failed");
    }
    Ok(())
}

/// Startup gate: log warnings, abort on essential failures (OPS-3).
pub fn startup_checks(cfg: &config::Config) -> Result<()> {
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

#[cfg(feature = "hub")]
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

#[cfg(feature = "hub")]
pub async fn run_hub(cfg: config::HubConfig, config_path: Option<PathBuf>) -> Result<()> {
    run_hub_inner(cfg, None, config_path).await
}

/// AR-5 all-in-one: the hub plus an IN-PROCESS mediahost — module logic
/// unchanged, transport replaced by channels, byte plane replaced by
/// direct file reads. The satellite listener stays up: external
/// mediahosts/transcoders enroll and dial in exactly as in modular mode.
#[cfg(all(feature = "hub", feature = "mediahost"))]
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
#[cfg(feature = "hub")]
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
            benchmark_elements()
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

#[cfg(feature = "hub")]
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
    #[cfg(not(feature = "mediahost"))]
    anyhow::ensure!(
        local_mediahost.is_none(),
        "this binary was built without the in-process mediahost (feature `mediahost`)"
    );
    #[cfg(feature = "mediahost")]
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

#[cfg(feature = "mediahost")]
pub async fn run_mediahost(cfg: config::MediahostConfig) -> Result<()> {
    // Before any discovery runs: what the scan records is whatever
    // decoder GStreamer autoplugs, so this list is what keeps the
    // library's view of a stream from being narrower than playback's.
    kahawai_media::demote_elements(&cfg.demote_decoders)?;
    kahawai_mediahost::run(
        &cfg.hub,
        &cfg.state_dir,
        &cfg.name,
        cfg.collections,
        cfg.rescan_minutes,
    )
    .await
}
