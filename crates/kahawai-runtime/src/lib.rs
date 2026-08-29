//! Plumbing every kahawai binary needs and no role depends on: config,
//! logging, the doctor, the environment gate, and the pipeline worker.
//!
//! It knows nothing of the hub, the mediahost or the transcoder, and
//! that is the point. Cargo unifies features across a build, so a lib
//! shared by all four binaries would link whatever the fattest one asks
//! for — which is how a mediahost ended up carrying Tesseract. The role
//! crates depend on this; it depends on none of them, so no build can
//! put SQLite, axum or an OCR engine into a satellite.
//!
//! Role-specific doctor rows come in through [`doctor_checks`] rather
//! than through feature gates, for the same reason.

use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;

pub mod calibrate;
pub mod config;

/// Which modules this binary can actually run — the doctor only reports
/// rows a role can be judged on.
#[derive(Clone, Copy, Default)]
pub struct Roles {
    pub hub: bool,
    pub mediahost: bool,
    pub transcoder: bool,
    /// AIO may run full local video encode in the hub-supervised worker.
    /// Plain hub's remux/audio-only worker does not require video probing.
    pub local_encode: bool,
}

impl Roles {
    /// Everything, for the all-in-one binary.
    pub const fn all() -> Self {
        Self {
            hub: true,
            mediahost: true,
            transcoder: true,
            local_encode: true,
        }
    }
}

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
    kahawai_media::demote_elements(&cfg.effective_decoder_demotions())?;
    Ok((cfg, used))
}

/// The per-session pipeline worker (§1.1 crash isolation), spawned by
/// the hub and the transcoder as `<current_exe> remux-worker ...` — so
/// every binary that supervises sessions carries this arm.
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
    /// The source is interlaced: deinterlace before the encoder.
    #[arg(long)]
    pub deinterlace: bool,
    /// The pid this worker belongs to. See the PDEATHSIG guard in
    /// [`run_remux_worker`]; absent means the guard cannot check and
    /// leaves the kernel's signal as the only tie.
    #[arg(long)]
    pub supervisor_pid: Option<u32>,
    /// HUB-32b: burn this image subtitle track (e{n}) into the picture.
    #[arg(long)]
    pub burn_sub: Option<usize>,
    /// Display sets to burn, extracted by the mediahost. Present for
    /// every dispatched session — a worker cannot walk the source
    /// index itself (every read crosses the byte plane).
    #[arg(long)]
    pub burn_sets: Option<PathBuf>,
    /// HUB-32a: burn this EMBEDDED text subtitle track (e{n}) into the
    /// picture with libass. A user's SIDECAR .ass burns from
    /// `--burn-ass-file` instead — the demuxer pad is used for embedded
    /// tracks because it is what carries the file's attached fonts.
    #[arg(long)]
    pub burn_ass: Option<usize>,
    #[arg(long)]
    pub burn_ass_file: Option<PathBuf>,
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
fn selected_benchmark_elements<'a>(
    only: Option<&'a str>,
    tonemap: bool,
    available: &[&'a str],
) -> Vec<&'a str> {
    match only {
        Some(element) => vec![element],
        // `benchmark --tonemap` is one isolated piece. Measuring every
        // encoder here made a known-crashing SVT-AV1 probe dump core once in
        // this child and then again in its own `--only` child.
        None if tonemap => Vec::new(),
        None => available.to_vec(),
    }
}

pub fn run_benchmark(
    _cfg: &config::Config,
    cache: PathBuf,
    only: Option<String>,
    tonemap: bool,
) -> Result<()> {
    let available: Vec<&str> = [
        kahawai_media::remux::h264_encoder(),
        kahawai_media::remux::hevc_encoder(),
        kahawai_media::remux::av1_encoder(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let elements = selected_benchmark_elements(only.as_deref(), tonemap, &available);
    kahawai_media::bench::measure_into(&elements, tonemap, &cache);
    Ok(())
}

pub fn run_remux_worker(cfg: &config::Config, w: WorkerArgs) -> Result<()> {
    // Die WITH the supervisor: kill_on_drop only fires inside a
    // living parent, so a hub/transcoder restart used to orphan
    // pipeline workers indefinitely (one survived three days).
    // PDEATHSIG is the kernel's guarantee; the getppid check
    // closes the race where the parent died before prctl ran.
    //
    // Against the supervisor's OWN pid, not against 1. Reparenting to
    // init is what a dead parent looks like on a normal box, but in a
    // container the hub is pid 1 itself — it is the ENTRYPOINT — so
    // every worker it spawned had getppid() == 1 legitimately and this
    // guard refused all of them. The image could not play anything
    // (reported 2026-08-06, reproduced with `docker run --entrypoint sh
    // … kahawai remux-worker …`, where the shell is pid 1 and stands in
    // for the hub).
    #[cfg(target_os = "linux")]
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        if let Some(expected) = w.supervisor_pid
            && libc::getppid() != expected as libc::pid_t
        {
            anyhow::bail!("supervisor already gone; not starting");
        }
    }
    // TC-6 CPU shares. Applied by the worker to ITSELF rather than by
    // the spawner, so the hub and the transcoder share one
    // implementation and neither needs `pre_exec` — and read out of the
    // config this process loads, the same route `demote_decoders`
    // takes, so there is nothing to plumb through two spawn sites.
    //
    // Raising niceness never needs privileges; lowering it does, and a
    // refusal is logged rather than failing a session over a knob.
    if cfg.transcoder.worker_nice != 0 {
        // SAFETY: setpriority on our own process, with a value clamped
        // to the range the kernel accepts.
        let rc = unsafe {
            libc::setpriority(
                libc::PRIO_PROCESS,
                0,
                cfg.transcoder.worker_nice.clamp(-20, 19),
            )
        };
        if rc == 0 {
            tracing::info!(nice = cfg.transcoder.worker_nice, "worker niceness applied");
        } else {
            tracing::warn!(
                nice = cfg.transcoder.worker_nice,
                error = %std::io::Error::last_os_error(),
                "setpriority refused; worker runs at the default priority"
            );
        }
    }
    if cfg.transcoder.worker_threads > 0 {
        kahawai_media::remux::set_encoder_threads(cfg.transcoder.worker_threads);
    }
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
        deinterlace: w.deinterlace,
        burn_subtitle: w.burn_sub,
        burn_ass: w.burn_ass,
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
        w.burn_ass_file.as_deref(),
    )
}

/// Environment checks (OPS-3): shared GStreamer inventory plus per-module
/// filesystem and clock checks from the loaded config.
pub fn doctor_checks(
    cfg: &config::Config,
    roles: Roles,
    extra: Vec<kahawai_media::doctor::Check>,
) -> Vec<kahawai_media::doctor::Check> {
    use kahawai_media::doctor::Check;
    // `load_config` applied the process-global decoder policy before any
    // checker or worker could initialize GStreamer.
    let verify_encoders = roles.transcoder || roles.local_encode;
    // HUB-36: whichever role this box plays, its benchmark cache lives
    // beside that role's state; show measured speeds when they exist.
    let bench_cache = verify_encoders
        .then(|| {
            [
                cfg.hub.data_dir.join("benchmarks.json"),
                cfg.transcoder.state_dir.join("benchmarks.json"),
            ]
            .into_iter()
            .find(|p| p.exists())
        })
        .flatten();
    let mut checks =
        kahawai_media::doctor::gstreamer_checks(bench_cache.as_deref(), verify_encoders);

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
    checks.extend(extra);
    if roles.hub {
        checks.push(dir_check("hub data dir", &cfg.hub.data_dir, true));
    }
    if roles.mediahost {
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
    roles: Roles,
    extra: Vec<kahawai_media::doctor::Check>,
    json: bool,
    calibrate: bool,
    fix: bool,
    config_path: Option<&std::path::Path>,
) -> Result<()> {
    use kahawai_media::doctor::Status;
    let mut checks = doctor_checks(cfg, roles, extra);
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
pub fn startup_checks(
    cfg: &config::Config,
    roles: Roles,
    extra: Vec<kahawai_media::doctor::Check>,
) -> Result<()> {
    use kahawai_media::doctor::Status;
    let checks = doctor_checks(cfg, roles, extra);
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

#[cfg(test)]
mod benchmark_tests {
    use super::*;

    #[test]
    fn tonemap_child_never_runs_encoders() {
        let available = ["h264", "av1"];
        assert!(selected_benchmark_elements(None, true, &available).is_empty());
        assert_eq!(
            selected_benchmark_elements(None, false, &available),
            available
        );
        assert_eq!(
            selected_benchmark_elements(Some("av1"), true, &available),
            ["av1"]
        );
    }
}
