//! In-hub remuxer (AR-10, §4.6): repackage supported streams into HLS with
//! **no re-encoding and no transcoder** — `appsrc ! parsebin ! <hls sink>`.
//! Parsing and repackaging elementary streams costs a few % CPU.
//!
//! The HLS sink follows the plugin-fallback strategy (see HLS_SINKS):
//! hlssink3 when installed, hlssink2 otherwise. TS segments either way
//! (the HLS baseline, HUB-17); fMP4/CMAF via cmafmux is a future upgrade.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSrc;

/// Caps structure names mpegtsmux can actually carry, read from its own
/// sink pad templates. Never hand-list what the element can tell us: a
/// hardcoded list shipped eac3 (which mpegtsmux rejects at runtime →
/// opaque not-negotiated) and omitted dts/opus (which it happily muxes).
fn ts_muxable_names() -> &'static std::collections::HashSet<String> {
    static NAMES: std::sync::OnceLock<std::collections::HashSet<String>> =
        std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names = std::collections::HashSet::new();
        let _ = crate::init();
        let Some(factory) = gst::ElementFactory::find("mpegtsmux") else {
            return names; // doctor already warns; remux will bail cleanly
        };
        for tmpl in factory.static_pad_templates() {
            if tmpl.direction() == gst::PadDirection::Sink {
                for s in tmpl.caps().iter() {
                    names.insert(s.name().to_string());
                }
            }
        }
        // The templates advertise these, but at runtime the muxer refuses
        // them unless enable-custom-mappings=true — and no browser plays
        // AV1/VP9-in-TS anyway. Treat as needs-transcoder, not muxable.
        names.remove("video/x-av1");
        names.remove("video/x-vp9");
        names
    })
}

/// Which muxer pad kind a parsed stream belongs on, if TS can carry it.
fn ts_compatible(caps_name: &str) -> Option<&'static str> {
    if !ts_muxable_names().contains(caps_name) {
        return None;
    }
    if caps_name.starts_with("video/") {
        Some("video")
    } else if caps_name.starts_with("audio/") {
        Some("audio")
    } else {
        None
    }
}

/// Normalized codec name (from discovery) → caps structure name.
/// Codecs discovery couldn't normalize pass through as raw caps names
/// (`video/x-divx`, `video/x-msmpeg`, …) — usable directly for decoder
/// lookups, so old exotics still plan as Encode instead of dropping.
fn codec_to_caps_name<'a>(kind: &str, codec: &'a str) -> Option<&'a str> {
    if codec.contains('/') {
        return Some(codec);
    }
    Some(match (kind, codec) {
        ("video", "h264") => "video/x-h264",
        ("video", "hevc") => "video/x-h265",
        ("video", "av1") => "video/x-av1",
        ("video", "vp9") => "video/x-vp9",
        ("video", "mpeg") => "video/mpeg",
        ("audio", "aac" | "mp3" | "mpeg-audio") => "audio/mpeg",
        ("audio", "ac3") => "audio/x-ac3",
        ("audio", "eac3") => "audio/x-eac3",
        ("audio", "dts") => "audio/x-dts",
        ("audio", "opus") => "audio/x-opus",
        ("audio", "flac") => "audio/x-flac",
        ("audio", "truehd") => "audio/x-true-hd",
        ("audio", "vorbis") => "audio/x-vorbis",
        _ => return None,
    })
}

/// AAC encoders in preference order (fdk has the best quality). Used via
/// [`aac_encoder`], which also dry-run-verifies the winner (TC-1: a broken
/// element is discovered at startup, not mid-session).
pub const AAC_ENCODERS: &[&str] = &["fdkaacenc", "avenc_aac", "voaacenc"];

/// Best available AAC encoder, verified once by a dry-run pipeline
/// (`audiotestsrc ! ... ! encoder ! fakesink` to EOS). None → no audio
/// transcoding on this machine.
pub fn aac_encoder() -> Option<&'static str> {
    static VERIFIED: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *VERIFIED.get_or_init(|| {
        let _ = crate::init();
        AAC_ENCODERS.iter().copied().find(|name| {
            if gst::ElementFactory::find(name).is_none() {
                return false;
            }
            let ok = dry_run_encoder(name);
            if !ok {
                tracing::warn!(encoder = name, "AAC encoder failed dry-run; trying next");
            }
            ok
        })
    })
}

/// H.264 encoders in preference order: hardware first (VA-API, NVENC,
/// QSV, VideoToolbox), then software. Dry-run verification is what makes this list
/// safe — a hw element on a box without the driver fails the probe and
/// the next one wins (TC-1/TC-6).
pub const H264_ENCODERS: &[&str] =
    &[
    "vah264enc",
    "vaapih264enc",
    "nvh264enc",
    "qsvh264enc",
    "vtenc_h264_hw", // VideoToolbox (Apple Silicon)
    "vtenc_h264",
    "x264enc",
    "openh264enc",
];

/// Best available H.264 encoder, dry-run-verified once. None → this box
/// cannot transcode video.
pub fn h264_encoder() -> Option<&'static str> {
    static VERIFIED: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *VERIFIED.get_or_init(|| {
        let _ = crate::init();
        H264_ENCODERS.iter().copied().find(|name| {
            if gst::ElementFactory::find(name).is_none() {
                return false;
            }
            let ok = dry_run_video_encoder(name);
            if !ok {
                tracing::warn!(encoder = name, "H.264 encoder failed dry-run; trying next");
            }
            ok
        })
    })
}

/// Verified encoder capabilities for the transcoder's registration
/// report (TC-1): (codec, element, hardware) triples that survived a
/// dry run. Hardware = anything before the software entries in the
/// preference lists (placement prefers hw boxes).
pub fn encoder_capabilities() -> Vec<(&'static str, &'static str, bool)> {
    const SW_VIDEO: &[&str] = &["x264enc", "openh264enc"];
    let mut caps = Vec::new();
    if let Some(el) = h264_encoder() {
        caps.push(("h264", el, !SW_VIDEO.contains(&el)));
    }
    if let Some(el) = aac_encoder() {
        caps.push(("aac", el, false));
    }
    caps
}

fn dry_run_video_encoder(name: &str) -> bool {
    // No forced pixel format: encoders differ (x264enc takes I420,
    // nvh264enc only NV12/RGBA-family) — videoconvert lets negotiation
    // pick whatever the encoder accepts, exactly like the real pipeline.
    dry_run(&format!(
        "videotestsrc num-buffers=5 ! video/x-raw,width=320,height=240 ! videoconvert ! {name} ! fakesink"
    ))
}

fn dry_run_encoder(name: &str) -> bool {
    dry_run(&format!(
        "audiotestsrc num-buffers=5 ! audioconvert ! audioresample ! {name} ! fakesink"
    ))
}

fn dry_run(launch: &str) -> bool {
    let Ok(p) = gst::parse::launch(launch) else {
        return false;
    };
    if p.set_state(gst::State::Playing).is_err() {
        return false;
    }
    let ok = p
        .bus()
        .and_then(|bus| {
            bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
        })
        .is_some_and(|msg| msg.type_() == gst::MessageType::Eos);
    let _ = p.set_state(gst::State::Null);
    ok
}

/// Source caps names of one kind, for decode-fit placement.
pub fn source_caps_names(kind: &str, info: &kahawai_core::media::MediaInfo) -> Vec<String> {
    let codecs: Vec<&str> = match kind {
        "video" => info.video.iter().map(|v| v.codec.as_str()).collect(),
        _ => info.audio.iter().map(|a| a.codec.as_str()).collect(),
    };
    codecs
        .into_iter()
        .filter_map(|c| codec_to_caps_name(kind, c))
        .map(str::to_string)
        .collect()
}

/// Every caps name the installed decoders can sink, for the transcoder
/// capability report (registry-derived, never hand-listed).
pub fn decoder_caps_names() -> Vec<String> {
    let _ = crate::init();
    let mut names: Vec<String> = gst::ElementFactory::factories_with_type(
        gst::ElementFactoryType::DECODER,
        gst::Rank::MARGINAL,
    )
    .iter()
    .flat_map(|f| {
        f.static_pad_templates()
            .into_iter()
            .filter(|t| t.direction() == gst::PadDirection::Sink)
            .flat_map(|t| t.caps().iter().map(|s| s.name().to_string()).collect::<Vec<_>>())
            .collect::<Vec<_>>()
    })
    .collect();
    names.sort();
    names.dedup();
    names
}

/// Can any installed decoder take this stream? Derived from the element
/// registry (never hand-list what it can tell us).
fn can_decode(caps_name: &str) -> bool {
    let caps = gst::Caps::new_empty_simple(caps_name);
    gst::ElementFactory::factories_with_type(
        gst::ElementFactoryType::DECODER,
        gst::Rank::MARGINAL,
    )
    .iter()
    .any(|f| f.can_sink_any_caps(&caps))
}

/// What happens to one stream kind in a session (HUB-16 decision order:
/// copy what the client and muxer both take, encode what they don't but
/// a decoder can read, drop the rest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamMode {
    Copy,
    /// Decode → re-encode to the target codec (h264 video / AAC audio).
    Encode,
    Off,
}

/// What the receiving client can actually decode (HUB-14). Muxability
/// alone lies: mpegtsmux happily carries MPEG-4 Part 2 and DTS, but no
/// browser plays either — copy must satisfy the client AND the muxer.
pub struct Target {
    pub video: &'static [&'static str],
    pub audio: &'static [&'static str],
}

/// hls.js/MSE baseline: H.264 video; AAC or MP3 audio.
pub const WEB_TARGET: Target = Target { video: &["h264"], audio: &["aac", "mp3"] };

/// Per-kind session plan — the single source of truth shared between
/// session planning and pipeline routing, so the muxer pads requested up
/// front always match the streams that will actually be linked.
#[derive(Debug, Clone, Copy)]
pub struct RemuxPlan {
    pub video: StreamMode,
    pub audio: StreamMode,
}

impl RemuxPlan {
    pub fn has_video(&self) -> bool {
        self.video != StreamMode::Off
    }
    pub fn has_audio(&self) -> bool {
        self.audio != StreamMode::Off
    }
    /// Anything to produce at all?
    pub fn playable(&self) -> bool {
        self.has_video() || self.has_audio()
    }
}

pub fn plan_streams(info: &kahawai_core::media::MediaInfo, target: &Target) -> RemuxPlan {
    let names = ts_muxable_names();
    let copyable = |kind: &str, codec: &str, accepted: &[&str]| {
        accepted.contains(&codec)
            && codec_to_caps_name(kind, codec).is_some_and(|n| names.contains(n))
    };
    let video = if info.video.iter().any(|v| copyable("video", &v.codec, target.video)) {
        StreamMode::Copy
    } else if h264_encoder().is_some()
        && info
            .video
            .iter()
            .any(|v| codec_to_caps_name("video", &v.codec).is_some_and(can_decode))
    {
        StreamMode::Encode
    } else {
        StreamMode::Off
    };
    let audio = if info.audio.iter().any(|a| copyable("audio", &a.codec, target.audio)) {
        StreamMode::Copy
    } else if aac_encoder().is_some()
        && info
            .audio
            .iter()
            .any(|a| codec_to_caps_name("audio", &a.codec).is_some_and(can_decode))
    {
        StreamMode::Encode
    } else {
        StreamMode::Off
    };
    RemuxPlan { video, audio }
}

/// Human-readable per-kind verdict for the playback-info overlay
/// (§4.3b spirit: the player reports which path was taken and why —
/// nothing converts silently).
pub fn plan_summary(
    info: &kahawai_core::media::MediaInfo,
    plan: &RemuxPlan,
) -> (String, String) {
    let names = ts_muxable_names();
    let kind_summary = |kind: &str,
                        codecs: Vec<&str>,
                        mode: StreamMode,
                        target_codec: &str| {
        match mode {
            StreamMode::Copy => codecs
                .iter()
                .find(|c| codec_to_caps_name(kind, c).is_some_and(|n| names.contains(n)))
                .map(|c| format!("{c} copy"))
                .unwrap_or_else(|| "copy".into()),
            StreamMode::Encode => {
                let src = codecs
                    .iter()
                    .find(|c| codec_to_caps_name(kind, c).is_some_and(can_decode))
                    .copied()
                    .unwrap_or(kind);
                format!("{src} → {target_codec} (transcoded)")
            }
            StreamMode::Off => {
                if codecs.is_empty() {
                    "none".into()
                } else {
                    format!("{} dropped (needs transcoder)", codecs[0])
                }
            }
        }
    };
    (
        kind_summary("video", info.video.iter().map(|v| v.codec.as_str()).collect(), plan.video, "h264"),
        kind_summary("audio", info.audio.iter().map(|a| a.codec.as_str()).collect(), plan.audio, "aac"),
    )
}

/// TS muxing needs specific stream-formats (h26x as Annex-B byte-stream,
/// AAC as ADTS) while containers store avc/hvc1/raw. A per-stream parser
/// between demux and muxer converts during caps negotiation — pure
/// repackaging, still no re-encoding.
fn parser_for(caps: &gst::CapsRef) -> Option<&'static str> {
    let s = caps.structure(0)?;
    let element = match s.name().as_str() {
        "video/x-h264" => "h264parse",
        "video/x-h265" => "h265parse",
        // mpegvideoparse only takes MPEG-1/2; Part 2 (DivX/XviD-era AVIs)
        // needs mpeg4videoparse or the muxer pad starves and the pipeline
        // hangs forever.
        "video/mpeg" => match s.get::<i32>("mpegversion").ok() {
            Some(4) => "mpeg4videoparse",
            _ => "mpegvideoparse",
        },
        "video/x-av1" => "av1parse",
        "video/x-vp9" => "vp9parse",
        "audio/mpeg" => match s.get::<i32>("mpegversion").ok() {
            Some(1) => "mpegaudioparse",
            _ => "aacparse",
        },
        "audio/x-ac3" | "audio/x-eac3" => "ac3parse",
        "audio/x-dts" => "dcaparse",
        "audio/x-opus" => "opusparse",
        _ => return None,
    };
    // Availability-guarded (plugin-fallback strategy): missing parser →
    // try a direct link rather than failing outright.
    gst::ElementFactory::find(element).is_some().then_some(element)
}

/// H.26x streams with B-frames need PTS/DTS recomputed from picture order
/// count, or mpegtsmux emits one frame out of decode order at each segment
/// boundary. mpv tolerates the DTS glitch; hls.js's MSE transmuxer rejects
/// the segment (`bufferAppendError`) and the browser shows garbage. The
/// timestamper fixes it at zero re-encode cost. Availability-guarded.
fn timestamper_for(caps: &gst::CapsRef) -> Option<&'static str> {
    let element = match caps.structure(0)?.name().as_str() {
        "video/x-h264" => "h264timestamper",
        "video/x-h265" => "h265timestamper",
        _ => return None,
    };
    gst::ElementFactory::find(element).is_some().then_some(element)
}

/// hlssink2 pads requested up front (splitmuxsink wants them before start);
/// each is taken by the first matching parsed stream.
type WaitingPads = Arc<Mutex<std::collections::HashMap<&'static str, gst::Pad>>>;

/// Offset-start gate: splitmuxsink is not flush-safe once it has seen
/// data (g_assert !ctx->is_reference aborts on a mid-GOP flush), so for
/// start_ms > 0 every pad feeding the HLS sink is blocked until the
/// initial seek has flushed through a still-virgin muxer.
struct SeekGate {
    blocked: Mutex<Vec<(gst::Pad, gst::PadProbeId)>>,
    triggered: std::sync::atomic::AtomicUsize,
    expected: usize,
}

impl SeekGate {
    fn new(expected: usize) -> Arc<Self> {
        Arc::new(Self {
            blocked: Mutex::new(Vec::new()),
            triggered: std::sync::atomic::AtomicUsize::new(0),
            expected,
        })
    }

    /// Block `pad` (a muxer feed) until [`open`]; counts the first
    /// arrival so the seek can wait for all branches to be negotiated.
    fn install(self: &Arc<Self>, pad: &gst::Pad) {
        let gate = self.clone();
        let counted = std::sync::atomic::AtomicBool::new(false);
        let id = pad
            .add_probe(
                gst::PadProbeType::BLOCK | gst::PadProbeType::BUFFER,
                move |_, _| {
                    if !counted.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        gate.triggered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    gst::PadProbeReturn::Ok
                },
            )
            .unwrap();
        self.blocked.lock().unwrap().push((pad.clone(), id));
    }

    fn all_triggered(&self) -> bool {
        self.triggered.load(std::sync::atomic::Ordering::SeqCst) >= self.expected
    }

    fn open(&self) {
        for (pad, id) in self.blocked.lock().unwrap().drain(..) {
            pad.remove_probe(id);
        }
    }
}

/// Plumb a fresh parsebin pad. Muxable-looking streams are routed right
/// here, synchronously — elements built before data flows behave
/// differently from elements inserted mid-stream (h264parse merges
/// parameter-set AUs when added mid-flow and drains a timestampless PPS
/// runt at EOS; corpus-sweep regression), so the pre-roll path must stay
/// the pre-roll path. But advertised caps can also lie: mislabeled
/// tracks (E-AC-3 tag, AC-3 bitstream) are re-typed by parsebin's
/// internal parser only once data flows, and fakesinking them on the
/// advertised caps starved the muxer pad forever (corpus-sweep finding).
/// So only apparently-unmuxable streams defer routing to the real caps
/// event; the queue absorbs data while the decision waits, and the caps
/// event precedes the first buffer in the same streaming thread, so
/// deciding in the probe is race-free.
/// The plan's mode for a stream of these caps.
fn mode_for(caps_name: &str, plan: &RemuxPlan) -> StreamMode {
    if caps_name.starts_with("video/") {
        plan.video
    } else if caps_name.starts_with("audio/") {
        plan.audio
    } else {
        StreamMode::Off
    }
}

/// Would route_stream do something useful with a stream of these caps?
fn routable(caps_name: &str, plan: &RemuxPlan) -> bool {
    match mode_for(caps_name, plan) {
        StreamMode::Copy => ts_compatible(caps_name).is_some(),
        StreamMode::Encode => can_decode(caps_name),
        StreamMode::Off => false,
    }
}

fn plumb_parsed_pad(
    pipe: &gst::Pipeline,
    waiting: &WaitingPads,
    pad: &gst::Pad,
    plan: RemuxPlan,
    gate: &Option<Arc<SeekGate>>,
) {
    // queue: decouples the muxer from parsebin's threads (the aggregator
    // deadlocks without it). Default queue limits (1 MiB / 1 s) are far
    // too small: the HLS sink holds one branch back while waiting for a
    // keyframe-aligned cut on the other, and files with uneven track ends
    // or high bitrates deadlock (corpus-sweep finding). Bound by bytes
    // only — generous enough for real interleave skew, still OOM-safe.
    let queue = gst::ElementFactory::make("queue")
        .property("max-size-bytes", 64u32 * 1024 * 1024)
        .property("max-size-buffers", 0u32)
        .property("max-size-time", 0u64)
        .build()
        .unwrap();
    pipe.add(&queue).unwrap();
    queue.sync_state_with_parent().unwrap();
    pad.link(&queue.static_pad("sink").unwrap()).unwrap();
    let qsrc = queue.static_pad("src").unwrap();

    let advertised = pad
        .stream()
        .and_then(|s| s.caps())
        .or_else(|| pad.current_caps())
        .unwrap_or_else(gst::Caps::new_empty);
    let name = advertised.structure(0).map(|s| s.name().to_string()).unwrap_or_default();
    if routable(&name, &plan) {
        route_stream(pipe, waiting, &qsrc, &advertised, plan, gate);
        return;
    }

    let pipe = pipe.clone();
    let waiting = waiting.clone();
    let gate = gate.clone();
    qsrc.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |qpad, info| {
        if let Some(gst::PadProbeData::Event(ev)) = &info.data
            && let gst::EventView::Caps(c) = ev.view()
            && qpad.peer().is_none()
        {
            route_stream(&pipe, &waiting, qpad, &c.caps_owned(), plan, &gate);
        }
        gst::PadProbeReturn::Ok
    });
}

/// Route a stream to the muxer (via parser/timestamper) or a fakesink,
/// now that its negotiated caps are known.
/// hlssink3 (≤0.15.3, imp.rs:304) unwraps the PTS of each fragment's
/// first buffer; a PTS-less frame (old AVI streams, parser EOS drains)
/// aborts the whole process — a Rust panic in an FFI callback cannot
/// unwind. Guard every pad that feeds the sink: borrow the DTS, or drop
/// the buffer.
fn guard_pts(pad: &gst::Pad) {
    pad.add_probe(gst::PadProbeType::BUFFER, |_, info| {
        if let Some(gst::PadProbeData::Buffer(buffer)) = &mut info.data {
            // ponytail: pts=dts misorders B-frames on the copy path
            // (sweep flags those [bad dts] → they plan as Encode now);
            // dropping instead starves fragments and trips more panics.
            if buffer.pts().is_none() {
                match buffer.dts() {
                    Some(dts) => buffer.make_mut().set_pts(dts),
                    None => return gst::PadProbeReturn::Drop,
                }
            }
        }
        gst::PadProbeReturn::Ok
    });
}

fn route_stream(
    pipe: &gst::Pipeline,
    waiting: &WaitingPads,
    from: &gst::Pad,
    caps: &gst::Caps,
    plan: RemuxPlan,
    gate: &Option<Arc<SeekGate>>,
) {
    let caps_name = caps.structure(0).map(|s| s.name().to_string()).unwrap_or_default();
    let mode = mode_for(&caps_name, &plan);
    // Encode: claim the kind's muxer pad for the decode→re-encode branch.
    if mode == StreamMode::Encode && can_decode(&caps_name) {
        let kind = if caps_name.starts_with("video/") { "video" } else { "audio" };
        if let Some(sinkpad) = waiting.lock().unwrap().remove(kind) {
            tracing::info!(caps = %caps_name, kind, "transcoding stream");
            if kind == "video" {
                build_video_encode_chain(pipe, from, sinkpad, gate);
            } else {
                build_audio_encode_chain(pipe, from, sinkpad, &caps_name, gate);
            }
            return;
        }
    }
    let target = (mode == StreamMode::Copy)
        .then(|| ts_compatible(&caps_name).and_then(|kind| waiting.lock().unwrap().remove(kind)))
        .flatten();
    match target {
        Some(sinkpad) => {
            let mut tail = from.clone();
            // parser → timestamper, each present only when it applies;
            // every hop is pure repackaging, no decode.
            for name in [parser_for(caps), timestamper_for(caps)].into_iter().flatten() {
                let el = gst::ElementFactory::make(name).build().unwrap();
                pipe.add(&el).unwrap();
                el.sync_state_with_parent().unwrap();
                tail.link(&el.static_pad("sink").unwrap()).unwrap();
                tail = el.static_pad("src").unwrap();
            }
            guard_pts(&tail);
            if let Some(g) = gate {
                g.install(&tail);
            }
            if let Err(e) = tail.link(&sinkpad) {
                tracing::warn!(caps = %caps_name, error = %e, "remux: pad link failed");
            }
        }
        None => {
            tracing::info!(caps = %caps_name, "remux: dropping stream (not TS-compatible or duplicate)");
            // sync=false: don't pace the dropped stream at realtime speed;
            // async=false: don't hold pipeline preroll hostage to a sparse
            // track (subtitles) that may not produce a buffer for minutes
            // (sweep finding: multi-track files deadlocked in PAUSED).
            let fake = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .property("async", false)
                .build()
                .unwrap();
            pipe.add(&fake).unwrap();
            fake.sync_state_with_parent().unwrap();
            if let Err(e) = from.link(&fake.static_pad("sink").unwrap()) {
                tracing::warn!(caps = %caps_name, error = %e, "remux: fakesink link failed");
            }
        }
    }
}

/// decodebin → videoconvert → H.264 encoder → h264parse (byte-stream
/// for the TS muxer). Rescues codecs no browser decodes (MPEG-4 Part 2,
/// AV1/VP9-in-TS). videoconvert costs one GPU→CPU hop with hw decoders;
/// ponytail: cudaconvert zero-copy path when both ends are NVENC/NVDEC.
fn build_video_encode_chain(
    pipe: &gst::Pipeline,
    from: &gst::Pad,
    sinkpad: gst::Pad,
    gate: &Option<Arc<SeekGate>>,
) {
    let Some(enc_name) = h264_encoder() else {
        tracing::error!("video encode routed with no verified H.264 encoder");
        return;
    };
    let decode = gst::ElementFactory::make("decodebin").build().unwrap();
    // Hardware decoders output device memory (nvav1dec → CUDAMemory,
    // 10-bit P010) that videoconvert cannot take. NVENC target: stay on
    // the GPU end to end (cudaupload passes CUDA through, uploads system
    // memory; cudaconvert handles format — zero copy for NVDEC→NVENC).
    // Other targets: cudadownload first (passthrough for system memory),
    // then videoconvert. All availability-guarded.
    let converter_names: Vec<&str> = if enc_name.starts_with("nv") {
        // videoconvert first: exotic decoder outputs (palettized RGB8P
        // from msrle-era AVIs) never reach the CUDA elements, which only
        // take common formats; it is passthrough for anything sane. Costs
        // NVDEC→NVENC zero-copy (CUDA output can't cross videoconvert, so
        // hw decoders fall back to system memory) — measured acceptable.
        vec!["videoconvert", "cudaupload", "cudaconvert"]
    } else {
        vec!["cudadownload", "videoconvert"]
    }
    .into_iter()
    .filter(|n| gst::ElementFactory::find(n).is_some())
    .collect();
    let converters: Vec<gst::Element> = converter_names
        .iter()
        .map(|n| gst::ElementFactory::make(n).build().unwrap())
        .collect();
    let enc = gst::ElementFactory::make(enc_name).build().unwrap();
    // Sane defaults, guarded per element (props differ across encoders).
    // nvh264enc/x264enc take kbit/s.
    set_prop_str_if_present(&enc, "bitrate", "6000");
    let parse = gst::ElementFactory::make("h264parse").build().unwrap();

    let mut chain: Vec<&gst::Element> = converters.iter().collect();
    chain.push(&enc);
    chain.push(&parse);
    pipe.add(&decode).unwrap();
    pipe.add_many(chain.iter().copied()).unwrap();
    gst::Element::link_many(chain.iter().copied()).unwrap();
    decode.sync_state_with_parent().unwrap();
    for el in &chain {
        el.sync_state_with_parent().unwrap();
    }
    let out = parse.static_pad("src").unwrap();
    guard_pts(&out);
    if let Some(g) = gate {
        g.install(&out);
    }
    if let Err(e) = out.link(&sinkpad) {
        tracing::warn!(error = %e, "video encode chain → muxer link failed");
    }
    let convert_sink = chain[0].static_pad("sink").unwrap();
    decode.connect_pad_added(move |_, pad| {
        if convert_sink.is_linked() {
            return; // first decoded stream wins
        }
        if let Err(e) = pad.link(&convert_sink) {
            tracing::warn!(error = %e, "decodebin → video encode chain link failed");
        }
    });
    if let Err(e) = from.link(&decode.static_pad("sink").unwrap()) {
        tracing::warn!(error = %e, "→ decodebin link failed");
    }
}

/// decodebin (auto-picks the best-ranked decoder — registry-derived, per
/// the fallback strategy) → audioconvert → audioresample → AAC encoder →
/// aacparse (raw→ADTS for the TS muxer) → muxer pad. The only decode/
/// encode work in the hub, and audio-only by design: a few % CPU.
fn build_audio_encode_chain(
    pipe: &gst::Pipeline,
    from: &gst::Pad,
    sinkpad: gst::Pad,
    caps_name: &str,
    gate: &Option<Arc<SeekGate>>,
) {
    let Some(enc_name) = aac_encoder() else {
        // Planner guarantees this; guard anyway (fakesink beats a stall).
        tracing::error!("audio encode routed with no verified AAC encoder");
        return;
    };
    // The AC-3 family's caps cannot be trusted: ac3parse labels E-AC-3
    // dependent-substream tracks (DD+ 7.1) as plain AC-3, and decodebin
    // then plugs a52dec, which dies on every block (corpus finding:
    // Despicable Me 3 / Super Mario). libav's eac3 decoder handles both
    // syntaxes, so for ac3/eac3 caps force it via a caps rewrite instead
    // of trusting autoplug. Availability-guarded; decodebin otherwise.
    let ac3_family = matches!(caps_name, "audio/x-ac3" | "audio/x-eac3");
    if ac3_family && gst::ElementFactory::find("avdec_eac3").is_some() {
        let setter = gst::ElementFactory::make("capssetter")
            .property("caps", gst::Caps::new_empty_simple("audio/x-eac3"))
            .property("join", false)
            .property("replace", true)
            .build()
            .unwrap();
        let dec = gst::ElementFactory::make("avdec_eac3").build().unwrap();
        build_audio_tail(pipe, from, sinkpad, enc_name, &[setter, dec], gate);
        return;
    }
    let decode = gst::ElementFactory::make("decodebin").build().unwrap();
    let convert = gst::ElementFactory::make("audioconvert").build().unwrap();
    let resample = gst::ElementFactory::make("audioresample").build().unwrap();
    let enc = gst::ElementFactory::make(enc_name).build().unwrap();
    set_prop_str_if_present(&enc, "bitrate", "192000");
    let parse = gst::ElementFactory::make("aacparse").build().unwrap();

    pipe.add_many([&decode, &convert, &resample, &enc, &parse]).unwrap();
    gst::Element::link_many([&convert, &resample, &enc, &parse]).unwrap();
    for el in [&decode, &convert, &resample, &enc, &parse] {
        el.sync_state_with_parent().unwrap();
    }
    let out = parse.static_pad("src").unwrap();
    guard_pts(&out);
    if let Some(g) = gate {
        g.install(&out);
    }
    if let Err(e) = out.link(&sinkpad) {
        tracing::warn!(error = %e, "remux: encode chain → muxer link failed");
    }
    let convert_sink = convert.static_pad("sink").unwrap();
    decode.connect_pad_added(move |_, pad| {
        if convert_sink.is_linked() {
            return; // first decoded stream wins
        }
        if let Err(e) = pad.link(&convert_sink) {
            tracing::warn!(error = %e, "remux: decodebin → encode chain link failed");
        }
    });
    if let Err(e) = from.link(&decode.static_pad("sink").unwrap()) {
        tracing::warn!(error = %e, "remux: → decodebin link failed");
    }
}

/// Static front-end variant of the audio encode chain: `from` →
/// front elements → audioconvert → audioresample → encoder → aacparse →
/// muxer pad. Used when the decoder must be chosen explicitly instead of
/// trusting decodebin's caps-based autoplug.
fn build_audio_tail(
    pipe: &gst::Pipeline,
    from: &gst::Pad,
    sinkpad: gst::Pad,
    enc_name: &str,
    front: &[gst::Element],
    gate: &Option<Arc<SeekGate>>,
) {
    let convert = gst::ElementFactory::make("audioconvert").build().unwrap();
    let resample = gst::ElementFactory::make("audioresample").build().unwrap();
    let enc = gst::ElementFactory::make(enc_name).build().unwrap();
    set_prop_str_if_present(&enc, "bitrate", "192000");
    let parse = gst::ElementFactory::make("aacparse").build().unwrap();

    let mut chain: Vec<&gst::Element> = front.iter().collect();
    chain.extend([&convert, &resample, &enc, &parse]);
    pipe.add_many(chain.iter().copied()).unwrap();
    gst::Element::link_many(chain.iter().copied()).unwrap();
    for el in &chain {
        el.sync_state_with_parent().unwrap();
    }
    let out = parse.static_pad("src").unwrap();
    guard_pts(&out);
    if let Some(g) = gate {
        g.install(&out);
    }
    if let Err(e) = out.link(&sinkpad) {
        tracing::warn!(error = %e, "remux: encode chain → muxer link failed");
    }
    if let Err(e) = from.link(&chain[0].static_pad("sink").unwrap()) {
        tracing::warn!(error = %e, "remux: → decoder link failed");
    }
}

/// Byte source for a remux: sized and random-access, because real-world
/// containers demand seeks (MP4 with the moov atom at the end cannot be
/// demuxed as a forward-only stream — the demuxer must jump to the tail
/// for its index before streaming the data). The hub backs this with a
/// mediahost read lease; tools back it with a local file.
pub trait RemuxSource: Send + 'static {
    fn size(&self) -> u64;
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize>;
}

/// Local-file source (sweep tool, tests).
pub struct FileSource {
    file: std::fs::File,
    size: u64,
}

impl FileSource {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Self { file, size })
    }
}

impl RemuxSource for FileSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::{Read, Seek, SeekFrom};
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read(buf)
    }
}

enum FeedCmd {
    /// (bytes wanted, feed generation at request time) — a Need stamped
    /// before a seek must never be served after it: with slow sources
    /// (lease/socket) a stale read can land after flush-stop and push
    /// old-position bytes into the new segment, which the demuxer then
    /// parses as garbage ("large block, file might be corrupt").
    Need(u32, u64),
}

pub struct RemuxJob {
    pipeline: gst::Pipeline,
    error: Arc<Mutex<Option<String>>>,
    finished: Arc<std::sync::atomic::AtomicBool>,
}

/// HLS sink elements in preference order: hlssink3 (gst-plugins-rs, better
/// maintained, richer playlist control) over hlssink2 (plugins-bad).
/// Plugin-fallback strategy: pick the best available at runtime, set
/// properties guarded by existence so element/version differences degrade
/// instead of panicking, and let `doctor` recommend the preferred one.
pub const HLS_SINKS: &[&str] = &["hlssink3", "hlssink2"];

/// Set a property only if this element (version) has it.
fn set_prop_if_present<V: Into<gst::glib::Value>>(el: &gst::Element, name: &str, value: V) {
    use gst::glib::prelude::ObjectExt;
    if el.find_property(name).is_some() {
        el.set_property_from_value(name, &value.into());
    } else {
        tracing::debug!(element = %el.name(), property = name, "property not present; skipped");
    }
}

/// Same, for enum properties set by nick.
fn set_prop_str_if_present(el: &gst::Element, name: &str, value: &str) {
    use gst::glib::prelude::ObjectExt;
    if el.find_property(name).is_some() {
        el.set_property_from_str(name, value);
    } else {
        tracing::debug!(element = %el.name(), property = name, "property not present; skipped");
    }
}

/// Best available HLS sink, configured for `out_dir`. Returns the element
/// and its factory name (for logs/tests).
fn make_hls_sink(out_dir: &Path, prefer: Option<&str>) -> Result<(gst::Element, &'static str)> {
    // An explicit preference (retry-after-sink-crash, TC-6) wins if the
    // element exists; otherwise the usual best-available order.
    let name = prefer
        .and_then(|p| {
            HLS_SINKS.iter().find(|n| **n == p && gst::ElementFactory::find(n).is_some())
        })
        .or_else(|| HLS_SINKS.iter().find(|n| gst::ElementFactory::find(n).is_some()))
        .context("no HLS sink element (hlssink3/hlssink2) — see `kahawai doctor`")?;
    let sink = gst::ElementFactory::make(name).build()?;
    set_prop_if_present(&sink, "location", out_dir.join("segment%05d.ts").to_str().unwrap());
    set_prop_if_present(&sink, "playlist-location", out_dir.join("master.m3u8").to_str().unwrap());
    set_prop_if_present(&sink, "target-duration", 4u32);
    // Keep every segment and playlist entry (VOD-style growing playlist).
    set_prop_if_present(&sink, "playlist-length", 0u32);
    set_prop_if_present(&sink, "max-files", 0u32); // hlssink2
    set_prop_if_present(&sink, "max-num-segment-files", 0u32); // hlssink3
    // EVENT: players may seek within already-produced segments while the
    // remux is still running (ENDLIST still lands at EOS). hlssink3 only.
    set_prop_str_if_present(&sink, "playlist-type", "event");
    tracing::info!(sink = name, "HLS sink selected");
    Ok((sink, name))
}

/// Start a remux/transcode writing `master.m3u8` + `segment*.ts` into
/// `out_dir`, pulling bytes from `source` on demand (seeks included).
/// The plan comes from discovery via [`plan_streams`] — the muxer pads
/// must be requested before the pipeline starts, and an unfed pad would
/// stall it.
pub fn start(out_dir: &Path, plan: RemuxPlan, source: Box<dyn RemuxSource>) -> Result<RemuxJob> {
    start_full(out_dir, plan, source, 0, None)
}

/// Like [`start`], seeking to `start_ms` (nearest keyframe at or before
/// it) before rolling — the §6 seek story: a seek beyond produced
/// segments is a pipeline restart at the target offset.
pub fn start_at(
    out_dir: &Path,
    plan: RemuxPlan,
    source: Box<dyn RemuxSource>,
    start_ms: u64,
) -> Result<RemuxJob> {
    start_full(out_dir, plan, source, start_ms, None)
}

/// Full-control variant: offset plus an HLS sink override (TC-6 retry).
pub fn start_full(
    out_dir: &Path,
    plan: RemuxPlan,
    mut source: Box<dyn RemuxSource>,
    start_ms: u64,
    sink: Option<&str>,
) -> Result<RemuxJob> {
    crate::init()?;

    let pipeline = gst::Pipeline::new();
    let appsrc = AppSrc::builder()
        .stream_type(gstreamer_app::AppStreamType::Seekable)
        .block(true)
        .max_bytes(8 * 1024 * 1024)
        .build();
    appsrc.set_size(source.size() as i64);

    // appsrc callbacks run on GStreamer threads and must not block on I/O;
    // they forward commands to a feeder thread that owns the source.
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<FeedCmd>();
    // Seeks apply synchronously in the callback (generation bump + new
    // position); the feeder picks both up before serving any Need.
    let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let seek_to: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    // Held by the feeder for the duration of each Need. seek_data takes
    // it after bumping the generation: any in-flight feed then finishes
    // inside the flush (its push fails Flushing — appsrc unblocks
    // producers before invoking seek_data), so a slow source read can
    // never land pre-seek bytes after flush-stop.
    let busy: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
    let gen_need = generation.clone();
    let gen_seek = generation.clone();
    let seek_cb = seek_to.clone();
    let busy_seek = busy.clone();
    appsrc.set_callbacks(
        gstreamer_app::AppSrcCallbacks::builder()
            .need_data(move |_, length| {
                let _ = cmd_tx.send(FeedCmd::Need(
                    length,
                    gen_need.load(std::sync::atomic::Ordering::SeqCst),
                ));
            })
            .seek_data(move |_, offset| {
                *seek_cb.lock().unwrap() = Some(offset);
                gen_seek.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                drop(busy_seek.lock().unwrap());
                true
            })
            .build(),
    );
    let feeder_src = appsrc.clone();
    let gen_feed = generation;
    let seek_feed = seek_to;
    std::thread::spawn(move || {
        let mut pos: u64 = 0;
        let mut at_eos = false;
        while let Ok(cmd) = cmd_rx.recv() {
            match cmd {
                FeedCmd::Need(length, stamp) => {
                    let _busy = busy.lock().unwrap();
                    if let Some(target) = seek_feed.lock().unwrap().take() {
                        pos = target;
                        at_eos = false;
                    }
                    if stamp != gen_feed.load(std::sync::atomic::Ordering::SeqCst) {
                        continue; // stamped before a seek: stale, drop
                    }
                    if at_eos {
                        continue;
                    }
                    let want = (length as usize).clamp(256 * 1024, 4 * 1024 * 1024);
                    let mut buf = vec![0u8; want];
                    match source.read_at(pos, &mut buf) {
                        Ok(0) => {
                            at_eos = true;
                            let _ = feeder_src.end_of_stream();
                        }
                        Ok(n) => {
                            buf.truncate(n);
                            let mut b = gst::Buffer::from_mut_slice(buf);
                            b.get_mut().unwrap().set_offset(pos);
                            pos += n as u64;
                            if feeder_src.push_buffer(b).is_err() {
                                // Flushing (seek in progress) or shutdown;
                                // either a Seek command follows or recv fails.
                                continue;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "remux source read failed; ending stream");
                            at_eos = true;
                            let _ = feeder_src.end_of_stream();
                        }
                    }
                }
            }
        }
    });
    let parsebin = gst::ElementFactory::make("parsebin").build()?;
    let (hlssink, _sink_name) = make_hls_sink(out_dir, sink)?;

    pipeline.add_many([appsrc.upcast_ref::<gst::Element>(), &parsebin, &hlssink])?;
    gst::Element::link_many([appsrc.upcast_ref::<gst::Element>(), &parsebin])?;

    // Request the muxer pads *now* — splitmuxsink inside hlssink2 must see
    // them before starting or it never leaves Ready.
    anyhow::ensure!(plan.playable(), "nothing to remux");
    let waiting: WaitingPads = Arc::new(Mutex::new(std::collections::HashMap::new()));
    if plan.has_video() {
        let pad = hlssink.request_pad_simple("video").context("requesting video pad")?;
        waiting.lock().unwrap().insert("video", pad);
    }
    if plan.has_audio() {
        let pad = hlssink.request_pad_simple("audio").context("requesting audio pad")?;
        waiting.lock().unwrap().insert("audio", pad);
    }

    // Every parsed stream gets a queue immediately (no buffer ever hits an
    // unlinked pad); routing to the pre-requested muxer pads happens per
    // stream once its real caps flow (see plumb_parsed_pad).
    let gate = (start_ms > 0)
        .then(|| SeekGate::new(plan.has_video() as usize + plan.has_audio() as usize));
    let pipe = pipeline.clone();
    let waiting2 = waiting.clone();
    let gate2 = gate.clone();
    parsebin.connect_pad_added(move |_, pad| {
        plumb_parsed_pad(&pipe, &waiting2, pad, plan, &gate2);
    });

    let error = Arc::new(Mutex::new(None::<String>));
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let bus = pipeline.bus().context("pipeline has no bus")?;
    let err2 = error.clone();
    let fin2 = finished.clone();
    // Watch the bus on a plain thread: EOS finalizes the playlist (ENDLIST).
    let pipeline2 = pipeline.clone();
    std::thread::spawn(move || {
        for msg in bus.iter_timed(gst::ClockTime::NONE) {
            match msg.view() {
                gst::MessageView::Eos(_) => {
                    let _ = pipeline2.set_state(gst::State::Null);
                    fin2.store(true, std::sync::atomic::Ordering::SeqCst);
                    break;
                }
                gst::MessageView::Error(e) => {
                    let text = format!("{} ({:?})", e.error(), e.debug());
                    tracing::error!(error = %text, "remux pipeline failed");
                    *err2.lock().unwrap() = Some(text);
                    let _ = pipeline2.set_state(gst::State::Null);
                    fin2.store(true, std::sync::atomic::Ordering::SeqCst);
                    break;
                }
                _ => {}
            }
        }
    });

    if let Some(gate) = &gate {
        // Offset start. splitmuxsink cannot survive a flush once it has
        // seen data (C assert aborts on mid-GOP flushes), so every muxer
        // feed is gated: roll toward PAUSED until all branches have data
        // blocked at the gates (source, demuxer and parsers are then
        // negotiated), seek through the still-virgin muxer, then open.
        pipeline.set_state(gst::State::Paused)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !gate.all_triggered() {
            if let Some(e) = error.lock().unwrap().clone() {
                anyhow::bail!("pipeline failed before offset seek: {e}");
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "streams never reached the seek gate"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // Straight at the demuxer: a pipeline-level seek routes through
        // the sinks, and the gated (unprerolled) HLS sink refuses it.
        let seek = gst::event::Seek::new(
            1.0,
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            gst::SeekType::Set,
            gst::ClockTime::from_mseconds(start_ms),
            gst::SeekType::None,
            gst::ClockTime::NONE,
        );
        anyhow::ensure!(parsebin.send_event(seek), "demuxer refused the start-offset seek");
        gate.open();
    }
    pipeline.set_state(gst::State::Playing)?;
    Ok(RemuxJob { pipeline, error, finished })
}

impl RemuxJob {
    pub fn failed(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    /// True once EOS or error fully processed (playlist finalized).
    pub fn finished(&self) -> bool {
        self.finished.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Hard stop (session teardown).
    pub fn stop(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Drop for RemuxJob {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const COPY_AV: RemuxPlan = RemuxPlan { video: StreamMode::Copy, audio: StreamMode::Copy };

    /// Manual repro: REMUX_SRC=/path/to/file cargo test -p kahawai-media \
    ///   remux_file_from_env -- --ignored --nocapture
    #[test]
    #[ignore]
    fn remux_file_from_env() {
        let src = std::path::PathBuf::from(std::env::var("REMUX_SRC").expect("set REMUX_SRC"));
        let out = tempfile::tempdir().unwrap();
        let info = crate::discover(&src, Duration::from_secs(30)).unwrap();
        let plan = plan_streams(&info, &WEB_TARGET);
        eprintln!("plan: {plan:?}");
        let job = start(out.path(), plan, Box::new(FileSource::open(&src).unwrap())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(120);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(job.failed().is_none(), "remux failed: {:?}", job.failed());
        assert!(job.finished(), "did not finish");
        let playlist = std::fs::read_to_string(out.path().join("master.m3u8")).unwrap();
        let segs = std::fs::read_dir(out.path()).unwrap().count();
        eprintln!("OK: {} entries in dir, ENDLIST={}", segs, playlist.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn ts_compat_follows_muxer_templates() {
        crate::init().unwrap();
        // Basics that any mpegtsmux supports.
        assert_eq!(ts_compatible("video/x-h264"), Some("video"));
        assert_eq!(ts_compatible("audio/mpeg"), Some("audio"));
        assert_eq!(ts_compatible("text/x-raw"), None);
        // Every answer must agree with the muxer's own template.
        let names = ts_muxable_names();
        assert_eq!(ts_compatible("audio/x-eac3").is_some(), names.contains("audio/x-eac3"));
        assert_eq!(ts_compatible("audio/x-dts").is_some(), names.contains("audio/x-dts"));

        // Flags derive from the same truth: eac3-only audio yields
        // has_audio only if the muxer takes eac3 (it does not, today).
        let info = kahawai_core::media::MediaInfo {
            video: vec![kahawai_core::media::VideoStream { codec: "hevc".into(), ..Default::default() }],
            audio: vec![kahawai_core::media::AudioStream { codec: "eac3".into(), ..Default::default() }],
            ..Default::default()
        };
        let plan = plan_streams(&info, &WEB_TARGET);
        // hevc is muxable but not in the web target: transcode or drop.
        assert_ne!(plan.video, StreamMode::Copy);
        // eac3 is neither web-playable nor muxable: never Copy.
        assert_ne!(plan.audio, StreamMode::Copy);
    }

    /// Corpus-sweep catch #2: one track ending well before the other used
    /// to deadlock the HLS sink against undersized queues.
    #[test]
    fn remuxes_uneven_track_ends() {
        crate::init().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("uneven.mkv");
        let p = gst::parse::launch(&format!(
            "videotestsrc num-buffers=100 ! video/x-raw,format=I420,width=320,height=240 ! x264enc speed-preset=ultrafast ! h264parse ! matroskamux name=m audiotestsrc num-buffers=200 ! audioconvert ! fdkaacenc ! m. m. ! filesink location=\"{}\"",
            src_path.display()
        ))
        .unwrap();
        p.set_state(gst::State::Playing).unwrap();
        p.bus().unwrap().timed_pop_filtered(gst::ClockTime::from_seconds(30), &[gst::MessageType::Eos]).unwrap();
        p.set_state(gst::State::Null).unwrap();

        let out = tempfile::tempdir().unwrap();
        let job = start(out.path(), COPY_AV, Box::new(FileSource::open(&src_path).unwrap())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "uneven-track remux deadlocked (queue-sizing regression)");
        assert!(job.failed().is_none(), "remux failed: {:?}", job.failed());
        assert!(std::fs::read_to_string(out.path().join("master.m3u8")).unwrap().contains("#EXT-X-ENDLIST"));
    }

    /// The corpus sweep's first catch: MP4 with the moov atom at the end
    /// (the mp4mux default, and common in the wild) cannot be demuxed as a
    /// forward-only stream — the seekable source is what makes it work.
    #[test]
    fn remuxes_nonfaststart_mp4() {
        crate::init().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("in.mp4");
        crate::testutil::render_h264_aac_mp4(&src_path);

        let out = tempfile::tempdir().unwrap();
        let job = start(out.path(), COPY_AV, Box::new(FileSource::open(&src_path).unwrap())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "moov-at-end mp4 remux did not finish (push-mode regression)");
        assert!(job.failed().is_none(), "remux failed: {:?}", job.failed());
        let playlist = std::fs::read_to_string(out.path().join("master.m3u8")).unwrap();
        assert!(playlist.contains("#EXT-X-ENDLIST"));
        assert!(playlist.contains("segment00000.ts"));
    }

    /// M3 slice 1: audio that TS cannot carry (E-AC-3) is transcoded to
    /// AAC in-hub while video passes through untouched.
    #[test]
    fn transcodes_eac3_audio_to_aac() {
        crate::init().unwrap();
        if !crate::testutil::has_element("avenc_eac3") {
            eprintln!("skipping: no avenc_eac3 to build the fixture");
            return;
        }
        if ts_muxable_names().contains("audio/x-eac3") {
            eprintln!("skipping: this mpegtsmux muxes eac3 natively");
            return;
        }
        if aac_encoder().is_none() {
            eprintln!("skipping: no verified AAC encoder");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("eac3.mkv");
        crate::testutil::render_h264_eac3_mkv(&src_path);

        let info = crate::discover(&src_path, Duration::from_secs(30)).unwrap();
        let plan = plan_streams(&info, &WEB_TARGET);
        assert_eq!(plan.audio, StreamMode::Encode, "eac3 should plan as Encode: {info:?}");
        assert_eq!(plan.video, StreamMode::Copy);

        let out = tempfile::tempdir().unwrap();
        let job = start(out.path(), plan, Box::new(FileSource::open(&src_path).unwrap())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "transcode did not finish");
        assert!(job.failed().is_none(), "transcode failed: {:?}", job.failed());
        assert!(std::fs::read_to_string(out.path().join("master.m3u8"))
            .unwrap()
            .contains("#EXT-X-ENDLIST"));

        // The produced segment must carry AAC audio and h264 video.
        let seg = crate::discover(&out.path().join("segment00000.ts"), Duration::from_secs(30))
            .unwrap();
        assert_eq!(seg.video.len(), 1, "video missing from transcoded segment: {seg:?}");
        assert_eq!(seg.video[0].codec, "h264");
        assert_eq!(seg.audio.len(), 1, "audio missing from transcoded segment: {seg:?}");
        assert_eq!(seg.audio[0].codec, "aac", "audio not transcoded to AAC: {seg:?}");
    }

    /// M3: video no browser decodes (MPEG-4 Part 2) is transcoded to
    /// H.264; the web target profile drives the plan (HUB-14/16).
    #[test]
    fn transcodes_mpeg4_video_to_h264() {
        crate::init().unwrap();
        if !crate::testutil::has_element("avenc_mpeg4") {
            eprintln!("skipping: no avenc_mpeg4 to build the fixture");
            return;
        }
        if h264_encoder().is_none() {
            eprintln!("skipping: no verified H.264 encoder");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("divx.mkv");
        crate::testutil::render(&format!(
            "videotestsrc num-buffers=125 ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! avenc_mpeg4 ! matroskamux name=m audiotestsrc num-buffers=215 ! audioconvert ! fdkaacenc ! m. m. ! filesink location=\"{}\"",
            src_path.display()
        ));

        let info = crate::discover(&src_path, Duration::from_secs(30)).unwrap();
        let plan = plan_streams(&info, &WEB_TARGET);
        assert_eq!(plan.video, StreamMode::Encode, "mpeg4 should plan as Encode: {info:?}");
        assert_eq!(plan.audio, StreamMode::Copy, "aac should copy: {info:?}");

        let out = tempfile::tempdir().unwrap();
        let job = start(out.path(), plan, Box::new(FileSource::open(&src_path).unwrap())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "video transcode did not finish");
        assert!(job.failed().is_none(), "video transcode failed: {:?}", job.failed());
        let seg =
            crate::discover(&out.path().join("segment00000.ts"), Duration::from_secs(30)).unwrap();
        assert_eq!(seg.video.first().map(|v| v.codec.as_str()), Some("h264"), "{seg:?}");
        assert_eq!(seg.audio.first().map(|a| a.codec.as_str()), Some("aac"), "{seg:?}");
    }

    /// §6 seek story: starting at an offset produces only the tail.
    #[test]
    fn starts_at_offset() {
        crate::init().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("in.mkv");
        crate::testutil::render_h264_aac_mkv(&src_path); // 10 s fixture

        let out = tempfile::tempdir().unwrap();
        let job = start_at(
            out.path(),
            COPY_AV,
            Box::new(FileSource::open(&src_path).unwrap()),
            6_000,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "offset remux did not finish");
        assert!(job.failed().is_none(), "offset remux failed: {:?}", job.failed());
        let playlist = std::fs::read_to_string(out.path().join("master.m3u8")).unwrap();
        assert!(playlist.contains("#EXT-X-ENDLIST"));
        let total: f64 = playlist
            .lines()
            .filter_map(|l| l.strip_prefix("#EXTINF:"))
            .filter_map(|l| l.trim_end_matches(',').parse::<f64>().ok())
            .sum();
        // 10 s source, started at 6 s (snapped to a keyframe at or
        // before): expect roughly the tail, never the whole file.
        assert!(
            total > 2.0 && total < 6.5,
            "expected ~4s tail, playlist covers {total}s:\n{playlist}"
        );
    }

    /// The crashing combo from the field: offset start + encode branch.
    /// splitmuxsink aborts on flushes after data; the seek gate must
    /// keep it virgin until the initial seek lands.
    #[test]
    fn starts_at_offset_with_encode_branch() {
        crate::init().unwrap();
        if aac_encoder().is_none() {
            eprintln!("skipping: no verified AAC encoder");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("in.mkv");
        crate::testutil::render_h264_flac_mkv(&src_path); // ~10 s

        let info = crate::discover(&src_path, Duration::from_secs(30)).unwrap();
        let plan = plan_streams(&info, &WEB_TARGET);
        assert_eq!(plan.audio, StreamMode::Encode, "flac should plan Encode: {info:?}");

        let out = tempfile::tempdir().unwrap();
        // The flac fixture is ~5 s; start at 2.5 s → expect a ~2.5 s tail.
        let job = start_at(out.path(), plan, Box::new(FileSource::open(&src_path).unwrap()), 2_500)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "offset encode remux did not finish");
        assert!(job.failed().is_none(), "offset encode remux failed: {:?}", job.failed());
        let playlist = std::fs::read_to_string(out.path().join("master.m3u8")).unwrap();
        let total: f64 = playlist
            .lines()
            .filter_map(|l| l.strip_prefix("#EXTINF:"))
            .filter_map(|l| l.trim_end_matches(',').parse::<f64>().ok())
            .sum();
        assert!(
            total > 1.5 && total < 4.0,
            "expected only the tail, playlist covers {total}s:\n{playlist}"
        );
    }

    #[test]
    fn hls_sink_selection_prefers_best_available() {
        crate::init().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (_, name) = make_hls_sink(dir.path(), None).unwrap();
        // An explicit preference wins when installed.
        if gst::ElementFactory::find("hlssink2").is_some() {
            let d2 = tempfile::tempdir().unwrap();
            let (_, forced) = make_hls_sink(d2.path(), Some("hlssink2")).unwrap();
            assert_eq!(forced, "hlssink2");
        }
        let expected = if gst::ElementFactory::find("hlssink3").is_some() {
            "hlssink3"
        } else {
            "hlssink2"
        };
        assert_eq!(name, expected);
    }

    #[test]
    fn remuxes_mkv_to_hls_without_reencoding() {
        crate::init().unwrap();
        // Fixture: h264 + AAC in MKV (both TS-compatible).
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("in.mkv");
        crate::testutil::render_h264_aac_mkv(&src_path);

        let out = tempfile::tempdir().unwrap();
        let job = start(out.path(), COPY_AV, Box::new(FileSource::open(&src_path).unwrap())).unwrap();

        let deadline = Instant::now() + Duration::from_secs(30);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "remux did not finish in time");
        assert!(job.failed().is_none(), "remux failed: {:?}", job.failed());

        let playlist = std::fs::read_to_string(out.path().join("master.m3u8")).unwrap();
        assert!(playlist.contains("segment00000.ts"), "playlist:\n{playlist}");
        assert!(playlist.contains("#EXT-X-ENDLIST"), "playlist not finalized");
        if gst::ElementFactory::find("hlssink3").is_some() {
            assert!(
                playlist.contains("#EXT-X-PLAYLIST-TYPE:EVENT"),
                "hlssink3 playlists must be EVENT for in-flight seeking:\n{playlist}"
            );
        }

        // The segment still carries h264 — remux, not transcode.
        let info = crate::discover(
            &out.path().join("segment00000.ts"),
            Duration::from_secs(15),
        )
        .unwrap();
        assert_eq!(info.container.as_deref(), Some("mpegts"));
        assert_eq!(info.video.len(), 1);
        assert_eq!(info.video[0].codec, "h264");
        assert_eq!(info.audio.first().map(|a| a.codec.as_str()), Some("aac"));

        // Every segment's video DTS must be monotonic. The bug (one frame
        // out of decode order) appears at segment boundaries *after* the
        // first, so all segments are checked — hls.js rejects the segment
        // otherwise (`bufferAppendError`). The fixture has B-frames so the
        // timestamper is genuinely exercised.
        let segs: Vec<_> = std::fs::read_dir(out.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "ts"))
            .collect();
        assert!(segs.len() >= 2, "need multiple segments to test boundaries");
        for seg in &segs {
            let (missing, non_mono) = video_dts_defects(seg);
            // Without the timestamper the first frames of a segment carry no
            // DTS (N/A); with B-frames the muxer can also emit them out of
            // decode order. Either makes hls.js reject the segment
            // (`bufferAppendError`) while mpv tolerates it.
            assert_eq!(missing, 0, "{}: {missing} video packets with no DTS", seg.display());
            assert_eq!(non_mono, 0, "{}: {non_mono} non-monotonic video DTS", seg.display());
        }
    }

    /// Ffprobe a segment's video packets; return `(missing_dts, non_monotonic)`.
    fn video_dts_defects(seg: &std::path::Path) -> (usize, usize) {
        let out = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v", "-show_entries",
                   "packet=dts", "-of", "csv=p=0"])
            .arg(seg)
            .output()
            .expect("ffprobe required for the remux DTS test");
        let (mut missing, mut non_mono) = (0usize, 0usize);
        let mut prev: Option<i64> = None;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let field = line.trim().trim_end_matches(',');
            if field.is_empty() {
                continue;
            }
            match field.parse::<i64>() {
                Ok(dts) => {
                    if prev.is_some_and(|p| dts < p) {
                        non_mono += 1;
                    }
                    prev = Some(dts);
                }
                Err(_) => missing += 1, // "N/A"
            }
        }
        (missing, non_mono)
    }
}
