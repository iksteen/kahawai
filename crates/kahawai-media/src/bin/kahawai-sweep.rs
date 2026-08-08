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
/// How long a pipeline may produce nothing before it counts as stuck.
/// Progress, not elapsed time, is what separates a slow disk from a
/// hang — see the wait loop in `sweep_one`.
const PIPELINE_STALL: Duration = Duration::from_secs(120);
/// Backstop for a pipeline that produces forever without finishing.
/// --full feeds whole files, so it earns a longer one.
const fn pipeline_cap(full: bool) -> Duration {
    Duration::from_secs(if full { 1800 } else { 600 })
}
/// The parent's backstop for a child that never answers at all. It has
/// to sit above the child's own cap, or it fails the slow files the
/// child was about to pass.
fn child_watchdog(full: bool) -> Duration {
    pipeline_cap(full) + Duration::from_secs(120)
}

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
    let mut keep: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--full" => full = true,
            "--one" => one = args.next().map(PathBuf::from),
            "--limit" => limit = args.next().and_then(|v| v.parse().ok()).expect("--limit N"),
            "--jobs" => jobs = args.next().and_then(|v| v.parse().ok()).expect("--jobs N"),
            // A CapabilityProfile as JSON (dump the browser's
            // buildProfile() from the console to sweep a real client).
            "--profile" => profile_path = args.next().map(PathBuf::from),
            // Write the segments here instead of a temp dir that is
            // deleted on the way out. --one only: the point is to read
            // the output of a single verdict afterwards.
            "--keep" => keep = args.next().map(PathBuf::from),
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
        let (verdict, detail) = sweep_one(&path, full, has_ffprobe, &profile, keep.as_deref());
        println!("{}\t{}", verdict.tag().trim_end(), detail);
        return;
    }

    let Some(dir) = dir else {
        eprintln!(
            "usage: kahawai-sweep <dir> [--full] [--limit N] [--jobs N]\n       kahawai-sweep --one <file> [--full] [--keep <dir>]"
        );
        std::process::exit(2);
    };

    if std::process::Command::new("ffprobe")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("note: ffprobe not found — the segment stream-kind check is skipped");
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
    let deadline = Instant::now() + child_watchdog(full);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return (
                    Verdict::Fail,
                    format!("[watchdog] killed after {:?}", child_watchdog(full)),
                );
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

/// The tail kept alongside the head, so a container whose index lives at
/// the end (Matroska Cues, an mp4 moov, an AVI idx1) can still be read.
/// Scaled, because an index grows with the recording: a 13-hour 32 GB
/// capture carries a 23 MB moov, which a flat 16 MiB window cut in
/// half — qtdemux then reported a perfectly good file as "contains no
/// playable streams". The window only permits reads, it does not force
/// them, so a larger one costs nothing on files that never seek there.
const TAIL_BYTES: u64 = 16 * 1024 * 1024;
const TAIL_FRACTION: u64 = 256;

/// Serves the file's first `head` bytes and its last `tail` bytes, and
/// reports EOF in between: a truncated file that kept its index.
/// Head-sweeps whole libraries in seconds per file.
///
/// The window is a function of the OFFSET, never of how much has been
/// read already. A cumulative budget looks equivalent and is not: the
/// tail reads this doc has always promised are free were spending the
/// head's allowance, so by the time anything re-read offset 0 — which
/// parsebin's typefind does, after the demuxer has been to the end —
/// it got EOF, and the file failed as "Can't typefind stream" having
/// produced nothing. Five good files failed that way, and WHICH five
/// moved between runs, because a cumulative counter makes the verdict
/// depend on read order.
struct BudgetSource {
    inner: kahawai_media::remux::FileSource,
    head: u64,
    tail: u64,
    exhausted: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl BudgetSource {
    fn new(inner: kahawai_media::remux::FileSource, head: u64, tail: u64) -> Self {
        Self {
            inner,
            head,
            tail,
            exhausted: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

impl kahawai_media::remux::RemuxSource for BudgetSource {
    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let size = self.inner.size();
        // `--full` passes u64::MAX, which makes the head the whole file.
        let head_end = self.head.min(size);
        let end = if offset < head_end {
            head_end
        } else if offset >= size.saturating_sub(self.tail) {
            size
        } else {
            self.exhausted.store(true, Ordering::Relaxed);
            return Ok(0);
        };
        let cap = ((end - offset) as usize).min(buf.len());
        if cap == 0 {
            self.exhausted.store(true, Ordering::Relaxed);
            return Ok(0);
        }
        self.inner.read_at(offset, &mut buf[..cap])
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
    keep: Option<&Path>,
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
        // to hold an order, so it takes the server default — which puts
        // flatten first and therefore never burns passively.
        &kahawai_media::negotiate::AssPolicy {
            burn_capable: kahawai_media::remux::ass_burn_available(),
            ..Default::default()
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
    // The TempDir binding must outlive the pipeline: dropping it wipes
    // the segments the checks below read.
    let tmp;
    let out_dir = match keep {
        Some(d) => match std::fs::create_dir_all(d) {
            Ok(()) => d.to_path_buf(),
            Err(e) => return (Verdict::Fail, format!("[tempdir] {e}")),
        },
        None => match tempfile::tempdir() {
            Ok(d) => {
                tmp = d;
                tmp.path().to_path_buf()
            }
            Err(e) => return (Verdict::Fail, format!("[tempdir] {e}")),
        },
    };
    let src = match kahawai_media::remux::FileSource::open(path) {
        Ok(s) => s,
        Err(e) => return (Verdict::Fail, format!("[open] {e}")),
    };
    let tail = TAIL_BYTES.max(kahawai_media::remux::RemuxSource::size(&src) / TAIL_FRACTION);
    let budget = BudgetSource::new(src, if full { u64::MAX } else { HEAD_BYTES }, tail);
    let truncated = budget.exhausted.clone();
    let job = match kahawai_media::remux::start(&out_dir, plan, Box::new(budget)) {
        Ok(j) => j,
        Err(e) => return (Verdict::Fail, format!("[start] {e:#}")),
    };
    // A deadline counted from the start measures the disk, not the file.
    // Four American Horror Story episodes that swept OK on an idle NAS
    // reported [hang] at four jobs on a busy one — each still writing
    // segments when the clock ran out, and each passing alone in 89 s of
    // a 120 s allowance. So the pipeline may take as long as it keeps
    // producing, and fails when it stops producing; the cap is only
    // there so a livelock still terminates.
    let mut produced = 0;
    let mut progressed = Instant::now();
    let cap = Instant::now() + pipeline_cap(full);
    let stalled = loop {
        if job.finished() {
            break false;
        }
        std::thread::sleep(Duration::from_millis(200));
        let bytes = output_bytes(&out_dir);
        if bytes > produced {
            produced = bytes;
            progressed = Instant::now();
        }
        if progressed.elapsed() > PIPELINE_STALL {
            break true;
        }
        if Instant::now() > cap {
            break true;
        }
    };
    if stalled {
        job.stop();
        let waited = progressed.elapsed().as_secs();
        return (
            Verdict::Fail,
            format!("[hang] no output for {waited}s; {codecs}"),
        );
    }

    // 4. Validate what came out.
    // Both containers: an fMP4 session writes init.mp4 + segment*.m4s,
    // and counting only .ts scored every one of them [no output] — which
    // is how a plan that had just been fixed still read as broken.
    let segments: Vec<PathBuf> = std::fs::read_dir(&out_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "ts" || e == "m4s"))
                .collect()
        })
        .unwrap_or_default();
    let playlist_ok = out_dir.join("master.m3u8").exists();

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
    // PES headers are a transport-stream thing; fMP4 carries its
    // timestamps in the moof, and nothing here reads those yet.
    if plan.has_video() {
        for seg in segments
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "ts"))
        {
            let (_, untimed, ooo) = video_pes_defects(&std::fs::read(seg).unwrap_or_default());
            if untimed + ooo > 0 {
                return (
                    Verdict::Fail,
                    format!(
                        "[bad dts] {}: {untimed} PES packets without a timestamp, \
                         {ooo} out-of-order — {codecs}",
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
            // An .m4s has no headers of its own — the tracks are declared
            // once, in the init segment, and ffprobe reads no streams at
            // all from the fragment alone.
            let init = out_dir.join("init.mp4");
            let probe = match first.extension().is_some_and(|e| e == "m4s") && init.exists() {
                true => init,
                false => first.clone(),
            };
            let (has_v, has_a) = segment_stream_kinds(&probe);
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
/// Bytes written into the output directory so far.
fn output_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

/// `(video PES packets, without a PTS, out of decode order)` for `ts`.
///
/// Read at the transport-stream level rather than through ffprobe,
/// because the two answer different questions. ffprobe re-splits the
/// elementary stream into access units and then attributes the PES
/// timestamps to them, so a stream whose access-unit boundary does not
/// coincide with the PES boundary loses a timestamp *in the reader*
/// while the file itself carries one on every PES.
///
/// That is not hypothetical: sources exist whose keyframe block ends
/// with the next picture's SEI (The Ark S02E08, and four more found by
/// this sweep). Copied faithfully, the SEI rides at the tail of the
/// keyframe's PES; ffprobe splits the access unit at that SEI, ends up
/// one boundary out, and reports the next picture as untimed. Every PES
/// in those segments has a PTS. Counting frames against packets to
/// cancel the difference — what this did before — cancels the wrong
/// ones: it hid the finding in 24 segments and fired on the 25th, which
/// was merely the truncated one.
///
/// What kahawai controls is the muxer's output, so that is what is
/// measured: every video PES carries a PTS, and decode order never goes
/// backwards.
fn video_pes_defects(ts: &[u8]) -> (usize, usize, usize) {
    const PKT: usize = 188;
    // PTS and DTS are 33-bit and wrap; a decrease of more than half the
    // range is that wrap, not a stream out of order.
    const HALF: i64 = 1 << 32;
    let (mut pes, mut untimed, mut ooo) = (0, 0, 0);
    let mut vpid: Option<u16> = None;
    let mut prev: Option<i64> = None;
    for chunk in ts.chunks_exact(PKT) {
        if chunk[0] != 0x47 || chunk[1] & 0x40 == 0 {
            continue;
        }
        let pid = (u16::from(chunk[1] & 0x1f) << 8) | u16::from(chunk[2]);
        let off = match (chunk[3] >> 4) & 3 {
            1 => 4,
            3 => 5 + chunk[4] as usize,
            _ => continue,
        };
        let Some(p) = chunk.get(off..) else { continue };
        // Video stream ids are 0xE0-0xEF; the first one seen names the
        // PID, and anything else on the segment is somebody else's.
        if p.len() < 14 || p[..3] != [0x00, 0x00, 0x01] || !(0xE0..=0xEF).contains(&p[3]) {
            continue;
        }
        if *vpid.get_or_insert(pid) != pid {
            continue;
        }
        pes += 1;
        let flags = p[7] >> 6;
        if flags & 0b10 == 0 {
            untimed += 1;
            continue;
        }
        // With both, the DTS follows the PTS; with only a PTS, decode
        // order and presentation order are the same.
        let stamp = |b: &[u8]| -> i64 {
            (i64::from(b[0] & 0x0e) << 29)
                | (i64::from(b[1]) << 22)
                | (i64::from(b[2] & 0xfe) << 14)
                | (i64::from(b[3]) << 7)
                | (i64::from(b[4]) >> 1)
        };
        let dts = if flags == 0b11 && p.len() >= 19 {
            stamp(&p[14..19])
        } else {
            stamp(&p[9..14])
        };
        if prev.is_some_and(|q| dts < q && q - dts < HALF) {
            ooo += 1;
        }
        prev = Some(dts);
    }
    (pes, untimed, ooo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kahawai_media::remux::RemuxSource;

    /// The head must stay readable no matter what was read before it.
    ///
    /// This is the whole point of the window being a function of the
    /// offset: the previous cumulative budget let a tail read (the
    /// Matroska Cues, an mp4 moov) spend the head's allowance, so
    /// parsebin's typefind — which reads offset 0 again after the
    /// demuxer has been to the end — got EOF and the file failed as
    /// "Can't typefind stream" with no output at all.
    #[test]
    fn the_head_survives_a_tail_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, vec![7u8; 4096]).unwrap();
        let open = || kahawai_media::remux::FileSource::open(&path).unwrap();
        let mut src = BudgetSource::new(open(), 1024, 1024);
        let mut buf = [0u8; 512];

        // Read the index at the tail first, as a demuxer does.
        assert_eq!(src.read_at(3800, &mut buf).unwrap(), 296);
        // ... and the head is still there.
        assert_eq!(src.read_at(0, &mut buf).unwrap(), 512);
        assert_eq!(src.read_at(512, &mut buf).unwrap(), 512);
        assert!(!src.exhausted.load(Ordering::Relaxed));

        // A read that straddles the head boundary stops at it, and the
        // gap between head and tail reads as EOF.
        assert_eq!(src.read_at(768, &mut buf).unwrap(), 256);
        assert_eq!(src.read_at(2048, &mut buf).unwrap(), 0);
        assert!(src.exhausted.load(Ordering::Relaxed));

        // --full: the head is the whole file, so nothing is withheld.
        let mut all = BudgetSource::new(open(), u64::MAX, 1024);
        assert_eq!(all.read_at(2048, &mut buf).unwrap(), 512);
        assert!(!all.exhausted.load(Ordering::Relaxed));
    }

    /// Every video PES the muxer writes carries a timestamp.
    ///
    /// The earlier form of this check counted ffprobe's access units and
    /// subtracted decoded frames to cancel the parameter-set packets
    /// that legitimately have none. It cancelled the wrong ones: on a
    /// source whose keyframe block ends with the next picture's SEI it
    /// reported one untimed picture per file, in whichever segment
    /// happened to have no picture-less access unit to spend. Reading
    /// the PES headers answers the question directly, and needs neither
    /// ffprobe nor a decoder.
    ///
    /// Everything here comes out of a muxer, so it needs no fixture.
    #[test]
    fn every_video_pes_carries_a_timestamp() {
        use gstreamer::prelude::*;

        kahawai_media::init().unwrap();
        if !kahawai_media::testutil::require_elements(&["x264enc", "mpegtsmux"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ts = dir.path().join("clip.ts");
        // key-int-max=12 at 24 fps: a keyframe every half second, so the
        // muxer repeats the parameter sets several times in a 2 s clip.
        let pipeline = gstreamer::parse::launch(&format!(
            "videotestsrc num-buffers=48 ! video/x-raw,framerate=24/1,width=320,height=180 ! \
             x264enc key-int-max=12 ! h264parse ! mpegtsmux ! filesink location={}",
            ts.display()
        ))
        .expect("pipeline");
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gstreamer::ClockTime::from_seconds(30),
                &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
            )
            .expect("muxing timed out");
        pipeline.set_state(gstreamer::State::Null).unwrap();

        let muxed = std::fs::read(&ts).unwrap();
        let (pes, untimed, ooo) = video_pes_defects(&muxed);
        assert!(pes > 40, "expected a PES per picture, got {pes}");
        assert_eq!(untimed, 0, "a freshly muxed clip stamps every PES");
        assert_eq!(ooo, 0, "nothing here is out of order");

        // Now damage it, because a check that cannot fail is not a check.
        let (_, untimed, _) = video_pes_defects(&strip_one_picture_timestamp(&muxed));
        assert_eq!(untimed, 1, "a PES without a timestamp must be caught");
    }

    /// Blank the PTS/DTS flags of one video PES header, in place.
    ///
    /// Transport stream, so: 188-byte packets, the video PID is the one
    /// whose payload starts a video PES, and the timestamp flags live
    /// in the third byte of the PES header extension. The declared
    /// header length stays as it is and its bytes become stuffing,
    /// which is what a real muxer would leave behind.
    fn strip_one_picture_timestamp(ts: &[u8]) -> Vec<u8> {
        const PKT: usize = 188;
        let mut out = ts.to_vec();
        let payload_offset = |p: &[u8]| -> Option<usize> {
            match (p[3] >> 4) & 3 {
                1 => Some(4),
                3 => Some(5 + p[4] as usize),
                _ => None,
            }
        };
        // Skip the first few pictures: the very first one anchors the
        // stream, and damaging it changes what the decoder can do at all.
        let mut seen = 0;
        for off in (0..out.len().saturating_sub(PKT)).step_by(PKT) {
            let p = &out[off..off + PKT];
            if p[0] != 0x47 || p[1] & 0x40 == 0 {
                continue;
            }
            let Some(o) = payload_offset(p) else { continue };
            if p.get(o..o + 4) != Some(&[0x00, 0x00, 0x01, 0xE0]) || p[o + 7] & 0xC0 == 0 {
                continue;
            }
            seen += 1;
            if seen < 8 {
                continue;
            }
            let hdr_len = p[o + 8] as usize;
            out[off + o + 7] = 0x00; // PTS_DTS_flags = none
            for k in 0..hdr_len {
                out[off + o + 9 + k] = 0xFF;
            }
            break;
        }
        out
    }
}
