//! Corpus sweep: run every media file in a directory through discovery and
//! the real remux pipeline, validate the output, and report one verdict
//! per file. The pre-flight radar for a library — find the files that will
//! misbehave before a user presses play.
//!
//!   kahawai-sweep <dir> [--full] [--limit N] [--jobs N]
//!
//! Default sweeps the first 48 MiB of each file (seconds per file; a real
//! demux/mux problem shows up immediately). --full feeds entire files.
//! Verdicts: OK, OK(head) — errored after producing segments when fed a
//! truncated head, which healthy files may do — DEGRADED (a stream needs a
//! transcoder and is dropped), SKIP (nothing TS-muxable), FAIL.
//!
//! Each file runs in a child process (`--one`): GStreamer plugins can
//! abort outright on hostile input (a gst-plugins-rs panic in an FFI
//! callback is non-unwinding), and a crash must become that file's FAIL
//! verdict, not the end of the sweep. The parent enforces a watchdog and
//! kills hung children.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const MEDIA_EXTS: &[&str] = &[
    "mkv", "mp4", "m4v", "webm", "avi", "mov", "ts", "m2ts", "ogv", "flac", "mp3", "ogg", "oga",
    "opus", "m4a", "aac", "wav",
];
const HEAD_BYTES: u64 = 48 * 1024 * 1024;
/// In-child pipeline deadline; the parent's watchdog is the backstop.
const PIPELINE_DEADLINE: Duration = Duration::from_secs(120);
const CHILD_WATCHDOG: Duration = Duration::from_secs(180);

#[derive(PartialEq, Clone, Copy)]
enum Verdict {
    Ok,
    OkHead,
    Degraded,
    Skip,
    Fail,
}

impl Verdict {
    fn tag(self) -> &'static str {
        match self {
            Verdict::Ok => "OK      ",
            Verdict::OkHead => "OK(head)",
            Verdict::Degraded => "DEGRADED",
            Verdict::Skip => "SKIP    ",
            Verdict::Fail => "FAIL    ",
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut dir = None;
    let mut full = false;
    let mut limit = usize::MAX;
    let mut jobs = 4usize;
    let mut one = None;
    let mut profile_path: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--full" => full = true,
            "--one" => one = args.next().map(PathBuf::from),
            "--limit" => limit = args.next().and_then(|v| v.parse().ok()).expect("--limit N"),
            "--jobs" => jobs = args.next().and_then(|v| v.parse().ok()).expect("--jobs N"),
            // A CapabilityProfile as JSON (dump the browser's
            // buildProfile() from the console to sweep a real client).
            "--profile" => profile_path = args.next().map(PathBuf::from),
            other => dir = Some(PathBuf::from(other)),
        }
    }
    let profile: kahawai_core::media::CapabilityProfile = match &profile_path {
        Some(p) => serde_json::from_str(&std::fs::read_to_string(p).expect("--profile file"))
            .expect("--profile JSON"),
        None => Default::default(),
    };

    // Child mode: sweep exactly one file, print "<tag>\t<detail>", exit 0.
    if let Some(path) = one {
        kahawai_media::init().expect("gstreamer init");
        let has_ffprobe = std::process::Command::new("ffprobe")
            .arg("-version")
            .output()
            .is_ok();
        let (verdict, detail) = sweep_one(&path, full, has_ffprobe, &profile);
        println!("{}\t{}", verdict.tag().trim_end(), detail);
        return;
    }

    let Some(dir) = dir else {
        eprintln!("usage: kahawai-sweep <dir> [--full] [--limit N] [--jobs N]");
        std::process::exit(2);
    };

    if std::process::Command::new("ffprobe")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("note: ffprobe not found — segment DTS checks skipped");
    }

    let mut files = Vec::new();
    walk(&dir, &mut files);
    files.sort();
    files.truncate(limit);
    eprintln!(
        "sweeping {} files with {} jobs ({})",
        files.len(),
        jobs,
        if full { "full files" } else { "first 48 MiB" }
    );

    let exe = std::env::current_exe().expect("current_exe");
    let next = AtomicUsize::new(0);
    let counts = Mutex::new([0usize; 5]);
    let started = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..jobs.max(1) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = files.get(i) else { break };
                    let (verdict, detail) = sweep_in_child(&exe, path, full);
                    let rel = path.strip_prefix(&dir).unwrap_or(path);
                    println!("{} {} {}", verdict.tag(), rel.display(), detail);
                    counts.lock().unwrap()[verdict as usize] += 1;
                }
            });
        }
    });

    let c = counts.into_inner().unwrap();
    eprintln!(
        "\nswept {} files in {:.0?}: {} ok, {} ok(head), {} degraded, {} skip, {} FAIL",
        files.len(),
        started.elapsed(),
        c[Verdict::Ok as usize],
        c[Verdict::OkHead as usize],
        c[Verdict::Degraded as usize],
        c[Verdict::Skip as usize],
        c[Verdict::Fail as usize],
    );
    if c[Verdict::Fail as usize] > 0 {
        std::process::exit(1);
    }
}

/// Run one file in a child process; crashes and hangs become verdicts.
fn sweep_in_child(exe: &Path, path: &Path, full: bool) -> (Verdict, String) {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--one").arg(path);
    if full {
        cmd.arg("--full");
    }
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (Verdict::Fail, format!("[spawn] {e}")),
    };
    let deadline = Instant::now() + CHILD_WATCHDOG;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return (Verdict::Fail, "[watchdog] killed after 180s".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return (Verdict::Fail, format!("[wait] {e}")),
        }
    };
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        use std::io::Read;
        let _ = stdout.read_to_string(&mut out);
    }
    if !status.success() {
        // Signal or abort: the pipeline (often a plugin bug) took the
        // child down. Exactly what isolation is for.
        return (Verdict::Fail, format!("[crashed] {status}"));
    }
    match out.trim_end().split_once('\t') {
        Some((tag, detail)) => {
            let verdict = match tag {
                "OK" => Verdict::Ok,
                "OK(head)" => Verdict::OkHead,
                "DEGRADED" => Verdict::Degraded,
                "SKIP" => Verdict::Skip,
                _ => Verdict::Fail,
            };
            (verdict, detail.to_string())
        }
        None => (Verdict::Fail, "[protocol] child printed no verdict".into()),
    }
}

/// Serves from an inner source until `budget` bytes have been read in
/// total (seeks are free — the moov probe at the tail must succeed), then
/// reports EOF. Head-sweeps whole libraries in seconds per file.
struct BudgetSource {
    inner: kahawai_media::remux::FileSource,
    remaining: u64,
    exhausted: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl BudgetSource {
    fn new(inner: kahawai_media::remux::FileSource, budget: u64) -> Self {
        Self {
            inner,
            remaining: budget,
            exhausted: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

impl kahawai_media::remux::RemuxSource for BudgetSource {
    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            self.exhausted.store(true, Ordering::Relaxed);
            return Ok(0);
        }
        let cap = (self.remaining as usize).min(buf.len());
        let n = self.inner.read_at(offset, &mut buf[..cap])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| MEDIA_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        {
            out.push(p);
        }
    }
}

fn sweep_one(
    path: &Path,
    full: bool,
    has_ffprobe: bool,
    profile: &kahawai_core::media::CapabilityProfile,
) -> (Verdict, String) {
    // 1. Discovery, as the mediahost scanner would do it.
    let info = match kahawai_media::discover(path, Duration::from_secs(30)) {
        Ok(i) => i,
        Err(e) => return (Verdict::Fail, format!("[discover] {e:#}")),
    };
    let codecs = describe(&info);

    // 2. Negotiate, as the hub would (HUB-14).
    let sp = kahawai_media::negotiate::negotiate(
        profile,
        &info,
        0,
        0,
        true,
        None,
        kahawai_media::remux::tonemap_available(),
        // The sweep reads local files: the burn-in index walk is free.
        true,
        // No OCR store outside the hub: the sweep judges sources, not
        // the hub's caches.
        &[],
        None,
        // Same reasoning as the burn-in walk: the sweep encodes where it
        // reads, so it can burn ASS iff this box has assrender. No user
        // to hold a preference, so it never burns passively.
        kahawai_media::negotiate::AssBurn {
            capable: kahawai_media::remux::ass_burn_available(),
            preferred: false,
        },
        // The sweep runs where it encodes: its own verified encoders
        // are the fleet.
        &kahawai_media::remux::encoder_capabilities()
            .iter()
            .map(|(c, _, _)| c.to_string())
            .collect::<Vec<_>>(),
    );
    let plan = sp.plan;
    if sp.cost == kahawai_media::negotiate::Cost::Unplayable {
        return (Verdict::Skip, format!("[needs transcoder] {codecs}"));
    }
    let cost_tag = match sp.cost {
        kahawai_media::negotiate::Cost::Direct => "direct",
        kahawai_media::negotiate::Cost::Copy => "copy",
        kahawai_media::negotiate::Cost::AudioEncode => "a-enc",
        kahawai_media::negotiate::Cost::VideoEncode => "v-enc",
        kahawai_media::negotiate::Cost::Unplayable => unreachable!(),
    };
    let codecs = format!("[{cost_tag}] {codecs}");
    let video_dropped = !plan.has_video() && !info.video.is_empty();
    let audio_dropped = !plan.has_audio() && !info.audio.is_empty();
    let mut codecs = codecs;
    if plan.video == kahawai_media::remux::StreamMode::Encode {
        codecs = format!("{codecs} [video→h264]");
    }
    if plan.audio == kahawai_media::remux::StreamMode::Encode {
        codecs = format!("{codecs} [audio→aac]");
    }

    // 3. Remux through the real pipeline.
    let out = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => return (Verdict::Fail, format!("[tempdir] {e}")),
    };
    let src = match kahawai_media::remux::FileSource::open(path) {
        Ok(s) => s,
        Err(e) => return (Verdict::Fail, format!("[open] {e}")),
    };
    let budget = BudgetSource::new(src, if full { u64::MAX } else { HEAD_BYTES });
    let truncated = budget.exhausted.clone();
    let job = match kahawai_media::remux::start(out.path(), plan, Box::new(budget)) {
        Ok(j) => j,
        Err(e) => return (Verdict::Fail, format!("[start] {e:#}")),
    };
    let deadline = Instant::now() + PIPELINE_DEADLINE;
    while !job.finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if !job.finished() {
        job.stop();
        return (
            Verdict::Fail,
            format!("[hang] pipeline never finished; {codecs}"),
        );
    }

    // 4. Validate what came out.
    let segments: Vec<PathBuf> = std::fs::read_dir(out.path())
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "ts"))
                .collect()
        })
        .unwrap_or_default();
    let playlist_ok = out.path().join("master.m3u8").exists();

    let truncated = truncated.load(Ordering::Relaxed);
    if let Some(err) = job.failed() {
        // A budget-truncated stream may end in a demux error after healthy
        // output; that is expected. An error before real output is not.
        if truncated && playlist_ok && segments.len() >= 2 {
            // fall through to segment checks, verdict OkHead
        } else {
            let first = err.lines().next().unwrap_or("");
            return (Verdict::Fail, format!("[pipeline] {first} — {codecs}"));
        }
    }
    if !playlist_ok || segments.is_empty() {
        return (Verdict::Fail, format!("[no output] {codecs}"));
    }
    if has_ffprobe && plan.has_video() {
        for seg in &segments {
            let (missing, ooo) = video_dts_defects(seg);
            if missing + ooo > 0 {
                return (
                    Verdict::Fail,
                    format!(
                        "[bad dts] {}: {missing} missing, {ooo} out-of-order — {codecs}",
                        seg.file_name().unwrap_or_default().to_string_lossy()
                    ),
                );
            }
        }
    }
    // Every stream the plan promised must actually be in the output —
    // a silently-dropped track would otherwise sweep as OK.
    if has_ffprobe {
        let mut sorted = segments.clone();
        sorted.sort();
        if let Some(first) = sorted.first() {
            let (has_v, has_a) = segment_stream_kinds(first);
            if plan.has_video() && !has_v {
                return (
                    Verdict::Fail,
                    format!("[missing video] planned but absent — {codecs}"),
                );
            }
            if plan.has_audio() && !has_a {
                return (
                    Verdict::Fail,
                    format!("[missing audio] planned but absent — {codecs}"),
                );
            }
        }
    }

    let dropped = match (video_dropped, audio_dropped) {
        (true, _) => " [video dropped: needs transcoder]",
        (_, true) => " [audio dropped: needs transcoder]",
        _ => "",
    };
    if video_dropped || audio_dropped {
        return (Verdict::Degraded, format!("{codecs}{dropped}"));
    }
    if job.failed().is_some() || truncated {
        (Verdict::OkHead, codecs)
    } else {
        (Verdict::Ok, codecs)
    }
}

fn describe(info: &kahawai_core::media::MediaInfo) -> String {
    let v: Vec<&str> = info.video.iter().map(|s| s.codec.as_str()).collect();
    let a: Vec<&str> = info.audio.iter().map(|s| s.codec.as_str()).collect();
    format!(
        "[{} v={} a={}]",
        info.container.as_deref().unwrap_or("?"),
        if v.is_empty() {
            "-".into()
        } else {
            v.join(",")
        },
        if a.is_empty() {
            "-".into()
        } else {
            a.join(",")
        },
    )
}

/// Which stream kinds a segment actually contains, via ffprobe.
fn segment_stream_kinds(seg: &Path) -> (bool, bool) {
    let Ok(out) = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(seg)
        .output()
    else {
        return (true, true); // ffprobe hiccup: don't fail the file on it
    };
    let text = String::from_utf8_lossy(&out.stdout);
    (text.contains("video"), text.contains("audio"))
}

/// (missing_dts, non_monotonic) video packets per segment, via ffprobe.
fn video_dts_defects(seg: &Path) -> (usize, usize) {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v",
            "-show_entries",
            "packet=dts",
            "-of",
            "csv=p=0",
        ])
        .arg(seg)
        .output();
    let Ok(out) = out else { return (0, 0) };
    let (mut missing, mut ooo) = (0, 0);
    let mut prev: Option<i64> = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let field = line.trim().trim_end_matches(',');
        if field.is_empty() {
            continue;
        }
        match field.parse::<i64>() {
            Ok(dts) => {
                if prev.is_some_and(|p| dts < p) {
                    ooo += 1;
                }
                prev = Some(dts);
            }
            Err(_) => missing += 1,
        }
    }
    (missing, ooo)
}
