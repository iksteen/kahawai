//! HUB-36: measured encode speed, so a capability report can say
//! "av1: yes, at 0.1×" instead of just "yes".
//!
//! Element presence and a dry run prove an encoder *works*; they say
//! nothing about whether it keeps up. Measured on this fleet: the same
//! 2160p HDR work runs ~0.65× realtime on one box and several× on
//! another, and placement could not tell them apart (HUB-36). Software
//! AV1 made it sharper — every box reports `av1` since HUB-15b, but on
//! a J5005 that is svtav1enc at a crawl.
//!
//! ## The reference workload: `videotestsrc pattern=snow`
//!
//! Noise, deliberately. The default SMPTE bars are nearly free for a
//! software encoder (static content, skip blocks everywhere) and would
//! report a J5005 at tens× realtime where real film runs below 1× —
//! exactly the dishonesty this module exists to kill. Noise defeats
//! motion estimation and intra prediction, the cost centers that make
//! software encoding slow, while fixed-function hardware encoders are
//! largely content-insensitive. So the error runs toward
//! under-promising, and because the pattern is deterministic, results
//! are exactly comparable ACROSS boxes — which is the property
//! placement actually ranks on. Absolute pessimism is corrected upward
//! per work class by the observed-pace EWMA, which sees real content.
//!
//! Encoder properties stay at element defaults for the same
//! comparability reason: this measures the box, not our tuning.
//!
//! ## Why a real clip, decoded in a loop
//!
//! The measurement transcodes a checked-in reference clip
//! (`assets/ref-{1080p,2160p}.h264`, ~0.2 MB of Annex-B noise) through
//! the SAME element chain a session builds — decoder, the encoder's
//! own converter chain, encoder — looping the clip until the time cap.
//!
//! Two earlier shapes were wrong, both caught by measurement:
//!
//! 1. `videotestsrc pattern=snow` feeding the encoder measured the
//!    NOISE GENERATOR: it produces 2.98× at 1080p and 0.78× at 2160p on
//!    the dev box, and every encoder duly reported 2.7–3.5× / 0.56–0.73×
//!    with nvh264enc "slower" than x264enc. It would have ranked boxes
//!    by CPU RNG speed.
//! 2. Pushing pre-generated raw frames from `appsrc` fixed that but
//!    measured the FEED PATH, and that bias is vendor-specific: system
//!    memory into VideoToolbox is slow, while a real session hands the
//!    encoder IOSurface-backed frames from the decoder with no copy.
//!    Measured on an M4: 0.80× synthetic vs **3.54× in a real
//!    session** — a 4.4× under-report, on exactly the axis placement
//!    ranks. NVENC showed no such gap (5.9× synthetic, 6.26× real), so
//!    the error silently favoured one vendor over another.
//!
//! Decoding a real bitstream reproduces the zero-copy decoder→encoder
//! path where the hardware has one. Annex-B is used because
//! elementary streams concatenate cleanly, so `multifilesrc loop=true`
//! is the whole looping mechanism — no seeking, no container headers
//! repeating. The content is noise so it stays incompressible and
//! motion estimation cannot cheat; it is checked in rather than
//! generated so every box encodes byte-identical input.
//!
//! The number therefore describes a REFERENCE TRANSCODE (h264 in,
//! target out) rather than an encoder in isolation. That is the more
//! useful quantity: it is the shape of the work placement dispatches.
//!
//! ## What a number means
//!
//! `multiple = (frames per second produced) / 24` — realtime multiples
//! against a 24 fps reference (the film rate that dominates this
//! library). The clock starts at the FIRST encoded buffer, not at
//! pipeline construction: preroll and context creation are one-off
//! costs, while sustain is the question placement asks. No timing means the
//! benchmark is incomplete and the capability stays out of serving.
//!
//! Results cache on disk keyed by the GStreamer version. Missing jobs measure
//! in background; a parent-observed crash quarantines that capability for this
//! fingerprint until an explicit successful benchmark clears it.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use serde::{Deserialize, Serialize};

/// The reference frame rate a multiple is expressed against.
const REFERENCE_FPS: f32 = 24.0;

/// Frames pushed into the synthetic (tone-map) feed — 10 s of
/// reference content, which the wall cap usually cuts short.
const FRAMES: usize = 240;

/// Wall-clock ceiling per measurement, matching the dry-run budget:
/// a box slower than this reports what it managed, honestly.
const CAP: Duration = Duration::from_secs(5);

/// Reference clips: 24 frames (1 s at the reference rate) of Annex-B
/// noise each, looped until the cap.
const REF_1080: &[u8] = include_bytes!("../assets/ref-1080p.h264");
const REF_2160: &[u8] = include_bytes!("../assets/ref-2160p.h264");
/// OPS-9 needs a clip in a codec that HAS hardware decoders worth
/// doubting. The encode benchmark only ever needed h264 (it is the
/// input, decoded the same way by every candidate), but the decoder
/// pathology this measures is `vah265dec`, so h265 has to exist as a
/// bitstream. Same 24 frames, same pictures — re-encoded from
/// `ref-1080p.h264`, so a decoder's two numbers describe the same
/// content in two codecs. 1080p only: a 20x rank inversion is not
/// subtler at 4K, and the asset would cost more than the finding.
const REF_1080_H265: &[u8] = include_bytes!("../assets/ref-1080p.h265");
/// OPS-9a: the codecs whose hardware decoders were never timed for want
/// of a bitstream. Not hypothetical — silence's hand-written demotions
/// name `vavp9dec`, `vavp8dec` and `vampeg2dec`, none of which OPS-9
/// could have found. Kept small (2.6 MB for all seven clips): decode
/// speed is compared BETWEEN decoders on one box, so the content only
/// has to be identical across candidates, not incompressible.
const REF_1080_AV1: &[u8] = include_bytes!("../assets/ref-1080p-av1.mkv");
const REF_1080_VP9: &[u8] = include_bytes!("../assets/ref-1080p-vp9.webm");
const REF_1080_VP8: &[u8] = include_bytes!("../assets/ref-1080p-vp8.webm");
/// 720p, and 24 frames: MPEG-2 at 1080p costs megabytes for content
/// this simple, and a rank inversion is not subtler at 720p.
const REF_720_MPEG2: &[u8] = include_bytes!("../assets/ref-720p-mpeg2.m2v");

/// Wall-clock ceiling for a DECODE measurement. Shorter than [`CAP`]
/// because decoding is the cheap half: even the pathological case
/// (~6 fps) produces enough frames in 2 s to be unmistakable against
/// ~121, and `doctor` is a command a human waits on.
const DECODE_CAP: Duration = Duration::from_secs(2);
/// Loops requested; the wall cap usually ends the run first.
const LOOPS: i32 = 60;
/// Below this at 1080p, the 2160p figure is derived rather than
/// measured (see `measure`): the box cannot sustain 1080p, so 4K is
/// decided, and the run would cost minutes of a satellite's CPU.
const SKIP_2160_BELOW: f32 = 1.0;
/// Distinct noise frames cycled into the GL tone-map measurement — more
/// than any encoder's reference depth, so nothing is trivially
/// predictable.
const POOL: usize = 6;

/// Realtime multiples for one element at the two resolutions that
/// matter.
///
/// `None` means this bucket was not measured successfully; an element becomes
/// a serving capability only when both buckets are present. `Some(v)` with a
/// tiny value is the opposite conclusion—measured and catastrophically slow—
/// and remains useful placement evidence rather than collapsing into absence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Speeds {
    pub s1080: Option<f32>,
    pub s2160: Option<f32>,
}

impl Speeds {
    /// The measurement for a source of this height (the plan's own
    /// bucket boundary: anything above 1080p is "4K work").
    pub fn at(&self, height: u32) -> Option<f32> {
        if height > 1080 {
            self.s2160
        } else {
            self.s1080
        }
    }

    fn complete(&self) -> bool {
        [self.s1080, self.s2160]
            .into_iter()
            .all(|speed| speed.is_some_and(|speed| speed > 0.0 && speed.is_finite()))
    }
}

/// Everything one box measured about itself, cached on disk.
const BENCH_CACHE_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchResults {
    /// Cache semantics version. Version 1 briefly mixed normal incomplete
    /// measurements into `crashes`; loading it retries those isolated jobs once
    /// so only parent-observed failures become authoritative under version 2.
    #[doc(hidden)]
    #[serde(default)]
    pub cache_version: u32,
    /// GStreamer version these numbers were taken with — a plugin
    /// upgrade can change them completely, so it keys the cache.
    pub gst: String,
    /// Keyed by ELEMENT name (nvh265enc, x265enc, …), not codec: the
    /// element is what actually runs, and a box that gains a hardware
    /// encoder should not inherit the software one's number.
    pub encoders: BTreeMap<String, Speeds>,
    /// The GL tone-map segment, measured through the real chain.
    pub tonemap: Option<Speeds>,
    /// Parent-observed child crashes by Unix timestamp. Presence means durable
    /// quarantine under this cache fingerprint; the timestamp is diagnostic,
    /// never an automatic recovery signal. Only successful explicit
    /// measurement or cache invalidation clears it.
    #[serde(default)]
    pub crashes: BTreeMap<String, i64>,
}

impl Default for BenchResults {
    fn default() -> Self {
        Self {
            cache_version: BENCH_CACHE_VERSION,
            gst: String::new(),
            encoders: BTreeMap::new(),
            tonemap: None,
            crashes: BTreeMap::new(),
        }
    }
}

const TONEMAP_JOB_KEY: &str = "@tonemap";

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

impl BenchResults {
    pub fn quarantined(&self, key: &str) -> bool {
        self.crashes.contains_key(key)
    }

    /// A serving encoder is one this exact benchmark fingerprint measured and
    /// has not subsequently quarantined.
    pub fn encoder_ready(&self, element: &str) -> bool {
        self.encoders.get(element).is_some_and(Speeds::complete) && !self.quarantined(element)
    }

    /// Tone-map follows the same authority as encoders; element presence alone
    /// is only the startup dry run, not evidence that the real chain survived.
    pub fn tonemap_ready(&self) -> bool {
        self.tonemap.is_some_and(|speeds| speeds.complete()) && !self.quarantined(TONEMAP_JOB_KEY)
    }

    fn quarantine(&mut self, key: &str) {
        self.crashes.insert(key.to_string(), unix_now());
    }

    fn clear_quarantine(&mut self, key: &str) {
        self.crashes.remove(key);
    }
}

/// One isolated benchmark child. Identity is carried separately from argv so
/// measurement, quarantine, and serving all name the same capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BenchmarkJob {
    ToneMap,
    Encoder(String),
}

impl BenchmarkJob {
    pub fn args(&self) -> Vec<String> {
        match self {
            Self::ToneMap => vec!["--tonemap".into()],
            Self::Encoder(element) => vec!["--only".into(), element.clone()],
        }
    }

    fn key(&self) -> &str {
        match self {
            Self::ToneMap => TONEMAP_JOB_KEY,
            Self::Encoder(element) => element,
        }
    }

    fn needed(&self, cache: &BenchResults) -> bool {
        if cache.quarantined(self.key()) {
            return false;
        }
        match self {
            Self::ToneMap => !cache.tonemap_ready(),
            Self::Encoder(element) => !cache.encoder_ready(element),
        }
    }
}

/// Missing, non-quarantined pieces that still need child processes. Successful
/// measurements are authoritative for this cache fingerprint and are not
/// repeated at every service start.
fn benchmark_jobs_for<'a>(
    elements: impl IntoIterator<Item = &'a str>,
    cached: Option<&BenchResults>,
) -> Vec<BenchmarkJob> {
    let mut offered = vec![BenchmarkJob::ToneMap];
    offered.extend(
        elements
            .into_iter()
            .map(|element| BenchmarkJob::Encoder(element.to_string())),
    );
    match cached {
        Some(cache) => offered
            .into_iter()
            .filter(|job| job.needed(cache))
            .collect(),
        None => offered,
    }
}

pub fn benchmark_jobs(cached: Option<&BenchResults>) -> Vec<BenchmarkJob> {
    benchmark_jobs_for(
        [
            crate::remux::h264_encoder(),
            crate::remux::hevc_encoder(),
            crate::remux::av1_encoder(),
        ]
        .into_iter()
        .flatten(),
        cached,
    )
}

/// Persist a negative result only after the supervising parent observed the
/// isolated child fail. An interrupted parent writes nothing, so a routine
/// service restart cannot masquerade as a benchmark crash.
pub fn record_crash(cache: &Path, job: &BenchmarkJob) {
    let mut out = load(cache).unwrap_or(BenchResults {
        gst: gst_version(),
        ..Default::default()
    });
    out.quarantine(job.key());
    match job {
        BenchmarkJob::ToneMap => out.tonemap = None,
        BenchmarkJob::Encoder(element) => {
            out.encoders.remove(element);
        }
    }
    store(cache, &out);
}

/// The floor a completed-but-barely-productive run reports. A box that
/// ran the full window and produced fewer than two frames has no
/// interval to time, but it is not unmeasured—it is catastrophically slow.
/// A positive floor preserves that evidence while still ranking it last;
/// zero is not a complete serving measurement. Measured live: silence encodes
/// 2160p AV1 in software at under one frame per five seconds.
const SPEED_FLOOR: f32 = 1.0 / (5.0 * REFERENCE_FPS);

/// Speed of `frames` buffers over `wall`, as a realtime multiple.
/// Fewer than two buffers gives no interval to measure; `ran` says
/// whether the pipeline nonetheless produced something, which is the
/// difference between "too slow to time" (`Some(SPEED_FLOOR)`, a real
/// upper bound) and "never ran" (`None`, unmeasured).
pub fn speed(frames: u64, wall: Duration, ran: bool) -> Option<f32> {
    if frames < 2 || wall.is_zero() {
        return (ran && frames >= 1).then_some(SPEED_FLOOR);
    }
    Some(((frames - 1) as f32 / wall.as_secs_f32()) / REFERENCE_FPS)
}

/// The GStreamer version string used as the cache key.
pub fn gst_version() -> String {
    let (maj, min, micro, nano) = gst::version();
    format!("{maj}.{min}.{micro}.{nano}")
}

/// Cached results, or None when missing, unreadable, malformed, or
/// taken with a different GStreamer (all the same thing to a caller:
/// measure again).
pub fn load(path: &Path) -> Option<BenchResults> {
    let raw = std::fs::read_to_string(path).ok()?;
    let mut r: BenchResults = serde_json::from_str(&raw).ok()?;
    if r.gst != gst_version() || r.cache_version > BENCH_CACHE_VERSION {
        return None;
    }
    if r.cache_version < BENCH_CACHE_VERSION {
        // Version 1 used the same map for parent-observed crashes and normal
        // incomplete child exits. Retrying every isolated entry once is the
        // only safe migration; a real crash is re-quarantined by the parent.
        r.crashes.clear();
        r.cache_version = BENCH_CACHE_VERSION;
        store(path, &r);
    }
    Some(r)
}

/// Best-effort persist — a box that cannot write its cache simply
/// re-measures next boot (mediahost sync-generation precedent).
pub fn store(path: &Path, r: &BenchResults) {
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(r)?)?;
        std::fs::rename(tmp, path)
    };
    if let Err(e) = write() {
        tracing::warn!(path = %path.display(), error = %e,
            "benchmark cache not written; will re-measure next start");
    }
}

fn merge_encoder_measurement(out: &mut BenchResults, element: &str, measured: Speeds) {
    if measured.complete() {
        out.clear_quarantine(element);
        out.encoders.insert(element.to_string(), measured);
    } else {
        // A normally exiting child can still encounter a transient pipeline
        // setup failure. Leave the job missing so startup retries it; only the
        // supervising parent can turn a crash or timeout into quarantine.
        out.encoders.remove(element);
    }
}

fn merge_tonemap_measurement(out: &mut BenchResults, measured: Speeds) {
    if measured.complete() {
        out.clear_quarantine(TONEMAP_JOB_KEY);
        out.tonemap = Some(measured);
    } else {
        out.tonemap = None;
    }
}

/// Measure every given encoder element (and the tone-map segment when
/// asked) at both resolutions, writing `cache` after EACH element.
///
/// Incremental because the measurement can take the process with it:
/// svtav1enc segfaults on the J5005 at 1080p (exit 139), where its
/// 320x240 startup dry-run passes happily. A crash used to lose every
/// result gathered before it; now the two encoders measured first
/// survive, and only the one that died is missing.
pub fn measure_into(elements: &[&str], tonemap: bool, cache: &Path) -> BenchResults {
    // MERGE into whatever is already there. Each piece of the benchmark
    // runs in its own child process (see the `benchmark` subcommand), so
    // a segfault costs exactly that piece — the results either side of
    // it are already on disk and must survive.
    let mut out = load(cache).unwrap_or(BenchResults {
        gst: gst_version(),
        ..Default::default()
    });
    if crate::init().is_err() {
        return out;
    }
    let tmp = std::env::temp_dir().join("kahawai-bench");
    let _ = std::fs::create_dir_all(&tmp);
    // Tone-map FIRST: it is cheap, it is HUB-15a's whole signal, and a
    // crash-prone encoder later in the list must not cost it. Silence
    // reported `null` forever because svtav1enc killed the child before
    // this ran — on the very box whose GL round trip motivated HUB-36.
    if tonemap {
        let s = Speeds {
            s1080: measure_tonemap(1920, 1080),
            s2160: measure_tonemap(3840, 2160),
        };
        tracing::info!(at_1080 = ?s.s1080, at_2160 = ?s.s2160, "tone-map speed measured");
        merge_tonemap_measurement(&mut out, s);
        store(cache, &out);
    }
    for el in elements {
        let measured = measure_one(el, &tmp);
        merge_encoder_measurement(&mut out, el, measured);
        store(cache, &out);
    }
    out
}

/// One element at both resolutions.
fn measure_one(el: &str, tmp: &Path) -> Speeds {
    let s1080 = measure_encoder(el, 1080, tmp);
    // Do not run a 2160p measurement whose answer is already bounded.
    // A box below realtime at 1080p is ~4x worse at 4K (four times the
    // pixels), which no threshold can rescue — and running it anyway is
    // expensive in the worst way: silence sat for MINUTES inside a
    // software-AV1 4K encode, because the wall cap bounds the
    // measurement window but not GStreamer's teardown of a mid-frame
    // encoder. Derived, and logged as such.
    let s2160 = match s1080 {
        Some(v) if v < SKIP_2160_BELOW => {
            let derived = v / 4.0;
            tracing::info!(
                element = el,
                at_1080 = v,
                derived_2160 = derived,
                "2160p derived from 1080p — already below realtime, and the \
                 measurement itself costs minutes on a box this slow"
            );
            Some(derived)
        }
        _ => measure_encoder(el, 2160, tmp),
    };
    let s = Speeds { s1080, s2160 };
    tracing::info!(
        element = el,
        at_1080 = ?s.s1080,
        at_2160 = ?s.s2160,
        "encoder speed measured"
    );
    s
}

/// In-memory variant, for callers with nowhere to persist (tests).
pub fn measure(elements: &[&str], tonemap: bool) -> BenchResults {
    let mut out = BenchResults {
        gst: gst_version(),
        ..Default::default()
    };
    if crate::init().is_err() {
        return out;
    }
    let tmp = std::env::temp_dir().join("kahawai-bench");
    let _ = std::fs::create_dir_all(&tmp);
    for el in elements {
        let measured = measure_one(el, &tmp);
        merge_encoder_measurement(&mut out, el, measured);
    }
    if tonemap {
        let s = Speeds {
            s1080: measure_tonemap(1920, 1080),
            s2160: measure_tonemap(3840, 2160),
        };
        tracing::info!(at_1080 = ?s.s1080, at_2160 = ?s.s2160, "tone-map speed measured");
        merge_tonemap_measurement(&mut out, s);
    }
    out
}

/// Transcode the reference clip for this resolution through the real
/// chain shape and time the encoder's output.
fn measure_encoder(element: &str, height: i32, dir: &Path) -> Option<f32> {
    let (clip, name) = if height > 1080 {
        (REF_2160, "ref-2160p.h264")
    } else {
        (REF_1080, "ref-1080p.h264")
    };
    // multifilesrc wants a path; the clip is a couple of hundred KB.
    let path = dir.join(name);
    if !path.exists() && std::fs::write(&path, clip).is_err() {
        return None;
    }

    let pipe = gst::Pipeline::new();
    let Ok(src) = gst::ElementFactory::make("multifilesrc")
        .property("location", path.to_string_lossy().as_ref())
        .property("loop", true)
        .property("num-buffers", LOOPS)
        .build()
    else {
        return None;
    };
    // parsebin over an explicit h264parse: it also picks the decoder's
    // preferred stream-format, which is what a session does.
    let (Some(parse), Some(decode)) = (make("h264parse"), make("decodebin")) else {
        return None;
    };
    let Ok(enc) = gst::ElementFactory::make(element).build() else {
        return None;
    };
    let Ok(sink) = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
    else {
        return None;
    };
    // The SAME converters the encode chain uses — that is the point.
    let converters: Vec<gst::Element> = crate::remux::encode_converter_names(element)
        .iter()
        .filter_map(|n| make(n))
        .collect();

    if pipe.add_many([&src, &parse, &decode]).is_err()
        || pipe.add_many(&converters).is_err()
        || pipe.add_many([&enc, &sink]).is_err()
    {
        return None;
    }
    if gst::Element::link_many([&src, &parse, &decode]).is_err() {
        let _ = pipe.set_state(gst::State::Null);
        return None;
    }
    let mut tail: Vec<&gst::Element> = converters.iter().collect();
    tail.push(&enc);
    tail.push(&sink);
    if gst::Element::link_many(tail.clone()).is_err() {
        let _ = pipe.set_state(gst::State::Null);
        return None;
    }
    // decodebin's src pad appears once caps are known.
    let head = tail[0].clone();
    decode.connect_pad_added(move |_, pad| {
        if let Some(sink_pad) = head.static_pad("sink")
            && !sink_pad.is_linked()
        {
            let _ = pad.link(&sink_pad);
        }
    });

    count_through(&pipe, &enc, || {})
}

/// The real tone-map segment, fed 10-bit: the GL upload/download round
/// trip is the cost that made a J5005 0.65× (HUB-15a), and 8-bit input
/// would not exercise it honestly.
fn measure_tonemap(w: i32, h: i32) -> Option<f32> {
    if !crate::remux::tonemap_available() {
        return None;
    }
    let mut chain = vec![make("videoconvert")?];
    chain.extend(crate::remux::tonemap_segment(""));
    // The GL segment genuinely wants 10-bit in: that upload/download
    // round trip IS the cost being measured (HUB-15a).
    run_counting(&chain, w, h, "I420_10LE")
}

/// OPS-9: frames per second one decoder element sustains on the
/// reference clip. FPS, not realtime multiples — the question here is
/// "is this element slower than that element", which is answered by
/// comparing two numbers of the same kind, and the finding reads in
/// the units the pathology was reported in (~6 fps versus ~121).
///
/// The element is named EXPLICITLY rather than autoplugged: the whole
/// point is to time the candidate GStreamer would have chosen against
/// the one it would not, and decodebin would just keep picking the
/// former. `None` means it could not be timed at all — a decoder that
/// refuses the clip is not thereby slow, and must not be reported as
/// though it were.
pub fn decode_fps(element: &str, codec: Codec) -> Option<f32> {
    if crate::init().is_err() {
        return None;
    }
    let tmp = std::env::temp_dir().join("kahawai-bench");
    let _ = std::fs::create_dir_all(&tmp);
    let path = tmp.join(codec.file());
    if !path.exists() && std::fs::write(&path, codec.clip()).is_err() {
        return None;
    }

    let pipe = gst::Pipeline::new();
    let src = if codec.loops() {
        gst::ElementFactory::make("multifilesrc")
            .property("location", path.to_string_lossy().as_ref())
            .property("loop", true)
            .property("num-buffers", LOOPS)
            .build()
            .ok()?
    } else {
        gst::ElementFactory::make("filesrc")
            .property("location", path.to_string_lossy().as_ref())
            .build()
            .ok()?
    };
    let dec = make(element)?;
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .ok()?;
    if pipe.add_many([&src, &dec, &sink]).is_err() {
        return None;
    }
    match codec.parser() {
        // Elementary stream: an explicit parser, linked straight
        // through, so the looped source stays a plain byte feed.
        Some(name) => {
            let parse = make(name)?;
            if pipe.add(&parse).is_err()
                || gst::Element::link_many([&src, &parse, &dec, &sink]).is_err()
            {
                let _ = pipe.set_state(gst::State::Null);
                return None;
            }
        }
        // Container: parsebin demuxes and parses, and its pad appears
        // once the caps are known.
        None => {
            let demux = make("parsebin")?;
            if pipe.add(&demux).is_err()
                || gst::Element::link(&src, &demux).is_err()
                || gst::Element::link(&dec, &sink).is_err()
            {
                let _ = pipe.set_state(gst::State::Null);
                return None;
            }
            let target = dec.clone();
            demux.connect_pad_added(move |_, pad| {
                if let Some(sink_pad) = target.static_pad("sink")
                    && !sink_pad.is_linked()
                {
                    let _ = pad.link(&sink_pad);
                }
            });
        }
    }
    count_through_capped(&pipe, &dec, DECODE_CAP, || {}).map(|m| m * REFERENCE_FPS)
}

/// A codec OPS-9 can measure a decoder against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Av1,
    Vp9,
    Vp8,
    Mpeg2,
}

impl Codec {
    fn clip(self) -> &'static [u8] {
        match self {
            Codec::H264 => REF_1080,
            Codec::H265 => REF_1080_H265,
            Codec::Av1 => REF_1080_AV1,
            Codec::Vp9 => REF_1080_VP9,
            Codec::Vp8 => REF_1080_VP8,
            Codec::Mpeg2 => REF_720_MPEG2,
        }
    }
    fn file(self) -> &'static str {
        match self {
            Codec::H264 => "ref-1080p.h264",
            Codec::H265 => "ref-1080p.h265",
            Codec::Av1 => "ref-1080p-av1.mkv",
            Codec::Vp9 => "ref-1080p-vp9.webm",
            Codec::Vp8 => "ref-1080p-vp8.webm",
            Codec::Mpeg2 => "ref-720p-mpeg2.m2v",
        }
    }
    /// Elementary streams concatenate, so `multifilesrc loop` can run a
    /// short clip until the cap and a fast decoder still gets a real
    /// sample. Container clips cannot be looped that way, so they are
    /// long enough to stand alone and are demuxed by `parsebin`.
    fn loops(self) -> bool {
        !matches!(self, Codec::Av1 | Codec::Vp9 | Codec::Vp8)
    }
    fn parser(self) -> Option<&'static str> {
        match self {
            Codec::H264 => Some("h264parse"),
            Codec::H265 => Some("h265parse"),
            Codec::Mpeg2 => Some("mpegvideoparse"),
            // parsebin demuxes these; no explicit parser.
            Codec::Av1 | Codec::Vp9 | Codec::Vp8 => None,
        }
    }
    /// The caps a decoder must accept to be a candidate for this codec.
    ///
    /// MPEG carries its version in a FIELD, not the media type, so a
    /// bare `video/mpeg` also matches the MPEG-1 and MPEG-4 decoders —
    /// which then appear in the mpeg2 row as candidates that "would not
    /// decode the clip", because of course they would not. Listing
    /// elements nobody should compare is how a check teaches its reader
    /// to skip it.
    pub fn caps(self) -> gst::Caps {
        let b = gst::Caps::builder(self.caps_name());
        match self {
            Codec::Mpeg2 => b.field("mpegversion", 2i32).build(),
            _ => b.build(),
        }
    }

    fn caps_name(self) -> &'static str {
        match self {
            Codec::H264 => "video/x-h264",
            Codec::H265 => "video/x-h265",
            Codec::Av1 => "video/x-av1",
            Codec::Vp9 => "video/x-vp9",
            Codec::Vp8 => "video/x-vp8",
            Codec::Mpeg2 => "video/mpeg",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "hevc",
            Codec::Av1 => "av1",
            Codec::Vp9 => "vp9",
            Codec::Vp8 => "vp8",
            Codec::Mpeg2 => "mpeg2",
        }
    }
    pub const ALL: [Codec; 6] = [
        Codec::H264,
        Codec::H265,
        Codec::Av1,
        Codec::Vp9,
        Codec::Vp8,
        Codec::Mpeg2,
    ];
}

fn make(name: &str) -> Option<gst::Element> {
    gst::ElementFactory::make(name).build().ok()
}

/// One frame of pseudo-random bytes. xorshift64*, not a crypto RNG:
/// this needs incompressible-looking data at memory bandwidth, and
/// the sequence is fixed so every box encodes identical content.
fn noise_frame(bytes: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    let mut out = vec![0u8; bytes];
    for chunk in out.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let v = x.to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
    out
}

/// Run a built pipeline to EOS or the cap, counting buffers out of
/// `at`. The clock starts at the first buffer: preroll, GL context
/// creation and encoder init are one-off, while sustain is not.
fn count_through(
    pipe: &gst::Pipeline,
    at: &gst::Element,
    on_playing: impl FnOnce(),
) -> Option<f32> {
    count_through_capped(pipe, at, CAP, on_playing)
}

fn count_through_capped(
    pipe: &gst::Pipeline,
    at: &gst::Element,
    cap: Duration,
    on_playing: impl FnOnce(),
) -> Option<f32> {
    let frames = std::sync::Arc::new(AtomicU64::new(0));
    let started: std::sync::Arc<std::sync::Mutex<Option<Instant>>> = Default::default();
    let Some(out_pad) = at.static_pad("src") else {
        let _ = pipe.set_state(gst::State::Null);
        return None;
    };
    {
        let (frames, started) = (frames.clone(), started.clone());
        out_pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            let mut s = started.lock().unwrap();
            if s.is_none() {
                *s = Some(Instant::now());
            } else {
                frames.fetch_add(1, Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        });
    }
    if pipe.set_state(gst::State::Playing).is_err() {
        let _ = pipe.set_state(gst::State::Null);
        return None;
    }
    // Probe attached, pipeline rolling: only now may a feeder start, so
    // the first buffer produced is the first buffer counted.
    on_playing();
    let deadline = Instant::now() + cap;
    // Wait for the run to finish or the cap to expire, whichever comes
    // first; a slow box simply reports the frames it managed.
    if let Some(bus) = pipe.bus() {
        let left = deadline.saturating_duration_since(Instant::now());
        if let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_nseconds(left.as_nanos() as u64),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) && msg.type_() == gst::MessageType::Error
        {
            if let gst::MessageView::Error(e) = msg.view() {
                tracing::debug!(
                    src = %e.src().map(|s| s.name().to_string()).unwrap_or_default(),
                    error = %e.error(),
                    "benchmark pipeline failed"
                );
            }
            let _ = pipe.set_state(gst::State::Null);
            return None;
        }
    }
    let elapsed = started
        .lock()
        .unwrap()
        .map(|t| t.elapsed())
        .unwrap_or_default();
    let n = frames.load(Ordering::Relaxed);
    let produced_any = started.lock().unwrap().is_some();
    let _ = pipe.set_state(gst::State::Null);
    speed(n + 1, elapsed, produced_any)
}

/// Push pre-generated noise through `chain` as fast as it is accepted
/// and time the output. Still the honest shape for the TONE-MAP
/// measurement: the real chain also hands the GL segment system
/// memory (decode → videoconvert → glupload).
fn run_counting(chain: &[gst::Element], w: i32, h: i32, format: &str) -> Option<f32> {
    let pipe = gst::Pipeline::new();
    // Every format here is 4:2:0 (1.5 bytes/pixel); 10-bit doubles it.
    // A wrong size only means a differently-shaped noise frame, which
    // the encoder is equally happy to compress.
    let frame_bytes = (w as usize * h as usize * 3 / 2) * if format.contains("10") { 2 } else { 1 };
    let pool: Vec<gst::Buffer> = (0..POOL)
        .map(|i| {
            gst::Buffer::from_mut_slice(noise_frame(frame_bytes, 0x9E37_79B9_7F4A_7C15 ^ i as u64))
        })
        .collect();

    let caps = gst::Caps::builder("video/x-raw")
        .field("format", format)
        .field("width", w)
        .field("height", h)
        .field("framerate", gst::Fraction::new(REFERENCE_FPS as i32, 1))
        .build();
    // Keep the encoder FED. appsrc defaults to a 200 KB internal queue
    // — smaller than one 4K frame (12.4 MB) — which lock-steps the feed
    // to one frame in flight and measures the harness instead of the
    // encoder. Hardware encoders pipeline deeply (VideoToolbox most of
    // all), so give them a pool's worth of runway and let block=true
    // pace the loop once it is full.
    let src = gstreamer_app::AppSrc::builder()
        .caps(&caps)
        .format(gst::Format::Time)
        .is_live(false)
        .block(true)
        .max_bytes((frame_bytes * POOL) as u64)
        .build();
    // sync=false: measure how fast it CAN run, not the clock.
    let Ok(sink) = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
    else {
        return None;
    };

    if pipe.add(src.upcast_ref::<gst::Element>()).is_err()
        || pipe.add(&sink).is_err()
        || pipe.add_many(chain).is_err()
    {
        return None;
    }
    let mut all: Vec<&gst::Element> = vec![src.upcast_ref()];
    all.extend(chain.iter());
    all.push(&sink);
    if gst::Element::link_many(all).is_err() {
        let _ = pipe.set_state(gst::State::Null);
        return None;
    }

    // Fed from a side thread STARTED BY THE COUNTER: push_buffer blocks
    // on the appsrc queue, so the chain's own speed paces the loop.
    // Waiting on the pipeline's state here instead does not work — GL
    // pipelines change state asynchronously and the wait outlived the
    // cap, silently reporting 0.00x for the tone-map.
    let last = chain.last().cloned();
    let spf = gst::ClockTime::SECOND.nseconds() as f64 / REFERENCE_FPS as f64;
    let feed = move || {
        std::thread::spawn(move || {
            for i in 0..FRAMES {
                // copy(), not clone(): a clone SHARES the buffer, so
                // get_mut() returns None and the loop would break before
                // pushing anything (measured: a silent 0.00x tone-map).
                // A shallow copy has its own metadata and shares the
                // pixel memory, so stamping PTS costs nothing.
                let mut buf = pool[i % POOL].copy();
                {
                    let Some(b) = buf.get_mut() else { break };
                    b.set_pts(gst::ClockTime::from_nseconds((i as f64 * spf) as u64));
                    b.set_duration(gst::ClockTime::from_nseconds(spf as u64));
                }
                if src.push_buffer(buf).is_err() {
                    break;
                }
            }
            let _ = src.end_of_stream();
        });
    };
    match last {
        Some(el) => count_through(&pipe, &el, feed),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_math_and_degenerate_inputs() {
        let near = |got: Option<f32>, want: f32| {
            let g = got.unwrap_or_else(|| panic!("expected a measurement, got None"));
            assert!((g - want).abs() < 0.001, "got {g}, want {want}");
        };
        // 24 fps produced in one second of wall time = 1.0× realtime.
        // 25 buffers = 24 intervals.
        near(speed(25, Duration::from_secs(1), true), 1.0);
        near(speed(25, Duration::from_secs(4), true), 0.25);
        near(speed(241, Duration::from_secs(1), true), 10.0);

        // Ran but produced one lonely frame: measured and dreadful, not
        // incomplete. The positive floor keeps the job serving-safe while
        // placement ranks it behind every useful encoder.
        assert_eq!(speed(1, Duration::from_secs(5), true), Some(SPEED_FLOOR));
        const { assert!(SPEED_FLOOR > 0.0 && SPEED_FLOOR < 0.01) };

        // Never produced anything: genuinely unmeasured.
        assert_eq!(speed(0, Duration::from_secs(1), true), None);
        assert_eq!(speed(1, Duration::from_secs(1), false), None);
        assert_eq!(speed(100, Duration::ZERO, false), None);
    }

    #[test]
    fn speeds_bucket_by_height() {
        let s = Speeds {
            s1080: Some(6.0),
            s2160: Some(2.0),
        };
        assert_eq!(s.at(1080), Some(6.0));
        assert_eq!(s.at(720), Some(6.0));
        assert_eq!(s.at(2160), Some(2.0));
        assert_eq!(s.at(1600), Some(2.0)); // scope 4K
        // An incomplete bucket stays absent and prevents serving readiness.
        assert_eq!(Speeds::default().at(1080), None);
    }

    #[test]
    fn cache_round_trip_and_version_invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("benchmarks.json");
        let mut r = BenchResults {
            gst: gst_version(),
            ..Default::default()
        };
        r.encoders.insert(
            "nvh265enc".into(),
            Speeds {
                s1080: Some(6.2),
                s2160: Some(2.1),
            },
        );
        r.tonemap = Some(Speeds {
            s1080: Some(3.0),
            s2160: Some(0.65),
        });
        store(&path, &r); // also creates the directory
        assert_eq!(load(&path), Some(r.clone()));

        // A GStreamer upgrade invalidates: numbers taken under other
        // plugins are not evidence about this one.
        let stale = BenchResults {
            gst: "0.0.0.0".into(),
            ..r.clone()
        };
        store(&path, &stale);
        assert_eq!(load(&path), None);

        assert_eq!(load(&dir.path().join("nope.json")), None);
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(load(&path), None);
    }

    #[test]
    fn mixed_provenance_quarantine_cache_retries_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("benchmarks.json");
        let mut old = serde_json::to_value(BenchResults {
            gst: gst_version(),
            crashes: BTreeMap::from([("transientenc".into(), 1)]),
            ..Default::default()
        })
        .unwrap();
        old.as_object_mut().unwrap().remove("cache_version");
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();

        let migrated = load(&path).unwrap();
        assert_eq!(migrated.cache_version, BENCH_CACHE_VERSION);
        assert!(!migrated.quarantined("transientenc"));
        assert!(
            benchmark_jobs_for(["transientenc"], Some(&migrated))
                .contains(&BenchmarkJob::Encoder("transientenc".into()))
        );
        assert_eq!(load(&path), Some(migrated), "migration was not durable");
    }

    #[test]
    fn incomplete_normal_results_remain_retryable() {
        let mut r = BenchResults {
            gst: gst_version(),
            ..Default::default()
        };
        merge_encoder_measurement(
            &mut r,
            "transientenc",
            Speeds {
                s1080: Some(2.0),
                s2160: None,
            },
        );
        merge_tonemap_measurement(
            &mut r,
            Speeds {
                s1080: None,
                s2160: None,
            },
        );

        assert!(!r.quarantined("transientenc"));
        assert!(!r.quarantined(TONEMAP_JOB_KEY));
        assert_eq!(
            benchmark_jobs_for(["transientenc"], Some(&r)),
            [
                BenchmarkJob::ToneMap,
                BenchmarkJob::Encoder("transientenc".into()),
            ],
            "a normally exiting incomplete child was suppressed permanently"
        );
    }

    /// A parent-observed crash is durable quarantine. Merely starting a child
    /// records nothing, and elapsed time never turns failure into capability.
    #[test]
    fn quarantine_requires_success_or_fingerprint_change() {
        let mut r = BenchResults {
            gst: gst_version(),
            ..Default::default()
        };
        r.quarantine("svtav1enc");
        r.encoders.insert(
            "vah264enc".into(),
            Speeds {
                s1080: Some(5.1),
                s2160: Some(1.4),
            },
        );
        r.encoders.insert(
            "half-measured".into(),
            Speeds {
                s1080: Some(2.0),
                s2160: None,
            },
        );
        assert!(r.quarantined("svtav1enc"));
        assert!(!r.encoder_ready("svtav1enc"));
        assert!(r.encoder_ready("vah264enc"));
        assert!(!r.encoder_ready("nvh264enc"), "unmeasured is not serving");
        assert!(
            !r.encoder_ready("half-measured"),
            "partial measurement became a serving capability"
        );
        assert_eq!(
            benchmark_jobs_for(["vah264enc", "svtav1enc", "half-measured"], Some(&r)),
            [
                BenchmarkJob::ToneMap,
                BenchmarkJob::Encoder("half-measured".into()),
            ],
            "only missing work should run; measured and quarantined jobs stay out"
        );

        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("benchmarks.json");
        record_crash(&cache, &BenchmarkJob::Encoder("svtav1enc".into()));
        let persisted = load(&cache).expect("crash record");
        assert!(persisted.quarantined("svtav1enc"));
        assert!(
            !benchmark_jobs_for(["svtav1enc"], Some(&persisted))
                .contains(&BenchmarkJob::Encoder("svtav1enc".into())),
            "quarantine changed merely because time passed"
        );

        let mut tone = persisted;
        tone.tonemap = Some(Speeds {
            s1080: Some(3.0),
            s2160: Some(0.7),
        });
        store(&cache, &tone);
        record_crash(&cache, &BenchmarkJob::ToneMap);
        let tone = load(&cache).expect("tone-map crash record");
        assert!(tone.tonemap.is_none(), "stale tone-map speed survived");
        assert!(tone.quarantined(TONEMAP_JOB_KEY));
        assert!(!tone.tonemap_ready());
        assert!(
            !benchmark_jobs_for(std::iter::empty(), Some(&tone)).contains(&BenchmarkJob::ToneMap),
            "quarantined tone-map was scheduled automatically"
        );

        // A different fingerprint invalidates the whole cache, which makes
        // every present job missing and eligible again.
        let stale = BenchResults {
            gst: "different-gstreamer".into(),
            ..tone
        };
        store(&cache, &stale);
        assert!(load(&cache).is_none());
        assert_eq!(
            benchmark_jobs_for(std::iter::empty(), None),
            [BenchmarkJob::ToneMap]
        );
    }

    /// The real thing on whatever this box has: every verified encoder
    /// must produce a positive, finite multiple.
    #[test]
    fn measures_this_box() {
        crate::init().unwrap();
        let Some(el) = crate::remux::h264_encoder() else {
            return; // no encoder here; doctor's problem, not ours
        };
        let r = measure(&[el], false);
        let s = r.encoders.get(el).copied().unwrap_or_default();
        let v = s
            .s1080
            .unwrap_or_else(|| panic!("{el} reported no 1080p measurement: {s:?}"));
        assert!(v > 0.0 && v.is_finite(), "{el} measured {s:?} at 1080p");
        // Sanity: nothing on earth transcodes 1080p at 10000× realtime.
        assert!(v < 10_000.0, "{el} implausible: {s:?}");
    }
}
