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
//! ## The source must not be the bottleneck
//!
//! `videotestsrc pattern=snow` CANNOT feed this measurement: measured
//! on the dev box, its generator alone produces 2.98× realtime at
//! 1080p and 0.78× at 2160p — so a first cut of this module reported
//! every encoder at 2.7–3.5× / 0.56–0.73× (nvh264enc "slower" than
//! x264enc), i.e. it measured the noise generator on the CPU and would
//! have ranked boxes by RNG speed. The frames are therefore generated
//! ONCE into a small pool with a cheap PRNG and pushed cyclically from
//! `appsrc`, so the only thing under the clock is the encoder. The
//! pool holds several distinct frames because a repeated frame makes
//! every P-frame free — the encoder would predict it perfectly and
//! report a fantasy.
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

/// Frames pushed per measurement — 10 s of reference content, which
/// the wall cap usually cuts short on slow boxes.
const FRAMES: i32 = 240;

/// Wall-clock ceiling per measurement, matching the dry-run budget:
/// a box slower than this reports what it managed, honestly.
const CAP: Duration = Duration::from_secs(5);

/// Distinct noise frames cycled into the encoder. Must exceed the
/// reference-frame depth of common encoders (1–4) so no P-frame ever
/// finds its own content again; the pool is otherwise as small as
/// possible because 4K frames are megabytes each.
const POOL: usize = 6;

/// Realtime multiples for one element at the two resolutions that
/// matter. 0.0 = unmeasured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Speeds {
    pub s1080: f32,
    pub s2160: f32,
}

impl Speeds {
    /// The measurement for a source of this height (the plan's own
    /// bucket boundary: anything above 1080p is "4K work").
    pub fn at(&self, height: u32) -> Option<f32> {
        let v = if height > 1080 {
            self.s2160
        } else {
            self.s1080
        };
        (v > 0.0).then_some(v)
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
}

/// The floor a completed-but-barely-productive run reports. A box that
/// ran the full window and produced fewer than two frames has no
/// interval to time, but it is NOT unmeasured — it is catastrophically
/// slow, and 0.0 would read as "no data, assume sufficient" and send
/// it 4K work. Measured live: silence encodes 2160p AV1 in software at
/// under one frame per five seconds. This is an upper bound on that.
const SPEED_FLOOR: f32 = 1.0 / (5.0 * REFERENCE_FPS);

/// Speed of `frames` buffers over `wall`, as a realtime multiple.
/// Fewer than two buffers gives no interval to measure; `ran` says
/// whether the pipeline nonetheless completed its window, which is the
/// difference between "too slow to time" (a real, tiny speed) and
/// "never ran" (unmeasured, 0.0).
pub fn speed(frames: u64, wall: Duration, ran: bool) -> f32 {
    if frames < 2 || wall.is_zero() {
        return if ran && frames >= 1 { SPEED_FLOOR } else { 0.0 };
    }
    ((frames - 1) as f32 / wall.as_secs_f32()) / REFERENCE_FPS
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

fn moved(a: f32, b: f32) -> bool {
    if a <= 0.0 || b <= 0.0 {
        return a != b; // measured ↔ unmeasured is always news
    }
    ((a - b).abs() / a) >= 0.25
}

/// Measure every given encoder element (and the tone-map segment when
/// asked) at both resolutions. Blocking and slow by nature — ~40 s
/// worst case for three encoders plus tone-map — so callers run it on
/// a blocking thread, off the startup path.
pub fn measure(elements: &[&str], tonemap: bool) -> BenchResults {
    let mut out = BenchResults {
        gst: gst_version(),
        ..Default::default()
    };
    if crate::init().is_err() {
        return out;
    }
    for el in elements {
        let s = Speeds {
            s1080: measure_encoder(el, 1920, 1080),
            s2160: measure_encoder(el, 3840, 2160),
        };
        tracing::info!(
            element = el,
            at_1080 = s.s1080,
            at_2160 = s.s2160,
            "encoder speed measured"
        );
        out.encoders.insert((*el).to_string(), s);
    }
    if tonemap {
        let s = Speeds {
            s1080: measure_tonemap(1920, 1080),
            s2160: measure_tonemap(3840, 2160),
        };
        tracing::info!(
            at_1080 = s.s1080,
            at_2160 = s.s2160,
            "tone-map speed measured"
        );
        out.tonemap = Some(s);
    }
    out
}

/// A raw format this encoder takes directly, preferring NV12 (what
/// every hardware encoder here wants). Feeding the encoder's own
/// format keeps a colour conversion out of the measurement — the
/// benchmark is supposed to time the ENCODER, not our pixel plumbing,
/// and different boxes prefer different formats.
fn native_format(element: &str) -> Option<String> {
    let f = gst::ElementFactory::find(element)?;
    let mut formats = Vec::new();
    for t in f.static_pad_templates() {
        if t.direction() != gst::PadDirection::Sink {
            continue;
        }
        for st in t.caps().iter() {
            if st.name() != "video/x-raw" {
                continue;
            }
            match st.get::<gst::List>("format") {
                Ok(list) => formats.extend(list.iter().filter_map(|v| v.get::<String>().ok())),
                Err(_) => {
                    if let Ok(one) = st.get::<String>("format") {
                        formats.push(one);
                    }
                }
            }
        }
    }
    if formats.iter().any(|f| f == "NV12") {
        return Some("NV12".into());
    }
    formats.into_iter().next()
}

/// `appsrc(native fmt) ! ENC ! fakesink`, counting encoded buffers.
fn measure_encoder(element: &str, w: i32, h: i32) -> f32 {
    let Ok(enc) = gst::ElementFactory::make(element).build() else {
        return 0.0;
    };
    // Fall back to a converter only when the template says nothing
    // useful — an unnecessary conversion would be charged to the
    // encoder, which is exactly the mis-measurement to avoid.
    match native_format(element) {
        Some(fmt) => run_counting(&[enc], w, h, &fmt),
        None => match make("videoconvert") {
            Some(c) => run_counting(&[c, enc], w, h, "I420"),
            None => 0.0,
        },
    }
}

/// The real tone-map segment, fed 10-bit: the GL upload/download round
/// trip is the cost that made a J5005 0.65× (HUB-15a), and 8-bit input
/// would not exercise it honestly.
fn measure_tonemap(w: i32, h: i32) -> f32 {
    if !crate::remux::tonemap_available() {
        return 0.0;
    }
    let Some(convert) = make("videoconvert") else {
        return 0.0;
    };
    let mut chain = vec![convert];
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

/// Push pre-generated noise through `chain` as fast as it is accepted
/// and time the output. The source is a memcpy from the pool, so the
/// clock measures `chain`, not frame generation.
fn run_counting(chain: &[gst::Element], w: i32, h: i32, format: &str) -> f32 {
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
        return 0.0;
    };

    if pipe.add(src.upcast_ref::<gst::Element>()).is_err()
        || pipe.add(&sink).is_err()
        || pipe.add_many(chain).is_err()
    {
        return 0.0;
    }
    let mut all: Vec<&gst::Element> = vec![src.upcast_ref()];
    all.extend(chain.iter());
    all.push(&sink);
    if gst::Element::link_many(all).is_err() {
        let _ = pipe.set_state(gst::State::Null);
        return 0.0;
    }

    // Count on the LAST chain element's src pad: what came out of the
    // thing under test, after any parser in the segment.
    let frames = std::sync::Arc::new(AtomicU64::new(0));
    let started: std::sync::Arc<std::sync::Mutex<Option<Instant>>> = Default::default();
    let Some(out_pad) = chain.last().and_then(|e| e.static_pad("src")) else {
        let _ = pipe.set_state(gst::State::Null);
        return 0.0;
    };
    {
        let (frames, started) = (frames.clone(), started.clone());
        out_pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            // The clock starts at the first buffer: preroll, GL context
            // creation and encoder init are one-off, sustain is not.
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
        return 0.0;
    }

    // Feed from this thread: push_buffer blocks on the appsrc queue,
    // so the encoder's own speed paces the loop.
    let spf = gst::ClockTime::SECOND.nseconds() as f64 / REFERENCE_FPS as f64;
    let deadline = Instant::now() + CAP;
    for i in 0..FRAMES {
        if Instant::now() >= deadline {
            break;
        }
        let mut buf = pool[i as usize % POOL].clone();
        {
            let b = buf.make_mut();
            b.set_pts(gst::ClockTime::from_nseconds((i as f64 * spf) as u64));
            b.set_duration(gst::ClockTime::from_nseconds(spf as u64));
        }
        if src.push_buffer(buf).is_err() {
            break;
        }
    }
    let _ = src.end_of_stream();

    // Drain what is still in flight, bounded by the same cap.
    if let Some(bus) = pipe.bus() {
        let left = deadline.saturating_duration_since(Instant::now());
        let _ = bus.timed_pop_filtered(
            gst::ClockTime::from_nseconds(left.as_nanos() as u64),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
    }
    let elapsed = started
        .lock()
        .unwrap()
        .map(|t| t.elapsed())
        .unwrap_or_default();
    let n = frames.load(Ordering::Relaxed);
    let _ = pipe.set_state(gst::State::Null);
    // n counts buffers AFTER the first, and `speed` subtracts one more
    // for the interval count — hand it the total. `ran` is true when
    // the encoder produced anything at all: that distinguishes "so slow
    // it managed one frame in the whole window" from "never started".
    let produced_any = started.lock().unwrap().is_some();
    speed(n + 1, elapsed, produced_any)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_math_and_degenerate_inputs() {
        // 24 fps produced in one second of wall time = 1.0× realtime.
        // 25 buffers = 24 intervals.
        assert!((speed(25, Duration::from_secs(1), true) - 1.0).abs() < 0.001);
        assert!((speed(25, Duration::from_secs(4), true) - 0.25).abs() < 0.001);
        assert!((speed(241, Duration::from_secs(1), true) - 10.0).abs() < 0.001);

        // Ran but produced one lonely frame: catastrophically slow, NOT
        // unmeasured. Reporting 0.0 here would read downstream as "no
        // data, assume sufficient" and send 4K AV1 to a J5005 — the
        // exact box that cannot do it (measured live).
        assert_eq!(speed(1, Duration::from_secs(5), true), SPEED_FLOOR);
        const { assert!(SPEED_FLOOR > 0.0 && SPEED_FLOOR < 0.01) };

        // Never produced anything: genuinely unmeasured.
        assert_eq!(speed(0, Duration::from_secs(1), true), 0.0);
        assert_eq!(speed(1, Duration::from_secs(1), false), 0.0);
        assert_eq!(speed(100, Duration::ZERO, false), 0.0);
    }

    #[test]
    fn speeds_bucket_by_height() {
        let s = Speeds {
            s1080: 6.0,
            s2160: 2.0,
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
                s1080: 6.2,
                s2160: 2.1,
            },
        );
        r.tonemap = Some(Speeds {
            s1080: 3.0,
            s2160: 0.65,
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
    fn drift_threshold() {
        let base = |v: f32| {
            let mut r = BenchResults {
                gst: gst_version(),
                ..Default::default()
            };
            r.encoders.insert(
                "x265enc".into(),
                Speeds {
                    s1080: v,
                    s2160: v / 3.0,
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
            s1080: 2.0,
            s2160: 0.6,
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
        assert!(
            s.s1080 > 0.0 && s.s1080.is_finite(),
            "{el} measured {s:?} at 1080p"
        );
        assert!(s.s2160 >= 0.0 && s.s2160.is_finite(), "{el} 2160p: {s:?}");
        // Sanity: nothing on earth encodes 1080p at 10000× realtime.
        assert!(s.s1080 < 10_000.0, "{el} implausible: {s:?}");
    }
}
