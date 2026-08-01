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
//! costs, while sustain is the question placement asks. `0.0` means
//! unmeasured, never "infinitely slow" — every consumer treats it as
//! "no data, assume sufficient".
//!
//! Results cache on disk keyed by the GStreamer version (a plugin
//! upgrade invalidates them); the caller re-measures in the background
//! and overwrites when reality drifts.

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
/// `None` = UNMEASURED: no data, which every consumer reads as "assume
/// sufficient". `Some(v)` with a tiny v is the OPPOSITE conclusion —
/// measured, and catastrophically slow. Those two must never share an
/// inhabitant: under a 0.0 sentinel the one box that cannot transcode
/// 4K AV1 looked exactly like a box nobody had asked yet.
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
}

/// Everything one box measured about itself, cached on disk.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BenchResults {
    /// GStreamer version these numbers were taken with — a plugin
    /// upgrade can change them completely, so it keys the cache.
    pub gst: String,
    /// Keyed by ELEMENT name (nvh265enc, x265enc, …), not codec: the
    /// element is what actually runs, and a box that gains a hardware
    /// encoder should not inherit the software one's number.
    pub encoders: BTreeMap<String, Speeds>,
    /// The GL tone-map segment, measured through the real chain.
    pub tonemap: Option<Speeds>,
    /// Elements this run STARTED measuring, written before each attempt.
    /// One listed here with no entry in `encoders` took the process
    /// down — which is a capability fact, not a gap: svtav1enc
    /// segfaults at 1080p on the J5005 while passing its 320x240
    /// startup dry-run, so a report built from dry runs alone
    /// advertises an encoder that crashes in session.
    #[serde(default)]
    pub attempted: Vec<String>,
}

impl BenchResults {
    /// Did measuring this element kill the benchmark? True only when it
    /// was attempted and produced nothing.
    pub fn crashed(&self, element: &str) -> bool {
        self.attempted.iter().any(|e| e == element) && !self.encoders.contains_key(element)
    }
}

/// The floor a completed-but-barely-productive run reports. A box that
/// ran the full window and produced fewer than two frames has no
/// interval to time, but it is NOT unmeasured — it is catastrophically
/// slow, and 0.0 would read as "no data, assume sufficient" and send
/// it 4K work. Measured live: silence encodes 2160p AV1 in software at
/// under one frame per five seconds. Reported as `Some(_)` — measured
/// and dreadful, which is the opposite conclusion from `None`.
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
    let r: BenchResults = serde_json::from_str(&raw).ok()?;
    (r.gst == gst_version()).then_some(r)
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

/// Has reality moved enough to be worth telling the hub about? Any
/// element appearing or disappearing counts (a driver came back, a
/// package was installed); so does a ≥25% relative change on any
/// measured speed — below that it is measurement noise, above it the
/// box is genuinely a different placement candidate.
pub fn drifted(old: &BenchResults, new: &BenchResults) -> bool {
    if old.gst != new.gst {
        return true;
    }
    let keys: std::collections::BTreeSet<&String> =
        old.encoders.keys().chain(new.encoders.keys()).collect();
    for k in keys {
        match (old.encoders.get(k), new.encoders.get(k)) {
            (Some(a), Some(b)) => {
                if moved(a.s1080, b.s1080) || moved(a.s2160, b.s2160) {
                    return true;
                }
            }
            _ => return true, // appeared or vanished
        }
    }
    match (old.tonemap, new.tonemap) {
        (Some(a), Some(b)) => moved(a.s1080, b.s1080) || moved(a.s2160, b.s2160),
        (None, None) => false,
        _ => true,
    }
}

fn moved(a: Option<f32>, b: Option<f32>) -> bool {
    match (a, b) {
        // Gaining or losing a measurement is always news.
        (Some(_), None) | (None, Some(_)) => true,
        (None, None) => false,
        (Some(a), Some(b)) if a > 0.0 => ((a - b).abs() / a) >= 0.25,
        (Some(a), Some(b)) => a != b,
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
    let mut out = BenchResults {
        gst: gst_version(),
        ..Default::default()
    };
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
        out.tonemap = Some(s);
        store(cache, &out);
    }
    for el in elements {
        // Record the attempt BEFORE making it: if this element takes the
        // process down, the next run can tell "crashed" from "never
        // asked".
        out.attempted.push((*el).to_string());
        store(cache, &out);
        out.encoders
            .insert((*el).to_string(), measure_one(el, &tmp));
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
        out.encoders
            .insert((*el).to_string(), measure_one(el, &tmp));
    }
    if tonemap {
        let s = Speeds {
            s1080: measure_tonemap(1920, 1080),
            s2160: measure_tonemap(3840, 2160),
        };
        tracing::info!(at_1080 = ?s.s1080, at_2160 = ?s.s2160, "tone-map speed measured");
        out.tonemap = Some(s);
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
    chain.extend(crate::remux::tonemap_segment());
    // The GL segment genuinely wants 10-bit in: that upload/download
    // round trip IS the cost being measured (HUB-15a).
    run_counting(&chain, w, h, "I420_10LE")
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
    let deadline = Instant::now() + CAP;
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

        // Ran but produced one lonely frame: MEASURED and dreadful, not
        // unmeasured. The two must stay distinguishable — downstream
        // reads absence as "assume sufficient", which would send 4K AV1
        // to the J5005, the one box that cannot do it (measured live).
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
        // Unmeasured reads as "no data", never as zero speed.
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

    /// A crash is a capability fact: attempted, produced nothing.
    /// Distinguishing it from "never asked" is what stops a box
    /// advertising an encoder that segfaults in session (silence,
    /// svtav1enc at 1080p — its 320x240 dry-run passes).
    #[test]
    fn crashed_distinguishes_attempted_from_unasked() {
        let mut r = BenchResults {
            gst: gst_version(),
            ..Default::default()
        };
        r.attempted.push("svtav1enc".into());
        r.attempted.push("vah264enc".into());
        r.encoders.insert(
            "vah264enc".into(),
            Speeds {
                s1080: Some(5.1),
                s2160: Some(1.4),
            },
        );
        assert!(r.crashed("svtav1enc"), "attempted, no result => crashed");
        assert!(!r.crashed("vah264enc"), "attempted and measured");
        assert!(!r.crashed("nvh264enc"), "never attempted is not a crash");
    }

    #[test]
    fn drift_threshold() {
        let base = |v: f32| {
            let mut r = BenchResults {
                gst: gst_version(),
                ..Default::default()
            };
            r.encoders.insert(
                "x265enc".into(),
                Speeds {
                    s1080: Some(v),
                    s2160: Some(v / 3.0),
                },
            );
            r
        };
        assert!(!drifted(&base(1.0), &base(1.0)));
        assert!(!drifted(&base(1.0), &base(1.2)), "20% is noise");
        assert!(drifted(&base(1.0), &base(1.3)), "30% is news");
        assert!(drifted(&base(1.0), &base(0.5)), "halved is news");

        // An element appearing (driver installed) is always news.
        let mut gained = base(1.0);
        gained
            .encoders
            .insert("nvh265enc".into(), Speeds::default());
        assert!(drifted(&base(1.0), &gained));

        // So is the tone-map tier appearing or vanishing.
        let mut tm = base(1.0);
        tm.tonemap = Some(Speeds {
            s1080: Some(2.0),
            s2160: Some(0.6),
        });
        assert!(drifted(&base(1.0), &tm));
        assert!(drifted(&tm, &base(1.0)));
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
