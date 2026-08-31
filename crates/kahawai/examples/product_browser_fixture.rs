//! Hermetic all-in-one fixture for the CI-9/CI-10 browser gate.
//!
//! The parent owns disposable media and data directories and supervises a
//! child copy of this executable. The child runs the real all-in-one entry
//! point; the separate loopback control listener can crash/restart it without
//! adding fixture routes to the product API.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use clap::{Parser, Subcommand};
use kahawai_core::media::CollectionConfig;
use kahawai_runtime::{WorkerArgs, config};
use serde_json::json;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

const DEFAULT_PUBLIC: &str = "127.0.0.1:18430";
const DEFAULT_SETUP: &str = "127.0.0.1:18431";
const DEFAULT_SATELLITE: &str = "127.0.0.1:18432";
const DEFAULT_CONTROL: &str = "127.0.0.1:18433";

#[derive(Parser)]
#[command(name = "product_browser_fixture")]
struct WorkerCli {
    #[command(subcommand)]
    command: WorkerCommand,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // one command per short-lived process
enum WorkerCommand {
    #[command(name = "remux-worker")]
    RemuxWorker(WorkerArgs),
    #[command(name = "benchmark")]
    Benchmark {
        #[arg(long)]
        cache: PathBuf,
        #[arg(long)]
        only: Option<String>,
        #[arg(long)]
        tonemap: bool,
    },
}

const ASS_HEADER: &str = r#"[Script Info]
ScriptType: v4.00+
PlayResX: 320
PlayResY: 240

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Liberation Sans,28,&H00FFFFFF,&H000000FF,&H00000000,&H80000000,0,0,0,0,100,100,0,0,1,2,1,2,10,10,18,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
"#;

#[derive(Clone)]
struct ChildArgs {
    root: PathBuf,
    web_dir: PathBuf,
    public: SocketAddr,
    setup: SocketAddr,
    satellite: SocketAddr,
}

struct Supervisor {
    child: Mutex<Child>,
    args: ChildArgs,
}

fn address(name: &str, default: &str) -> Result<SocketAddr> {
    std::env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .parse()
        .with_context(|| format!("invalid {name}"))
}

fn spawn_child(args: &ChildArgs) -> Result<Child> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("serve")
        .arg(&args.root)
        .arg(&args.web_dir)
        .arg(args.public.to_string())
        .arg(args.setup.to_string())
        .arg(args.satellite.to_string())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command.spawn().context("starting all-in-one fixture child")
}

async fn public_ready(state: &Supervisor) -> bool {
    {
        let mut child = state.child.lock().await;
        if child.try_wait().ok().flatten().is_some() {
            return false;
        }
    }
    tokio::net::TcpStream::connect(state.args.public)
        .await
        .is_ok()
}

async fn ready(State(state): State<Arc<Supervisor>>) -> impl IntoResponse {
    if public_ready(&state).await {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "starting")
    }
}

async fn restart(State(state): State<Arc<Supervisor>>) -> impl IntoResponse {
    let before;
    let after;
    {
        let mut child = state.child.lock().await;
        before = child.id();
        if let Err(error) = child.kill().await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("stopping child: {error}") })),
            );
        }
        let _ = child.wait().await;
        match spawn_child(&state.args) {
            Ok(fresh) => {
                after = fresh.id();
                *child = fresh;
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("starting child: {error:#}") })),
                );
            }
        }
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !public_ready(&state).await {
        if tokio::time::Instant::now() >= deadline {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                Json(json!({ "error": "restarted child did not become ready" })),
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    (
        StatusCode::OK,
        Json(json!({ "before": before, "after": after })),
    )
}

fn generate_media(root: &Path) -> Result<()> {
    let movies = root.join("media/movies");
    let hidden = root.join("media/hidden");
    std::fs::create_dir_all(&movies)?;
    std::fs::create_dir_all(&hidden)?;

    let required = ["x264enc", "x265enc", "matroskamux", "mp4mux"];
    anyhow::ensure!(
        kahawai_media::testutil::elements_available(&required),
        "browser fixture requires GStreamer elements: {}",
        required.join(", ")
    );
    anyhow::ensure!(
        kahawai_media::remux::aac_encoder().is_some(),
        "browser fixture requires a verified AAC encoder"
    );
    let h264 = kahawai_media::remux::h264_encoder()
        .context("browser fixture requires a verified H.264 encoder")?;

    let direct = movies.join("Direct Fixture (2026).mp4");
    kahawai_media::testutil::render_h264_aac_mp4(&direct);
    kahawai_media::testutil::render_h264_aac_mkv(&movies.join("Remux Fixture (2026).mkv"));
    kahawai_media::testutil::render_pq_hevc_mkv(&movies.join("Transcode Fixture (2026).mkv"));
    kahawai_media::testutil::render_h264_ass_mkv(
        &movies.join("Subtitle Fixture (2026).mkv"),
        ASS_HEADER,
        &[(250, 1_750, "CI subtitle is visible".into())],
    );
    std::fs::copy(direct, hidden.join("Hidden Fixture (2026).mp4"))?;

    let cache = root.join("hub/benchmarks.json");
    let measured = kahawai_media::bench::measure_into(&[h264], false, &cache);
    anyhow::ensure!(
        measured.encoder_ready(h264),
        "H.264 encoder {h264} did not complete its browser-fixture benchmark"
    );
    Ok(())
}

async fn serve(args: ChildArgs) -> Result<()> {
    let mut cfg = config::Config::default();
    cfg.hub.bind = args.public;
    cfg.hub.setup_bind = args.setup;
    cfg.hub.satellite_bind = args.satellite;
    cfg.hub.public_url = Some(format!("http://{}", args.public));
    cfg.hub.hostnames = vec!["localhost".into(), "127.0.0.1".into()];
    cfg.hub.data_dir = args.root.join("hub");
    cfg.mediahost.state_dir = args.root.join("mediahost");
    cfg.mediahost.name = "browser-fixture".into();
    cfg.mediahost.detect_segments = false;
    cfg.mediahost.rescan_minutes = 0;
    cfg.mediahost.collections = vec![
        CollectionConfig {
            name: "movies".into(),
            media_type: "movies".into(),
            roots: vec![args.root.join("media/movies")],
        },
        CollectionConfig {
            name: "hidden".into(),
            media_type: "movies".into(),
            roots: vec![args.root.join("media/hidden")],
        },
    ];
    cfg.all_in_one.transcoder = true;
    kahawai::run_all_in_one(cfg, None, Some(args.web_dir)).await
}

fn child_args(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<ChildArgs> {
    let root = PathBuf::from(args.next().context("missing fixture root")?);
    let web_dir = PathBuf::from(args.next().context("missing web directory")?);
    let public = args
        .next()
        .context("missing public address")?
        .to_string_lossy()
        .parse()?;
    let setup = args
        .next()
        .context("missing setup address")?
        .to_string_lossy()
        .parse()?;
    let satellite = args
        .next()
        .context("missing satellite address")?
        .to_string_lossy()
        .parse()?;
    anyhow::ensure!(args.next().is_none(), "unexpected fixture argument");
    Ok(ChildArgs {
        root,
        web_dir,
        public,
        setup,
        satellite,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    kahawai_runtime::init_tracing();
    match std::env::args_os().nth(1).as_deref() {
        Some(command)
            if command == std::ffi::OsStr::new("remux-worker")
                || command == std::ffi::OsStr::new("benchmark") =>
        {
            let cfg = config::Config::default();
            return match WorkerCli::parse().command {
                WorkerCommand::RemuxWorker(args) => kahawai_runtime::run_remux_worker(&cfg, args),
                WorkerCommand::Benchmark {
                    cache,
                    only,
                    tonemap,
                } => kahawai_runtime::run_benchmark(&cfg, cache, only, tonemap),
            };
        }
        _ => {}
    }

    let mut raw = std::env::args_os().skip(1);
    if raw.next().as_deref() == Some(std::ffi::OsStr::new("serve")) {
        return serve(child_args(raw)?).await;
    }

    let root = tempfile::Builder::new()
        .prefix("kahawai-browser-")
        .tempdir()
        .context("creating browser fixture directory")?;
    generate_media(root.path())?;

    let args = ChildArgs {
        root: root.path().to_path_buf(),
        web_dir: std::env::var_os("KAHAWAI_E2E_WEB_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("web/dist")),
        public: address("KAHAWAI_E2E_PUBLIC", DEFAULT_PUBLIC)?,
        setup: address("KAHAWAI_E2E_SETUP", DEFAULT_SETUP)?,
        satellite: address("KAHAWAI_E2E_SATELLITE", DEFAULT_SATELLITE)?,
    };
    let control = address("KAHAWAI_E2E_CONTROL", DEFAULT_CONTROL)?;
    let state = Arc::new(Supervisor {
        child: Mutex::new(spawn_child(&args)?),
        args,
    });
    let app = axum::Router::new()
        .route("/ready", get(ready))
        .route("/restart", post(restart))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(control).await?;
    println!("browser fixture control listening on http://{control}/ready");
    let result = axum::serve(listener, app).await;
    drop(root);
    result.context("serving browser fixture control")
}
