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
use gstreamer_video as gst_video;

/// What a container's muxer can carry, read from its own sink pad
/// templates. Never hand-list what the element can tell us: a hardcoded
/// list shipped eac3 (which mpegtsmux rejects at runtime → opaque
/// not-negotiated) and omitted dts/opus (which it happily muxes).
///
/// Kept as CAPS, not names, because names alone cannot separate mp3
/// from AAC — both are `audio/mpeg`, and isofmp4mux's template takes
/// only `mpegversion=4` while mpegtsmux takes 1 as well. Answering that
/// from the template is what keeps the two muxers' differences in one
/// place instead of in a special case per call site.
pub(crate) fn muxable_caps(format: SegmentFormat) -> &'static gst::Caps {
    static TS: std::sync::OnceLock<gst::Caps> = std::sync::OnceLock::new();
    static FMP4: std::sync::OnceLock<gst::Caps> = std::sync::OnceLock::new();
    let sink_caps = |element: &str| {
        // Before the first Caps::new_empty(): building caps at all
        // asserts an initialized GStreamer.
        let _ = crate::init();
        let mut caps = gst::Caps::new_empty();
        let Some(factory) = gst::ElementFactory::find(element) else {
            return caps; // doctor already warns; remux will bail cleanly
        };
        for tmpl in factory.static_pad_templates() {
            if tmpl.direction() == gst::PadDirection::Sink {
                caps.merge(tmpl.caps().copy());
            }
        }
        caps
    };
    match format {
        SegmentFormat::Ts => TS.get_or_init(|| {
            let caps = sink_caps("mpegtsmux");
            // The templates advertise these, but at runtime the muxer
            // refuses them unless enable-custom-mappings=true — and no
            // browser plays AV1/VP9-in-TS anyway. Treat as
            // needs-transcoder, not muxable.
            let mut kept = gst::Caps::new_empty();
            for s in caps.iter() {
                if !matches!(s.name().as_str(), "video/x-av1" | "video/x-vp9") {
                    kept.get_mut().unwrap().append_structure(s.to_owned());
                }
            }
            kept
        }),
        SegmentFormat::Fmp4 => FMP4.get_or_init(|| sink_caps("isofmp4mux")),
    }
}

/// The chosen container's muxable caps-structure names.
pub(crate) fn muxable_names(format: SegmentFormat) -> std::collections::HashSet<String> {
    muxable_caps(format)
        .iter()
        .map(|s| s.name().to_string())
        .collect()
}

/// Can this container carry a stream of these caps?
///
/// Compares the structure NAME and `mpegversion`, and deliberately
/// nothing else: every other template field (`stream-format`,
/// `alignment`, `parsed`, `framed`, …) is one the parser inserted in
/// [`route_stream`] converts, so demanding it here would drop streams
/// that mux perfectly well — h264 arrives byte-stream/nal from AVI and
/// leaves h264parse as avc/au. `mpegversion` is the one field no parser
/// can change, and the one that tells mp3 apart from AAC.
pub(crate) fn caps_muxable(caps: &gst::Caps, format: SegmentFormat) -> bool {
    let _ = crate::init();
    let Some(s) = caps.structure(0) else {
        return false;
    };
    let mut probe = s.to_owned();
    let extra: Vec<String> = probe
        .fields()
        .filter(|f| f.as_str() != "mpegversion")
        .map(|f| f.to_string())
        .collect();
    for f in extra {
        probe.remove_field(&f);
    }
    gst::Caps::from(probe).can_intersect(muxable_caps(format))
}

/// Which muxer pad kind a parsed stream belongs on, if the session's
/// segment container can carry it.
fn sink_compatible(caps: &gst::Caps, format: SegmentFormat) -> Option<&'static str> {
    if !caps_muxable(caps, format) {
        return None;
    }
    let name = caps.structure(0)?.name();
    if name.starts_with("video/") {
        Some("video")
    } else if name.starts_with("audio/") {
        Some("audio")
    } else {
        None
    }
}

/// Normalized codec name (from discovery) → caps structure name.
/// Codecs discovery couldn't normalize pass through as raw caps names
/// (`video/x-divx`, `video/x-msmpeg`, …) — usable directly for decoder
/// lookups, so old exotics still plan as Encode instead of dropping.
pub(crate) fn codec_to_caps_name<'a>(kind: &str, codec: &'a str) -> Option<&'a str> {
    if codec.contains('/') {
        return Some(codec);
    }
    Some(match (kind, codec) {
        ("video", "h264") => "video/x-h264",
        ("video", "hevc") => "video/x-h265",
        ("video", "av1") => "video/x-av1",
        ("video", "vp9") => "video/x-vp9",
        // All three share one caps NAME; the version is a field, so a
        // caps-level check cannot tell them apart. The precision lives
        // in the codec label a client matches against.
        ("video", "mpeg" | "mpeg1" | "mpeg2" | "mpeg4part2") => "video/mpeg",
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

/// A codec LABEL as caps, for asking [`caps_muxable`] whether a
/// container could carry the stream before a pipeline exists.
///
/// Carries `mpegversion` for the `audio/mpeg` family, where the label
/// is the only thing separating mp3 and MPEG-1 layer 1/2 audio (both
/// version 1, TS-only) from AAC (version 2/4, muxable everywhere).
/// Nothing else is constrained: see [`caps_muxable`] for why fields a
/// parser can fix up must not appear here.
pub(crate) fn codec_to_caps(kind: &str, codec: &str) -> Option<gst::Caps> {
    // Building caps asserts an initialized GStreamer, and negotiation
    // reaches here before anything else touches it.
    let _ = crate::init();
    let name = codec_to_caps_name(kind, codec)?;
    let b = gst::Caps::builder(name);
    Some(match (kind, codec) {
        ("audio", "mp3" | "mpeg-audio") => b.field("mpegversion", 1i32).build(),
        ("audio", "aac") => b
            .field("mpegversion", gst::List::new([2i32, 4i32]))
            .build(),
        _ => b.build(),
    })
}

/// AAC encoders in preference order (fdk has the best quality). Used via
/// [`aac_encoder`], which also dry-run-verifies the winner (TC-1: a broken
/// element is discovered at startup, not mid-session).
pub const AAC_ENCODERS: &[&str] = &["fdkaacenc", "avenc_aac", "voaacenc"];

/// First encoder in `list` that exists AND survives its dry run —
/// shared by every per-codec discovery fn below. The dry run is what
/// makes preference lists safe: a hw element on a box without the
/// driver fails the probe and the next one wins (TC-1/TC-6).
fn verified_encoder(list: &[&'static str], dry: fn(&str) -> bool) -> Option<&'static str> {
    let _ = crate::init();
    list.iter().copied().find(|name| {
        if gst::ElementFactory::find(name).is_none() {
            return false;
        }
        let ok = dry(name);
        if !ok {
            tracing::warn!(encoder = name, "encoder failed dry-run; trying next");
        }
        ok
    })
}

/// Best available AAC encoder, verified once by a dry-run pipeline
/// (`audiotestsrc ! ... ! encoder ! fakesink` to EOS). None → no audio
/// transcoding on this machine.
pub fn aac_encoder() -> Option<&'static str> {
    static VERIFIED: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *VERIFIED.get_or_init(|| verified_encoder(AAC_ENCODERS, dry_run_encoder))
}

/// Opus encoder (HUB-15b audio target for non-aac clients). One
/// candidate: opusenc ships with gst-plugins-base and there is no
/// hardware Opus encoder in the wild.
pub const OPUS_ENCODERS: &[&str] = &["opusenc"];

pub fn opus_encoder() -> Option<&'static str> {
    static VERIFIED: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *VERIFIED.get_or_init(|| verified_encoder(OPUS_ENCODERS, dry_run_encoder))
}

/// H.264 encoders in preference order: hardware first (VA-API, NVENC,
/// QSV, VideoToolbox), then software.
pub const H264_ENCODERS: &[&str] = &[
    "vah264enc",
    "vaapih264enc",
    "nvh264enc",
    "qsvh264enc",
    "vtenc_h264_hw", // VideoToolbox (Apple Silicon)
    "vtenc_h264",
    "x264enc",
    "openh264enc",
];

/// HEVC and AV1 encode targets (HUB-15b), same hardware-first shape.
pub const HEVC_ENCODERS: &[&str] = &[
    "vah265enc",
    "vaapih265enc",
    "nvh265enc",
    "qsvh265enc",
    "vtenc_h265_hw",
    "vtenc_h265",
    "x265enc",
];
pub const AV1_ENCODERS: &[&str] = &[
    "vaav1enc",
    "nvav1enc",
    "qsvav1enc",
    "svtav1enc",
    "rav1enc",
    "av1enc",
];

/// Best available H.264 encoder, dry-run-verified once. None → this box
/// cannot transcode video.
pub fn h264_encoder() -> Option<&'static str> {
    static VERIFIED: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *VERIFIED.get_or_init(|| verified_encoder(H264_ENCODERS, dry_run_video_encoder))
}

pub fn hevc_encoder() -> Option<&'static str> {
    static VERIFIED: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *VERIFIED.get_or_init(|| verified_encoder(HEVC_ENCODERS, dry_run_video_encoder))
}

pub fn av1_encoder() -> Option<&'static str> {
    static VERIFIED: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *VERIFIED.get_or_init(|| verified_encoder(AV1_ENCODERS, dry_run_video_encoder))
}

/// The converter chain that feeds an encoder, by encoder family. The
/// benchmark (HUB-36) builds the SAME chain so its numbers describe the
/// pipeline that will actually run — a synthetic feed measured the
/// wrong thing entirely on VideoToolbox (0.80x synthetic vs 3.54x in a
/// real session, measured 2026-08-01).
///
/// videoconvert first for the nv family: exotic decoder outputs
/// (palettized RGB8P from msrle-era AVIs) never reach the CUDA
/// elements, which only take common formats; it is passthrough for
/// anything sane. Costs NVDEC→NVENC zero-copy (CUDA output can't cross
/// videoconvert, so hw decoders fall back to system memory) — measured
/// acceptable.
pub(crate) fn encode_converter_names(enc_name: &str) -> Vec<&'static str> {
    if enc_name.starts_with("nv") {
        vec!["videoconvert", "cudaupload", "cudaconvert"]
    } else {
        vec!["cudadownload", "videoconvert"]
    }
    .into_iter()
    .filter(|n| gst::ElementFactory::find(n).is_some())
    .collect()
}

/// Verified encoder capabilities for the transcoder's registration
/// report (TC-1): (codec, element, hardware) triples that survived a
/// dry run. Hardware = anything before the software entries in the
/// preference lists (placement prefers hw boxes). A `hevc:`/`av1:`/
/// `opus:` entry appearing here is what makes the box eligible for
/// that encode target (HUB-15b) — placement hard-filters on it.
pub fn encoder_capabilities() -> Vec<(&'static str, &'static str, bool)> {
    const SW_VIDEO: &[&str] = &[
        "x264enc",
        "openh264enc",
        "x265enc",
        "svtav1enc",
        "rav1enc",
        "av1enc",
    ];
    let mut caps = Vec::new();
    for (codec, el) in [
        ("h264", h264_encoder()),
        ("hevc", hevc_encoder()),
        ("av1", av1_encoder()),
    ] {
        if let Some(el) = el {
            caps.push((codec, el, !SW_VIDEO.contains(&el)));
        }
    }
    for (codec, el) in [("aac", aac_encoder()), ("opus", opus_encoder())] {
        if let Some(el) = el {
            caps.push((codec, el, false));
        }
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
            .flat_map(|t| {
                t.caps()
                    .iter()
                    .map(|s| s.name().to_string())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    })
    .collect();
    names.sort();
    names.dedup();
    names
}

/// Can any installed decoder take this stream? Derived from the element
/// registry (never hand-list what it can tell us).
pub(crate) fn can_decode(caps_name: &str) -> bool {
    // Anything that builds caps or reads the registry needs GStreamer
    // up. This used to ride on a caller having touched the muxable-caps
    // cache first, which is not a contract — it is an ordering accident,
    // and it panicked the moment negotiation short-circuited past it.
    let _ = crate::init();
    let caps = gst::Caps::new_empty_simple(caps_name);
    gst::ElementFactory::factories_with_type(gst::ElementFactoryType::DECODER, gst::Rank::MARGINAL)
        .iter()
        .any(|f| f.can_sink_any_caps(&caps))
}

/// What happens to one stream kind in a session (HUB-16 decision order:
/// copy what the client and muxer both take, encode what they don't but
/// a decoder can read, drop the rest).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StreamMode {
    Copy,
    /// Decode → re-encode to the target codec (h264 video / AAC audio).
    Encode,
    #[default]
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
pub const WEB_TARGET: Target = Target {
    video: &["h264"],
    audio: &["aac", "mp3"],
};

/// HUB-15b encode targets and the segment container that carries them.
/// Small Copy enums because RemuxPlan crosses the worker CLI and the
/// protobuf wire; unknown strings parse to the legacy default so an
/// old sender never breaks a new binary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VideoTarget {
    #[default]
    H264,
    Hevc,
    Av1,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AudioTarget {
    #[default]
    Aac,
    Opus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SegmentFormat {
    /// MPEG-TS segments via hlssink3/hlssink2 — the proven path; every
    /// h264/aac session rides it unchanged.
    #[default]
    Ts,
    /// Fragmented-MP4 segments via isofmp4mux + the fmp4sink writer —
    /// the only container that carries hevc/av1/opus to browsers.
    Fmp4,
}

#[allow(clippy::should_implement_trait)] // infallible, legacy-defaulting — not the trait
impl VideoTarget {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "hevc" => Self::Hevc,
            "av1" => Self::Av1,
            _ => Self::H264,
        }
    }
}

#[allow(clippy::should_implement_trait)] // infallible, legacy-defaulting — not the trait
impl AudioTarget {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Aac => "aac",
            Self::Opus => "opus",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "opus" => Self::Opus,
            _ => Self::Aac,
        }
    }
}

#[allow(clippy::should_implement_trait)] // infallible, legacy-defaulting — not the trait
impl SegmentFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ts => "ts",
            Self::Fmp4 => "fmp4",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "fmp4" => Self::Fmp4,
            _ => Self::Ts,
        }
    }
}

/// Per-kind session plan — the single source of truth shared between
/// session planning and pipeline routing, so the muxer pads requested up
/// front always match the streams that will actually be linked.
#[derive(Debug, Clone, Copy, Default)]
pub struct RemuxPlan {
    pub video: StreamMode,
    pub audio: StreamMode,
    /// Which audio stream to carry, indexed over the file's audio
    /// streams in discovery/demux order (HUB-27 track selection).
    pub audio_track: usize,
    /// Same for video (dual-video muxes: clean + hardsubbed fansub
    /// releases, sample tracks next to the feature).
    pub video_track: usize,
    /// Encode-branch parameters (HUB-14/15). None = the historical
    /// fixed values: 6000 kbit video, no scaling, no downmix.
    pub video_kbps: Option<u32>,
    pub max_height: Option<u32>,
    /// HUB-15a: run the GL PQ→SDR tone-map segment in the video encode
    /// chain. Only set when the executing box reported the capability.
    pub tone_map: bool,
    /// HUB-32b last resort: burn this image subtitle track (the `e{n}`
    /// index) into the picture, for clients that cannot composite one
    /// themselves. Forces the video encode that carries it.
    pub burn_subtitle: Option<usize>,
    /// HUB-32a: burn this EMBEDDED text subtitle track (the same `e{n}`
    /// index space as `burn_subtitle`) into the picture with libass, for
    /// clients that cannot render ASS themselves and whose user prefers
    /// faithful typesetting over a flattened VTT. The demuxer's own
    /// `application/x-ass` pad is used because it is the only source
    /// that carries the file's attached fonts. Forces the video encode.
    /// A user's SIDECAR .ass burns from a file instead (`burn_ass_file`
    /// on `start_parts`) and has no fonts to attach.
    pub burn_ass: Option<usize>,
    pub max_channels: Option<u32>,
    /// HUB-15b: what the encode arms produce and which segment
    /// container carries the session. Defaults = the historical
    /// h264/aac/TS behavior.
    pub video_codec: VideoTarget,
    pub audio_codec: AudioTarget,
    pub segment_format: SegmentFormat,
}

/// Fallback declaration when nothing better is known: the historical
/// value, which is honest only for an encode (whose GOP we set) or a
/// short-GOP source.
pub const DEFAULT_TARGET_DURATION_SECS: u32 = 2;

/// The fragment length the sink asks for. Segments are whole GOPs
/// packed until this is passed, so the longest segment is bounded by
/// `FRAGMENT_TARGET + max keyframe gap` — the formula the declaration
/// is computed from, verified against real files (GOP 10.43 s produced
/// segments of 10.22-10.58 s).
pub const FRAGMENT_TARGET_SECS: u32 = 2;

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

pub fn plan_streams(
    info: &kahawai_core::media::MediaInfo,
    target: &Target,
    audio_track: usize,
    video_track: usize,
) -> RemuxPlan {
    // Clamp stale indexes (rescan shrank the track list) to the last track.
    let audio_track = audio_track.min(info.audio.len().saturating_sub(1));
    let video_track = video_track.min(info.video.len().saturating_sub(1));
    let names = muxable_names(SegmentFormat::Ts);
    let copyable = |kind: &str, codec: &str, accepted: &[&str]| {
        accepted.contains(&codec)
            && codec_to_caps_name(kind, codec).is_some_and(|n| names.contains(n))
    };
    let selected_v = info.video.get(video_track);
    let video = if selected_v.is_some_and(|v| copyable("video", &v.codec, target.video)) {
        StreamMode::Copy
    } else if h264_encoder().is_some()
        && selected_v.is_some_and(|v| codec_to_caps_name("video", &v.codec).is_some_and(can_decode))
    {
        StreamMode::Encode
    } else {
        StreamMode::Off
    };
    // The plan judges the SELECTED track, not "any track": switching
    // from an AAC track to a DTS one flips copy → encode.
    let selected = info.audio.get(audio_track);
    let audio = if selected.is_some_and(|a| copyable("audio", &a.codec, target.audio)) {
        StreamMode::Copy
    } else if aac_encoder().is_some()
        && selected.is_some_and(|a| codec_to_caps_name("audio", &a.codec).is_some_and(can_decode))
    {
        StreamMode::Encode
    } else {
        StreamMode::Off
    };
    RemuxPlan {
        video,
        audio,
        audio_track,
        video_track,
        ..Default::default()
    }
}

/// Human-readable per-kind verdict for the playback-info overlay
/// (§4.3b spirit: the player reports which path was taken and why —
/// nothing converts silently).
pub fn plan_summary(info: &kahawai_core::media::MediaInfo, plan: &RemuxPlan) -> (String, String) {
    let names = muxable_names(SegmentFormat::Ts);
    let kind_summary =
        |kind: &str, codecs: Vec<&str>, mode: StreamMode, target_codec: &str| match mode {
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
        };
    (
        kind_summary(
            "video",
            info.video
                .get(plan.video_track)
                .map(|v| v.codec.as_str())
                .into_iter()
                .collect(),
            plan.video,
            plan.video_codec.as_str(),
        ),
        kind_summary(
            "audio",
            info.audio
                .get(plan.audio_track)
                .map(|a| a.codec.as_str())
                .into_iter()
                .collect(),
            plan.audio,
            plan.audio_codec.as_str(),
        ),
    )
}

/// Streams parsebin must be stopped from parsing, so the parser we
/// attach gets the demuxer's own buffers instead of parsebin's output.
///
/// AV1, and deliberately nothing else. parsebin fixes a pad's caps when
/// it exposes it and never renegotiates, so it settles AV1 on
/// `alignment=frame`; isofmp4mux demands `alignment=tu`, and the
/// frame→tu conversion a second av1parse performs drops every buffer
/// timestamp — 16,796 dropped by `guard_pts`, muxer starved, session
/// frozen. Fed raw OBUs, our av1parse produces `tu` directly.
///
/// This is NOT the general rule "one parser per stream", which was
/// tried and measurably costs more than it buys. Applied to h264 it
/// makes mpegtsmux emit one timestamp-less PES per keyframe — a bare
/// 9-byte PPS, which ffmpeg reports as an access unit with no picture
/// and the sweep flags as a missing DTS (6 of 60 files). The bytes
/// h264parse produces are byte-identical either way; what the second
/// parser contributes is re-segmenting them, and that is what keeps the
/// muxer's PES boundaries honest. Measured with plain gst-launch, no
/// kahawai involved: `qtdemux ! h264parse ! mpegtsmux` gives 106
/// timestamp-less packets, `qtdemux ! h264parse ! h264parse !
/// mpegtsmux` gives none.
fn parsebin_must_not_parse(caps: &gst::CapsRef) -> bool {
    caps.structure(0).is_some_and(|s| s.name() == "video/x-av1")
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
    gst::ElementFactory::find(element)
        .is_some()
        .then_some(element)
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
    gst::ElementFactory::find(element)
        .is_some()
        .then_some(element)
}

/// hlssink2 pads requested up front (splitmuxsink wants them before start);
/// each is taken by the first matching parsed stream.
/// Where each stream's branch terminates, once its real caps are known:
/// the muxer's pad for that stream, claimed once.
///
/// A MULTI-PART source has one of these PER PART, holding that part's
/// pre-claimed `concat` sink pad instead of the muxer's. concat plays its
/// sink pads in the order they were REQUESTED, so they are requested up
/// front in timeline order — claiming them lazily from `pad-added` races
/// across the parts' parsebins and can run CD2 first.
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
                        gate.triggered
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

    /// Open the gates. The seek's KEY_UNIT flag snapped to a keyframe at
    /// or before the requested start, so the true playlist origin is only
    /// knowable now: the first post-flush buffer on each feed reports its
    /// stream time into `start.pos` (players align subtitles/seekbar to
    /// it — TS PTS can't carry this, mpegtsmux rebases to a fixed epoch).
    fn open_reporting(&self, start_pos: std::path::PathBuf) {
        let min = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
        for (pad, id) in self.blocked.lock().unwrap().drain(..) {
            let min = min.clone();
            let path = start_pos.clone();
            pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                if let Some(gst::PadProbeData::Buffer(b)) = &info.data
                    && let Some(pts) = b.pts()
                    && let Some(seg) = pad
                        .sticky_event::<gst::event::Segment>(0)
                        .and_then(|e| e.segment().downcast_ref::<gst::ClockTime>().cloned())
                    && let Some(st) = seg.to_stream_time(pts)
                {
                    let ms = st.mseconds();
                    if ms < min.fetch_min(ms, std::sync::atomic::Ordering::SeqCst) {
                        let _ = std::fs::write(&path, ms.to_string());
                    }
                    return gst::PadProbeReturn::Remove;
                }
                gst::PadProbeReturn::Ok // unstamped buffer: keep waiting
            });
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
fn routable(caps: &gst::Caps, plan: &RemuxPlan) -> bool {
    let Some(caps_name) = caps.structure(0).map(|s| s.name()) else {
        return false;
    };
    match mode_for(caps_name, plan) {
        StreamMode::Copy => sink_compatible(caps, plan.segment_format).is_some(),
        StreamMode::Encode => can_decode(caps_name),
        StreamMode::Off => false,
    }
}

#[allow(clippy::too_many_arguments)] // internal fan-out point: one call site
fn plumb_parsed_pad(
    pipe: &gst::Pipeline,
    waiting: &WaitingPads,
    pad: &gst::Pad,
    plan: RemuxPlan,
    gate: &Option<Arc<SeekGate>>,
    audio_seen: &Arc<std::sync::atomic::AtomicUsize>,
    video_seen: &Arc<std::sync::atomic::AtomicUsize>,
    subs_seen: &Arc<std::sync::atomic::AtomicUsize>,
    subs_dir: &std::path::Path,
    burn: &Option<std::sync::Arc<crate::burnin::Timeline>>,
    // HUB-32a: Some when this session burns ASS — the chosen track's pad
    // goes to assrender instead of a tap file.
    ass_link: &Option<Arc<Mutex<AssBurnLink>>>,
    // False for the second and later parts of a multi-part source: the
    // tracks are the same ones continuing, so extracting them again would
    // overwrite the first part's files with a stream that starts at its
    // own zero.
    extract_subs: bool,
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
    let name = advertised
        .structure(0)
        .map(|s| s.name().to_string())
        .unwrap_or_default();
    // Track selection: only the plan's audio track proceeds (demux order
    // matches discovery order — the assumption subtitle extraction
    // already relies on). Streams whose advertised caps hide their
    // audio-ness take the deferred path uncounted; acceptable, they're
    // also unroutable-looking to the picker UI.
    // HUB-32 live tap: ASS events already flow through this pipeline
    // from the session origin — write them to a session file the hub
    // streams to ASS-rendering clients. No second read of the source.
    // Indexing counts every subtitle pad in demux order, matching the
    // discovery-order e{n} keys.
    if name.starts_with("application/x-subtitle")
        || name.starts_with("application/x-ssa")
        || name.starts_with("application/x-ass")
        || name.starts_with("text/")
        || name.starts_with("subpicture/")
    {
        if !extract_subs {
            let fake = gst::ElementFactory::make("fakesink").build().unwrap();
            pipe.add(&fake).unwrap();
            fake.sync_state_with_parent().unwrap();
            let _ = qsrc.link(&fake.static_pad("sink").unwrap());
            return;
        }
        let idx = subs_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // HUB-32a: the burned track feeds assrender, not a tap. No tee —
        // a burned track is in the picture, so nothing would fetch the
        // sidecar, and switching away from a burn restarts the session.
        if plan.burn_ass == Some(idx)
            && let Some(link) = ass_link
        {
            tracing::info!(track = idx, caps = %name, "ASS burn: routing track to assrender");
            AssBurnLink::offer_source(link, AssSource::Pad(qsrc));
            return;
        }
        if name.starts_with("subpicture/") {
            tap_image_track(pipe, &qsrc, &advertised, subs_dir, idx, &name);
        } else {
            tap_text_track(pipe, &qsrc, &advertised, subs_dir, idx, &name);
        }
        return;
    }
    let unselected = if name.starts_with("audio/") {
        let idx = audio_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (idx != plan.audio_track).then_some((idx, plan.audio_track, "audio"))
    } else if name.starts_with("video/") || name.starts_with("image/") {
        let idx = video_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (idx != plan.video_track).then_some((idx, plan.video_track, "video"))
    } else {
        None
    };
    {
        if let Some((idx, selected, kind)) = unselected {
            tracing::info!(caps = %name, idx, selected, kind, "remux: dropping unselected track");
            let fake = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .property("async", false)
                .build()
                .unwrap();
            pipe.add(&fake).unwrap();
            fake.sync_state_with_parent().unwrap();
            if let Err(e) = qsrc.link(&fake.static_pad("sink").unwrap()) {
                tracing::warn!(caps = %name, error = %e, "remux: fakesink link failed");
            }
            return;
        }
    }
    if routable(&advertised, &plan) {
        route_stream(
            pipe,
            waiting,
            &qsrc,
            &advertised,
            plan,
            gate,
            burn,
            ass_link,
            subs_dir,
        );
        return;
    }

    let pipe = pipe.clone();
    let waiting = waiting.clone();
    let gate = gate.clone();
    let burn = burn.clone();
    let ass_link = ass_link.clone();
    let facts_dir = subs_dir.to_path_buf();
    qsrc.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |qpad, info| {
        if let Some(gst::PadProbeData::Event(ev)) = &info.data
            && let gst::EventView::Caps(c) = ev.view()
            && qpad.peer().is_none()
        {
            route_stream(
                &pipe,
                &waiting,
                qpad,
                &c.caps_owned(),
                plan,
                &gate,
                &burn,
                &ass_link,
                &facts_dir,
            );
        }
        gst::PadProbeReturn::Ok
    });
}

/// Tee a text subtitle stream into the session dir as it is demuxed:
/// ASS/SSA → `subs-e{idx}.ass` (composed script header immediately —
/// codec_data carries it — then re-timed Dialogue lines); every other
/// text codec → `subs-e{idx}.jsonl` (one `{"s","e","t"}` cue per
/// line). Sparse and text-sized — a flush per line is nothing.
fn tap_text_track(
    pipe: &gst::Pipeline,
    from: &gst::Pad,
    caps: &gst::Caps,
    dir: &std::path::Path,
    idx: usize,
    caps_name: &str,
) {
    let is_ass = caps_name.contains("ssa") || caps_name.contains("ass");
    let path = dir.join(format!(
        "subs-e{idx}.{}",
        if is_ass { "ass" } else { "jsonl" }
    ));
    let mut file = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "subtitle tap file failed");
            let fake = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .property("async", false)
                .build()
                .unwrap();
            pipe.add(&fake).unwrap();
            fake.sync_state_with_parent().unwrap();
            let _ = from.link(&fake.static_pad("sink").unwrap());
            return;
        }
    };
    use std::io::Write;
    if is_ass {
        let header = caps
            .structure(0)
            .and_then(|s| s.get::<gst::Buffer>("codec_data").ok())
            .and_then(|b| {
                b.map_readable()
                    .ok()
                    .map(|m| crate::subtitles::decode_text(m.as_slice()))
            })
            .unwrap_or_default();
        let _ = file.write_all(crate::subtitles::compose_header(&header).as_bytes());
    }
    let file = std::sync::Mutex::new(file);

    let appsink = gstreamer_app::AppSink::builder().sync(false).build();
    appsink.set_property("async", false);
    appsink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                if let Ok(sample) = sink.pull_sample()
                    && let Some(buffer) = sample.buffer()
                    && let Some(pts) = buffer.pts()
                    && let Ok(map) = buffer.map_readable()
                {
                    let start = pts.mseconds();
                    let end = start + buffer.duration().map(|d| d.mseconds()).unwrap_or(3000);
                    let raw = crate::subtitles::decode_text(map.as_slice());
                    if is_ass {
                        if let Some(line) = crate::subtitles::ass_dialogue(&raw, start, end) {
                            let mut f = file.lock().unwrap();
                            let _ = writeln!(f, "{line}");
                        }
                    } else {
                        let text = crate::subtitles::clean_cue_text(&raw);
                        if !text.is_empty() {
                            let line = serde_json::json!({"s": start, "e": end, "t": text});
                            let mut f = file.lock().unwrap();
                            let _ = writeln!(f, "{line}");
                        }
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    pipe.add(appsink.upcast_ref::<gst::Element>()).unwrap();
    appsink.sync_state_with_parent().unwrap();
    if let Err(e) = from.link(&appsink.static_pad("sink").unwrap()) {
        tracing::warn!(error = %e, "subtitle tap link failed");
    }
    tracing::info!(path = %path.display(), "tapping text subtitle track");
}

/// Tee an image subtitle stream (PGS / VobSub) into
/// `subs-e{idx}.jsonl`: one display-set line per event —
/// `{"s":ms,"cw":..,"ch":..,"o":[{"x","y","png":base64}…]}` — decoded
/// to RGBA server-side so any client can draw them on an overlay
/// canvas. Empty "o" clears the screen.
fn tap_image_track(
    pipe: &gst::Pipeline,
    from: &gst::Pad,
    caps: &gst::Caps,
    dir: &std::path::Path,
    idx: usize,
    caps_name: &str,
) {
    use base64::Engine;
    let path = dir.join(format!("subs-e{idx}.jsonl"));
    let file = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "image sub tap file failed");
            let fake = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .property("async", false)
                .build()
                .unwrap();
            pipe.add(&fake).unwrap();
            fake.sync_state_with_parent().unwrap();
            let _ = from.link(&fake.static_pad("sink").unwrap());
            return;
        }
    };
    let file = std::sync::Mutex::new(file);
    let is_pgs = caps_name.contains("pgs");
    let mut pgs = crate::imagesubs::PgsDecoder::default();
    // VobSub: 16-color palette + display size ride the codec_data (.idx text).
    let (vob_palette, vob_size) = caps
        .structure(0)
        .and_then(|s| s.get::<gst::Buffer>("codec_data").ok())
        .and_then(|b| {
            b.map_readable().ok().map(|m| {
                let text = crate::subtitles::decode_text(m.as_slice());
                let size = crate::imagesubs::vobsub_size(&text);
                (crate::imagesubs::vobsub_palette(&text), size)
            })
        })
        .unwrap_or_default();
    let vob_size = vob_size.unwrap_or((720, 576));

    let write_set = move |file: &std::sync::Mutex<std::fs::File>,
                          ms: u64,
                          cw: u32,
                          ch: u32,
                          objects: &[crate::imagesubs::ImageObject]| {
        use std::io::Write;
        let objs: Vec<serde_json::Value> = objects
            .iter()
            .filter_map(|o| {
                let png = crate::imagesubs::to_png(o).ok()?;
                Some(serde_json::json!({
                    "x": o.x, "y": o.y,
                    "png": base64::engine::general_purpose::STANDARD.encode(png),
                }))
            })
            .collect();
        let line = serde_json::json!({"s": ms, "cw": cw, "ch": ch, "o": objs});
        let mut f = file.lock().unwrap();
        let _ = writeln!(f, "{line}");
    };

    let appsink = gstreamer_app::AppSink::builder().sync(false).build();
    appsink.set_property("async", false);
    appsink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                if let Ok(sample) = sink.pull_sample()
                    && let Some(buffer) = sample.buffer()
                    && let Some(pts) = buffer.pts()
                    && let Ok(map) = buffer.map_readable()
                {
                    let ms = pts.mseconds();
                    // Belt and braces around the decoders (issue #1):
                    // this callback is invoked by C, so a panic here
                    // cannot unwind and ABORTS the worker — killing the
                    // session, and killing the sink-fallback retry with
                    // it, since this runs upstream of the sink. The
                    // decoders are bounds-checked; this makes the NEXT
                    // parser bug a dropped subtitle instead of a dead
                    // session. Same reasoning as guard_pts.
                    let guard = |f: &mut dyn FnMut()| {
                        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
                            tracing::error!(
                                pts_ms = ms,
                                "image subtitle decoder panicked; dropping this packet"
                            );
                        }
                    };
                    if is_pgs {
                        let mut fed = None;
                        guard(&mut || fed = pgs.feed(map.as_slice()).ok().flatten());
                        if let Some(set) = fed {
                            write_set(&file, ms, set.canvas_w, set.canvas_h, &set.objects);
                        }
                    } else if let Some(obj) = {
                        let mut decoded = None;
                        guard(&mut || {
                            decoded = crate::imagesubs::vobsub_decode(map.as_slice(), &vob_palette)
                                .ok()
                                .flatten()
                        });
                        decoded
                    } {
                        let end = ms + buffer.duration().map(|d| d.mseconds()).unwrap_or(5000);
                        write_set(
                            &file,
                            ms,
                            vob_size.0,
                            vob_size.1,
                            std::slice::from_ref(&obj),
                        );
                        write_set(&file, end, vob_size.0, vob_size.1, &[]);
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    pipe.add(appsink.upcast_ref::<gst::Element>()).unwrap();
    appsink.sync_state_with_parent().unwrap();
    if let Err(e) = from.link(&appsink.static_pad("sink").unwrap()) {
        tracing::warn!(error = %e, "image sub tap link failed");
    }
    tracing::info!(path = %path.display(), pgs = is_pgs, "tapping image subtitle track");
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

#[allow(clippy::too_many_arguments)] // internal fan-out point: one call site
fn route_stream(
    pipe: &gst::Pipeline,
    waiting: &WaitingPads,
    from: &gst::Pad,
    caps: &gst::Caps,
    plan: RemuxPlan,
    gate: &Option<Arc<SeekGate>>,
    burn: &Option<std::sync::Arc<crate::burnin::Timeline>>,
    ass_link: &Option<Arc<Mutex<AssBurnLink>>>,
    facts_dir: &std::path::Path,
) {
    let caps_name = caps
        .structure(0)
        .map(|s| s.name().to_string())
        .unwrap_or_default();
    let mode = mode_for(&caps_name, &plan);
    // Encode: claim the kind's muxer pad for the decode→re-encode branch.
    if mode == StreamMode::Encode && can_decode(&caps_name) {
        let kind = if caps_name.starts_with("video/") {
            "video"
        } else {
            "audio"
        };
        if let Some(sinkpad) = waiting.lock().unwrap().remove(kind) {
            tracing::info!(caps = %caps_name, kind, "transcoding stream");
            if kind == "video" {
                build_video_encode_chain(
                    pipe,
                    from,
                    sinkpad,
                    gate,
                    plan.video_codec,
                    plan.video_kbps,
                    plan.max_height,
                    plan.tone_map,
                    burn.clone(),
                    ass_link.clone(),
                );
            } else {
                build_audio_encode_chain(
                    pipe,
                    from,
                    sinkpad,
                    &caps_name,
                    gate,
                    plan.audio_codec,
                    plan.max_channels,
                    facts_dir,
                );
            }
            return;
        }
    }
    let target = (mode == StreamMode::Copy)
        .then(|| {
            sink_compatible(caps, plan.segment_format)
                .and_then(|kind| waiting.lock().unwrap().remove(kind))
        })
        .flatten();
    match target {
        Some(sinkpad) => {
            let mut tail = from.clone();
            // parser → timestamper, each present only when it applies;
            // every hop is pure repackaging, no decode.
            for name in [parser_for(caps), timestamper_for(caps)]
                .into_iter()
                .flatten()
            {
                let el = gst::ElementFactory::make(name).build().unwrap();
                // HLS requires independently decodable segments: h26x
                // parameter sets must ride every keyframe, or only the
                // first segment can start a decoder (players stall on
                // transitions and cold seeks).
                if name.ends_with("parse") {
                    set_prop_if_present(&el, "config-interval", -1i32);
                }
                pipe.add(&el).unwrap();
                el.sync_state_with_parent().unwrap();
                tail.link(&el.static_pad("sink").unwrap()).unwrap();
                tail = el.static_pad("src").unwrap();
            }
            guard_pts(&tail);
            // GATE UPSTREAM OF THE PARSER CHAIN, not at the muxer feed.
            //
            // The gate exists so the muxer is still virgin when the
            // offset seek flushes (splitmuxsink aborts on a mid-GOP
            // flush). Blocking at `from` keeps that property — nothing
            // gets past — and adds the one this path also needs: the
            // parser and `h264timestamper` never see pre-seek data
            // either, so the flush finds them with no state to corrupt.
            //
            // With the gate at the muxer feed instead, the timestamper
            // HAS built state by the time the seek flushes it, and
            // afterwards emits duplicate DTS (measured
            // `1.920, 1.960, 1.960, 2.000, 2.000` on a 25 fps source).
            // mpegtsmux rebases everything onto its own epoch and never
            // noticed; `isofmp4mux` PANICKED — "Timestamps going
            // backwards" — killing the worker, so the session produced
            // no segments and the player buffered forever. Reported
            // 2026-08-03 against an h264+FLAC episode, where every seek
            // and every resume rolled the dice.
            //
            // Blocking earlier is strictly stronger for the original
            // reason too: less reaches the sink, not more.
            if let Some(g) = gate {
                g.install(from);
            }
            if let Err(e) = tail.link(&sinkpad) {
                tracing::warn!(caps = %caps_name, error = %e, "remux: pad link failed");
            }
        }
        None => {
            tracing::info!(
                caps = %caps_name,
                container = plan.segment_format.as_str(),
                "remux: dropping stream (container cannot carry it, or duplicate)"
            );
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
/// HUB-15a: the PQ→SDR fragment shader (see tonemap.frag for the why).
const TONEMAP_FRAG: &str = include_str!("tonemap.frag");

/// HUB-15a: the GL tone-map segment, dry-run-verified once (TC-1
/// standard, same as the encoders): element presence is not enough — a
/// headless box can carry every GL plugin and still fail to open a GL
/// display, and that must surface here, not mid-session.
pub fn tonemap_available() -> bool {
    // Any target this box can actually encode into. Reporting the
    // capability without naming a target is what let a box claim it
    // could tone-map while every session died (see `tonemap_into`).
    [h264_encoder(), hevc_encoder(), av1_encoder()]
        .into_iter()
        .flatten()
        .any(tonemap_into)
}

/// Can this box burn ASS/SSA subtitles into the picture, feeding
/// `encoder`? HUB-32a's burn arm needs `assrender` (libass), which is
/// NOT universal — it is missing on the macOS satellite of this fleet
/// while present on the Linux ones, so placement has to filter on it
/// rather than assume it.
///
/// AR-13a: the probe ends in the real encoder. `assrender` sits where
/// the image blender does, after the tone map, and takes 8-bit system
/// memory, so an encoder that cannot follow it there is a box that
/// cannot burn — regardless of the element being installed.
///
/// `text_sink` is deliberately left unconnected: this asks whether the
/// VIDEO path holds together, which is what placement needs. Whether a
/// particular script parses is a per-session question with a
/// per-session answer.
pub fn ass_burn_into(encoder: &str) -> bool {
    static VERIFIED: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, bool>>,
    > = std::sync::OnceLock::new();
    let cache = VERIFIED.get_or_init(Default::default);
    if let Some(known) = cache.lock().unwrap().get(encoder) {
        return *known;
    }
    let ok = probe_ass_burn_into(encoder);
    if !ok {
        tracing::warn!(
            encoder,
            "ASS burn-in dry-run failed — tier unavailable for this target"
        );
    }
    cache.lock().unwrap().insert(encoder.to_string(), ok);
    ok
}

/// Any encode target this box can burn ASS into.
pub fn ass_burn_available() -> bool {
    [h264_encoder(), hevc_encoder(), av1_encoder()]
        .into_iter()
        .flatten()
        .any(ass_burn_into)
}

fn probe_ass_burn_into(encoder: &str) -> bool {
    if crate::init().is_err() || gst::ElementFactory::find("assrender").is_none() {
        return false;
    }
    let pipe = gst::Pipeline::new();
    let (Some(src), Some(pin), Some(render), Some(convert), Some(enc), Some(sink)) = (
        gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 5i32)
            .build()
            .ok(),
        gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("format", "I420")
                    .field("width", 320i32)
                    .field("height", 240i32)
                    .field("framerate", gst::Fraction::new(24, 1))
                    .build(),
            )
            .build()
            .ok(),
        gst::ElementFactory::make("assrender").build().ok(),
        gst::ElementFactory::make("videoconvert").build().ok(),
        gst::ElementFactory::make(encoder).build().ok(),
        gst::ElementFactory::make("fakesink").build().ok(),
    ) else {
        return false;
    };
    if pipe
        .add_many([&src, &pin, &render, &convert, &enc, &sink])
        .is_err()
        || gst::Element::link_many([&src, &pin, &render, &convert, &enc, &sink]).is_err()
        || pipe.set_state(gst::State::Playing).is_err()
    {
        let _ = pipe.set_state(gst::State::Null);
        return false;
    }
    let ok = pipe
        .bus()
        .and_then(|bus| {
            bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
        })
        .is_some_and(|msg| msg.type_() == gst::MessageType::Eos);
    let _ = pipe.set_state(gst::State::Null);
    ok
}

/// Can this box tone-map INTO `encoder`?
///
/// AR-13a: a dry run has to reproduce the chain a session builds, or it
/// measures something nobody runs. The old probe ended in `fakesink`
/// and passed no encoder, so the segment's output pin fell back to the
/// full format list and the sink accepted whatever came out. On the
/// J5005 that reported `tonemap: true` at 1.30x while every HDR
/// session died at negotiation — `vah264enc` takes NV12 and not I420,
/// and the unpinned list resolved to I420. A day of HDR playback was
/// lost to a capability the box had measured and did not have.
///
/// So the probe ends in the real encoder, with the pin that encoder
/// forces, fed the 10-bit frames an HDR decode actually produces —
/// 8-bit input would skip the upload/download conversion that costs
/// the most and negotiates the narrowest.
pub fn tonemap_into(encoder: &str) -> bool {
    static VERIFIED: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, bool>>,
    > = std::sync::OnceLock::new();
    let cache = VERIFIED.get_or_init(Default::default);
    if let Some(known) = cache.lock().unwrap().get(encoder) {
        return *known;
    }
    let ok = probe_tonemap_into(encoder);
    if !ok {
        tracing::warn!(encoder, "GL tone-map into this encoder failed dry-run");
    }
    cache.lock().unwrap().insert(encoder.to_string(), ok);
    ok
}

fn probe_tonemap_into(encoder: &str) -> bool {
    if crate::init().is_err()
        || [
            "glupload",
            "glcolorconvert",
            "glshader",
            "gldownload",
            "capssetter",
        ]
        .iter()
        .any(|n| gst::ElementFactory::find(n).is_none())
    {
        return false;
    }
    let pipe = gst::Pipeline::new();
    let Ok(src) = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", 5i32)
        .build()
    else {
        return false;
    };
    // 10-bit in, like the HDR source this exists for.
    let Ok(pin) = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "I420_10LE")
                .field("width", 320i32)
                .field("height", 240i32)
                .build(),
        )
        .build()
    else {
        return false;
    };
    let (Some(convert), Some(enc), Some(sink)) = (
        gst::ElementFactory::make("videoconvert").build().ok(),
        gst::ElementFactory::make(encoder).build().ok(),
        gst::ElementFactory::make("fakesink").build().ok(),
    ) else {
        return false;
    };
    let seg = tonemap_segment(encoder);
    if pipe.add_many([&src, &pin, &convert]).is_err()
        || pipe.add_many(&seg).is_err()
        || pipe.add_many([&enc, &sink]).is_err()
    {
        return false;
    }
    let mut all: Vec<&gst::Element> = vec![&src, &pin, &convert];
    all.extend(seg.iter());
    all.push(&enc);
    all.push(&sink);
    if gst::Element::link_many(all).is_err() || pipe.set_state(gst::State::Playing).is_err() {
        let _ = pipe.set_state(gst::State::Null);
        return false;
    }
    let ok = pipe
        .bus()
        .and_then(|bus| {
            bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
        })
        .is_some_and(|msg| msg.type_() == gst::MessageType::Eos);
    let _ = pipe.set_state(gst::State::Null);
    ok
}

/// How long the burn-in index walk may take before the session gives
/// up on it and plays without subtitles (HUB-32b). Generous for local
/// disk, far short of the playlist deadline.
const BURN_INDEX_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// PQ encode (SMPTE ST 2084 inverse EOTF): linear (1.0 = 10000 nits)
/// → PQ code. Rust twin of the shader's pq_encode.
fn pq_encode(y: f64) -> f64 {
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = 2523.0 / 4096.0 * 128.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 4096.0 * 32.0;
    const C3: f64 = 2392.0 / 4096.0 * 32.0;
    let p = y.max(0.0).powf(M1);
    ((C1 + C2 * p) / (1.0 + C3 * p)).powf(M2)
}

fn pq_eotf(e: f64) -> f64 {
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = 2523.0 / 4096.0 * 128.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 4096.0 * 32.0;
    const C3: f64 = 2392.0 / 4096.0 * 32.0;
    let p = e.max(0.0).powf(1.0 / M2);
    ((p - C1).max(0.0) / (C2 - C3 * p)).powf(1.0 / M1)
}

/// PQ(203 nits) — the EETF's SDR target, fixed.
const TGT_E: f64 = 0.580688881;

/// Scene-peak probe (HUB-15a dynamic adaptation): sample the luma
/// plane of every buffer entering the GL segment, track a smoothed
/// p99.9 peak (instant attack, slow decay — libplacebo's shape), and
/// feed the shader's EETF uniforms. A static 1000-nit assumption
/// measured ~0.7 signal on real scene highlights where libplacebo
/// reaches ~0.98 (the owner's "grey smear"): typical frames peak at
/// 200–800 nits, far below mastering ceilings.
/// Sample every 16th row and column of a luma plane into `out` as
/// 10-bit codes (~32k samples at 4K: enough for p99.9, cheap enough
/// for every frame).
///
/// Pure and bounds-checked on purpose. It runs inside a pad probe
/// called from C, where a panic cannot unwind and ABORTS the worker,
/// and it is fed whatever buffer layout a decoder chose: `stride` is
/// the FRAME's, which for a padded buffer exceeds the one the caps
/// imply, and the final row may be short where the allocator packed
/// tightly. Neither may index past the plane.
pub(crate) fn sample_luma(
    data: &[u8],
    stride: usize,
    w: usize,
    h: usize,
    ten_bit: bool,
    out: &mut Vec<u16>,
) {
    let bpp = if ten_bit { 2 } else { 1 };
    if stride < w * bpp {
        return; // nonsense geometry: sample nothing rather than guess
    }
    let mut y = 0;
    while y < h {
        let Some(row) = data.get(y * stride..(y * stride + w * bpp).min(data.len())) else {
            break;
        };
        let mut x = 0;
        while x * bpp + bpp <= row.len() && x < w {
            out.push(if ten_bit {
                let lo = row[x * 2] as u16;
                let hi = row[x * 2 + 1] as u16;
                ((hi << 8) | lo) >> 6 // P010: 10 bits in the high bits
            } else {
                (row[x] as u16) << 2
            });
            x += 16;
        }
        y += 16;
    }
}

fn attach_peak_probe(upload: &gst::Element, shader: &gst::Element) {
    let pad = upload.static_pad("sink").unwrap();
    let shader = shader.clone();
    // (smoothed peak, last-set peak, reusable sample buffer)
    let state = std::sync::Mutex::new((1000.0f64, 1000.0f64, Vec::<u16>::new()));
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let Some(gst::PadProbeData::Buffer(buffer)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(caps) = pad.current_caps() else {
            return gst::PadProbeReturn::Ok;
        };
        let Ok(vinfo) = gst_video::VideoInfo::from_caps(&caps) else {
            return gst::PadProbeReturn::Ok;
        };
        use gst_video::VideoFormat;
        use gst_video::prelude::VideoFrameExt;
        let ten_bit = match vinfo.format() {
            VideoFormat::P01010le | VideoFormat::I42010le => true,
            VideoFormat::Nv12 | VideoFormat::I420 => false,
            _ => return gst::PadProbeReturn::Ok,
        };
        let Ok(frame) = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &vinfo) else {
            return gst::PadProbeReturn::Ok;
        };
        let Ok(data) = frame.plane_data(0) else {
            return gst::PadProbeReturn::Ok;
        };
        // Stride and size come from the FRAME, never from the caps.
        // A decoder pads its buffers to suit itself — likely whenever
        // the coded size exceeds the display size, e.g. 1920x1038,
        // whose coded height is 1040 — so the caps-derived stride can
        // be smaller than the real one, and `y * stride` then indexes
        // past the plane. This probe runs in a pad probe called from C,
        // where a panic cannot unwind and ABORTS the worker: reported
        // as a SIGABRT at session start on an HDR title, and it
        // vanished when the client forced HDR (no tone-map, no probe).
        let stride = frame.plane_stride()[0].max(0) as usize;
        let (w, h) = (frame.width() as usize, frame.height() as usize);
        let mut st = state.lock().unwrap();
        let (_, _, ref mut samples) = *st;
        samples.clear();
        sample_luma(data, stride, w, h, ten_bit, samples);
        if samples.is_empty() {
            return gst::PadProbeReturn::Ok;
        }
        samples.sort_unstable();
        let p999 = samples[samples.len() - 1 - samples.len() / 1000];
        let nits = (pq_eotf(p999 as f64 / 1023.0) * 10000.0).clamp(203.0, 4000.0);
        // Instant attack (a clipped flash is worse than a dim one),
        // ~2 s decay at 24 fps.
        st.0 = if nits > st.0 {
            nits
        } else {
            st.0 * 0.98 + nits * 0.02
        };
        if (st.0 - st.1).abs() / st.1 > 0.01 {
            st.1 = st.0;
            let max_e = pq_encode(st.0 / 10000.0);
            let max_tgt = TGT_E / max_e;
            let uniforms = gst::Structure::builder("uniforms")
                .field("uMaxE", max_e as f32)
                .field("uMaxTgt", max_tgt as f32)
                .field("uKS", (1.5 * max_tgt - 0.5) as f32)
                .build();
            shader.set_property("uniforms", &uniforms);
        }
        gst::PadProbeReturn::Ok
    });
}

/// What the tone-map segment may hand the encoder behind it. NV12
/// first: the hardware encoders take it directly, so they keep
/// negotiating it. I420 is what openh264enc — sink template I420 and
/// nothing else — needs to be linkable at all.
pub(crate) const TONEMAP_OUT_FORMATS: [&str; 2] = ["NV12", "I420"];

/// Hold encoded video back until the first KEYFRAME, asking the encoder
/// for one as soon as data actually flows.
///
/// Installed only on a seeked start. The first fragment a player gets
/// must stand alone — there are no earlier parameter sets to fall back
/// on — and an encoder resuming mid-stream does not reliably lead with
/// an IDR.
///
/// The request is sent from INSIDE the probe, on the first buffer we
/// drop, and that timing is the point: sent while building the pipeline
/// it reaches an encoder that is not playing yet and does nothing
/// (measured — the first attempt at this fix changed exactly nothing).
/// By the time a buffer arrives here the chain is running, so the event
/// lands. `hlssink3`'s own fragment-boundary requests prove the encoder
/// honours them; this just needs one earlier.
///
/// BUFFER **and** BUFFER_LIST: a parser is free to push lists, and a
/// BUFFER-only probe never sees those — the other half of why the first
/// attempt was a no-op.
fn open_on_keyframe(pad: &gst::Pad) {
    let asked = std::sync::atomic::AtomicBool::new(false);
    let dropped = std::sync::atomic::AtomicUsize::new(0);
    pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |pad, info| {
            let leads_with_keyframe = match &info.data {
                Some(gst::PadProbeData::Buffer(b)) => {
                    !b.flags().contains(gst::BufferFlags::DELTA_UNIT)
                }
                // A list is keyframe-led if its FIRST buffer is: that is
                // what the muxer will see at the head of the fragment.
                Some(gst::PadProbeData::BufferList(l)) => l
                    .get(0)
                    .is_some_and(|b| !b.flags().contains(gst::BufferFlags::DELTA_UNIT)),
                _ => return gst::PadProbeReturn::Ok,
            };
            if leads_with_keyframe {
                tracing::info!(
                    dropped = dropped.load(std::sync::atomic::Ordering::SeqCst),
                    "seeked start: keyframe reached, video opened"
                );
                return gst::PadProbeReturn::Remove;
            }
            if !asked.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let sent = pad.send_event(
                    gst_video::UpstreamForceKeyUnitEvent::builder()
                        .all_headers(true)
                        .build(),
                );
                tracing::info!(sent, "seeked start: asked the encoder for a keyframe");
            }
            dropped.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            gst::PadProbeReturn::Drop
        },
    );
}

/// The GL tone-map segment: upload → RGBA → PQ→SDR shader → back to
/// system memory, then capssetter rewrites the colorimetry tag to
/// bt709 so the encoder's VUI tells the player the truth (the shader
/// changed the pixels; nothing else knows to change the label).
/// The one output format to pin for `encoder`: the first of
/// [`TONEMAP_OUT_FORMATS`] its sink pad actually accepts.
///
/// A LIST is not a preference order. Offering `{NV12, I420}` to
/// `vah264enc` — which takes NV12 and not I420 — resolves to I420 and
/// the pipeline dies with not-negotiated. Measured on the J5005:
///
/// ```text
///   {NV12,I420}   not-negotiated
///   NV12          OK
///   I420          FAILED
/// ```
///
/// So the pin has to name the format the DOWNSTREAM ENCODER takes,
/// which means knowing which encoder that is. Falls back to the whole
/// list when the element cannot be probed — no worse than before, and
/// the only case where a list is honest.
pub(crate) fn tonemap_out_caps(encoder: &str) -> gst::Caps {
    let accepted: Vec<&str> = gst::ElementFactory::find(encoder)
        .map(|f| {
            let sinks: Vec<gst::Caps> = f
                .static_pad_templates()
                .into_iter()
                .filter(|t| t.direction() == gst::PadDirection::Sink)
                .map(|t| t.caps())
                .collect();
            TONEMAP_OUT_FORMATS
                .iter()
                .copied()
                .filter(|fmt| {
                    // Feature-agnostic: hardware templates publish under
                    // memory:VAMemory/GLMemory/CUDAMemory, and it is the
                    // FORMAT agreement being tested, not the memory space.
                    let mut want = gst::Caps::builder("video/x-raw")
                        .field("format", *fmt)
                        .build();
                    want.get_mut()
                        .unwrap()
                        .set_features(0, Some(gst::CapsFeatures::new_any()));
                    sinks.iter().any(|c| !c.intersect(&want).is_empty())
                })
                .collect()
        })
        .unwrap_or_default();
    let formats = if accepted.is_empty() {
        TONEMAP_OUT_FORMATS.to_vec()
    } else {
        accepted
    };
    gst::Caps::builder("video/x-raw")
        .field("format", gst::List::new(formats))
        .build()
}

pub(crate) fn tonemap_segment(encoder: &str) -> Vec<gst::Element> {
    let upload = gst::ElementFactory::make("glupload").build().unwrap();
    let to_rgba = gst::ElementFactory::make("glcolorconvert").build().unwrap();
    let rgba = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .features(["memory:GLMemory"])
                .field("format", "RGBA")
                .build(),
        )
        .build()
        .unwrap();
    let shader = gst::ElementFactory::make("glshader")
        .property("fragment", TONEMAP_FRAG)
        .build()
        .unwrap();
    let from_rgba = gst::ElementFactory::make("glcolorconvert").build().unwrap();
    let download = gst::ElementFactory::make("gldownload").build().unwrap();
    // Pinned HERE, GPU-side: without it glcolorconvert stays RGBA and a
    // VA encoder with no converter between (the non-CUDA path has none
    // after this segment) refuses system-memory RGBA — observed as
    // not-negotiated on the J5005.
    //
    // A LIST, not NV12 alone. "Every encoder we place takes NV12" was
    // written for the hardware ones and is false for openh264enc, whose
    // sink template is I420 and nothing else: pinning NV12 left the
    // segment's trailing capssetter unlinkable to it, and because the
    // chain is linked from a pad-added callback the `unwrap` on that
    // link became a non-unwinding panic — SIGABRT, no session error, on
    // every HDR title a software-encoder box was asked to tone-map
    // (field report 2026-08-01; the user's workaround was forcing HDR on
    // in the browser, which skips this segment entirely). NV12 stays
    // first so hardware still negotiates it; I420 is what makes the
    // software path exist at all.
    let nv12 = gst::ElementFactory::make("capsfilter")
        .property("caps", tonemap_out_caps(encoder))
        .build()
        .unwrap();
    let relabel = gst::ElementFactory::make("capssetter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("colorimetry", "bt709")
                .build(),
        )
        .build()
        .unwrap();
    attach_peak_probe(&upload, &shader);
    vec![
        upload, to_rgba, rgba, shader, from_rgba, download, nv12, relabel,
    ]
}

/// How long the picture waits for its subtitles before giving up and
/// encoding without them. Generous: the pads are usually milliseconds
/// apart, and the only thing on the other side of this timeout is a
/// session that never starts.
const ASS_GATE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// HUB-32a: the two halves of an ASS burn meet here.
///
/// `assrender` needs the picture on `video_sink` and the styled events
/// on `text_sink`, and the two are decided in different callbacks with
/// no ordering guarantee: the encode chain (which owns the renderer) is
/// built from `pad-added`, and so is the demuxer's subtitle pad. Same
/// shape as `WaitingPads`, which already rendezvouses muxer pads across
/// the same callbacks — whichever half lands second does the linking.
#[derive(Default)]
pub(crate) struct AssBurnLink {
    /// `assrender`'s `text_sink` and the pipeline it lives in, once the
    /// encode chain exists.
    text: Option<(gst::Pipeline, gst::Pad)>,
    source: Option<AssSource>,
    /// A block probe holding the PICTURE until the subtitles are
    /// attached. Without it the encode simply outruns them: measured
    /// live on a dispatched session, the video chain was built 88 ms
    /// before the demuxer exposed its subtitle pad, and `assrender`
    /// logged "rendering disabled, doing buffer passthrough" for every
    /// frame in between — a clean picture with no subtitles in it,
    /// which nothing downstream can detect. `wait-text` does not cover
    /// this: it waits only once a text stream is LINKED.
    gate: Option<(gst::Pad, gst::PadProbeId)>,
}

/// Where the events come from. The two cases are genuinely different
/// mechanisms, not one with a flag (2026-08-02 experiments, recorded in
/// `docs/kahawai-implementation.md`).
pub(crate) enum AssSource {
    /// An EMBEDDED track: the demuxer's own `application/x-ass` pad,
    /// which is the only path that also carries the file's attached
    /// fonts.
    Pad(gst::Pad),
    /// A user's SIDECAR script — header (the container's `codec_data`
    /// equivalent) and one matroska-shaped payload per event. No fonts;
    /// it renders with the system's.
    File(String, Vec<(u64, u64, String)>),
}

impl AssBurnLink {
    fn couple(&mut self) {
        let (Some((pipe, sink)), Some(source)) = (&self.text, &self.source) else {
            return; // the other half has not arrived yet
        };
        if sink.is_linked() {
            return;
        }
        let result = match source {
            AssSource::Pad(src) => src.link(sink).map(|_| ()),
            AssSource::File(header, events) => play_ass_file(pipe, sink, header, events),
        };
        match result {
            Ok(()) => tracing::info!("ASS burn: subtitles linked to assrender"),
            // Loud: the encode carries on and produces a picture with no
            // subtitles in it, which is the failure this tier exists to
            // prevent and which nothing downstream can detect.
            Err(e) => tracing::error!(error = %e, "ASS burn: subtitle → assrender link failed"),
        }
        self.open_gate();
    }

    /// Let the picture through. Called once the subtitles are attached
    /// — or by the watchdog, which would rather ship a subtitle-less
    /// encode (loudly) than a session that never starts.
    fn open_gate(&mut self) {
        if let Some((pad, id)) = self.gate.take() {
            pad.remove_probe(id);
        }
    }

    /// Hold the video branch closed until [`couple`] succeeds. Installed
    /// on the encode chain's own sink pad, before decodebin is linked to
    /// it, so no frame can reach `assrender` un-subtitled.
    fn shut_gate(link: &Arc<Mutex<Self>>, pad: &gst::Pad) {
        let Some(id) = pad.add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_, _| {
            gst::PadProbeReturn::Ok
        }) else {
            return;
        };
        link.lock().unwrap().gate = Some((pad.clone(), id));
        // Never deadlock on a subtitle pad that does not come: a source
        // whose ASS track vanished between planning and demuxing would
        // otherwise wedge the session at zero segments forever.
        let link = link.clone();
        std::thread::spawn(move || {
            std::thread::sleep(ASS_GATE_WAIT);
            let mut l = link.lock().unwrap();
            if l.gate.is_some() {
                tracing::error!(
                    "ASS burn: no subtitle stream after {:?} — encoding WITHOUT subtitles",
                    ASS_GATE_WAIT
                );
                l.open_gate();
            }
        });
    }

    pub(crate) fn offer_text_sink(link: &Arc<Mutex<Self>>, pipe: &gst::Pipeline, pad: gst::Pad) {
        let mut l = link.lock().unwrap();
        l.text = Some((pipe.clone(), pad));
        l.couple();
    }

    pub(crate) fn offer_source(link: &Arc<Mutex<Self>>, source: AssSource) {
        let mut l = link.lock().unwrap();
        l.source = Some(source);
        l.couple();
    }
}

/// Play a sidecar script into `assrender` from an `appsrc`, because no
/// GStreamer element turns a subtitle FILE into the
/// `application/x-ass` stream a demuxer produces (`ssaparse` flattens
/// to `text/x-raw`, which is the other tier entirely).
///
/// Built and linked in one go, deliberately: an `appsrc` that reaches
/// PAUSED with its src pad unlinked pushes its caps into nothing and
/// pauses its own streaming task, and every later `push_buffer` then
/// queues into a task that will never run again. Measured — the events
/// arrived, `push_buffer` returned Ok for all of them, and assrender
/// logged "rendering disabled, doing buffer passthrough" for every
/// frame because its text caps had never been set.
fn play_ass_file(
    pipe: &gst::Pipeline,
    sink: &gst::Pad,
    header: &str,
    events: &[(u64, u64, String)],
) -> Result<(), gst::PadLinkError> {
    let src = gstreamer_app::AppSrc::builder()
        .caps(
            &gst::Caps::builder("application/x-ass")
                .field(
                    "codec_data",
                    gst::Buffer::from_slice(header.to_string().into_bytes()),
                )
                .build(),
        )
        .format(gst::Format::Time)
        .build();
    pipe.add(&src).unwrap();
    src.static_pad("src").unwrap().link(sink)?;
    src.sync_state_with_parent().unwrap();
    let events = events.to_vec();
    // A thread, not a `need-data` handler: the whole script is already
    // in memory (tens of kilobytes), so there is nothing to pull it
    // lazily for, and appsrc's queue applies the backpressure.
    std::thread::spawn(move || {
        for (start, end, line) in events {
            let mut buf = gst::Buffer::from_slice(line.into_bytes());
            {
                let b = buf.get_mut().unwrap();
                b.set_pts(gst::ClockTime::from_mseconds(start));
                b.set_duration(gst::ClockTime::from_mseconds(end.saturating_sub(start)));
            }
            if src.push_buffer(buf).is_err() {
                return; // shutting down; assrender keeps what it got
            }
        }
        let _ = src.end_of_stream();
    });
    Ok(())
}

/// Read a sidecar `.ass` into the form [`AssBurnLink`] plays from.
fn load_ass_file(path: &Path) -> Option<AssSource> {
    let text = match std::fs::read(path) {
        Ok(b) => crate::subtitles::decode_text(&b),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "ASS burn: sidecar unreadable");
            return None;
        }
    };
    let (header, events) = crate::subtitles::ass_file_events(&text);
    if events.is_empty() {
        tracing::warn!(path = %path.display(), "ASS burn: sidecar has no events");
        return None;
    }
    tracing::info!(path = %path.display(), events = events.len(), "ASS burn: sidecar loaded");
    Some(AssSource::File(header, events))
}

/// Link an encode branch and bring it up, reporting failure instead of
/// panicking.
///
/// Every caller runs inside a `pad-added` callback, where a Rust panic
/// cannot unwind out of the C frame and aborts the process instead: no
/// session error, no verdict, no TC-6 retry (the sink is downstream of
/// the panic, so its retry never sees one) — just SIGABRT. Both steps
/// here are genuinely fallible: linking is caps-dependent, and a state
/// change can be refused. `openh264enc` accepts I420 and nothing else,
/// so a tone-mapped chain pinned to NV12 could not link to it, and the
/// `unwrap` that used to be here turned a format mismatch into a dead
/// player for every HDR title on a software-encoder box (field report
/// 2026-08-01).
///
/// Returns false when the branch could not be brought up; the caller
/// abandons it and the session fails on its missing playlist, with the
/// reason in the log.
#[must_use]
fn link_and_start(chain: &[&gst::Element], also: Option<&gst::Element>) -> bool {
    if let Err(e) = gst::Element::link_many(chain.iter().copied()) {
        tracing::error!(error = %e, "encode chain failed to link; session will fail");
        return false;
    }
    for el in also.into_iter().chain(chain.iter().copied()) {
        if let Err(e) = el.sync_state_with_parent() {
            tracing::error!(
                element = %el.name(),
                error = %e,
                "encode element refused its state; session will fail"
            );
            return false;
        }
    }
    true
}

#[allow(clippy::too_many_arguments)] // one plan, spelled out
fn build_video_encode_chain(
    pipe: &gst::Pipeline,
    from: &gst::Pad,
    sinkpad: gst::Pad,
    gate: &Option<Arc<SeekGate>>,
    target: VideoTarget,
    video_kbps: Option<u32>,
    max_height: Option<u32>,
    tone_map: bool,
    burn: Option<std::sync::Arc<crate::burnin::Timeline>>,
    // HUB-32a: Some when the plan burns an ASS track — the element is
    // built here and its text pad published for the subtitle branch.
    ass_burn: Option<Arc<Mutex<AssBurnLink>>>,
) {
    let (encoder, parser) = match target {
        VideoTarget::H264 => (h264_encoder(), "h264parse"),
        VideoTarget::Hevc => (hevc_encoder(), "h265parse"),
        VideoTarget::Av1 => (av1_encoder(), "av1parse"),
    };
    let Some(enc_name) = encoder else {
        tracing::error!(
            target = target.as_str(),
            "video encode routed with no verified encoder"
        );
        return;
    };
    let decode = gst::ElementFactory::make("decodebin").build().unwrap();
    // Hardware decoders output device memory (nvav1dec → CUDAMemory,
    // 10-bit P010) that videoconvert cannot take. NVENC target: stay on
    // the GPU end to end (cudaupload passes CUDA through, uploads system
    // memory; cudaconvert handles format — zero copy for NVDEC→NVENC).
    // Other targets: cudadownload first (passthrough for system memory),
    // then videoconvert. All availability-guarded.
    let converter_names = encode_converter_names(enc_name);
    let converters: Vec<gst::Element> = converter_names
        .iter()
        .map(|n| gst::ElementFactory::make(n).build().unwrap())
        .collect();
    let enc = gst::ElementFactory::make(enc_name).build().unwrap();
    // Sane defaults, guarded per element (props differ across encoders;
    // verified by gst-inspect per candidate):
    //   bitrate         kbit/s  nv*enc, qsv*, va*, x264enc, x265enc, vtenc_*
    //   bitrate         bit/s   rav1enc (the one bps outlier)
    //   target-bitrate  kbit/s  svtav1enc, av1enc (neither has `bitrate`)
    // The plan may clamp (bandwidth cap).
    let kbps = video_kbps.unwrap_or(6000);
    if enc_name == "rav1enc" {
        set_prop_str_if_present(&enc, "bitrate", &(kbps * 1000).to_string());
    } else {
        set_prop_str_if_present(&enc, "bitrate", &kbps.to_string());
        set_prop_str_if_present(&enc, "target-bitrate", &kbps.to_string());
    }
    // A BACKSTOP, not the cadence. The sink asks for a keyframe at every
    // fragment boundary (see make_hls_sink) and that is what actually
    // cuts segments; this only bounds the GOP for an encoder that
    // ignores the request, where the alternative is an encoder-default
    // GOP of ~10 s and a 7 s session start on a 4K HDR encode (measured;
    // the encode itself ran 1.8× realtime and was not the problem).
    //
    // Deliberately FAR longer than the fragment interval. A pin near it
    // competes with the sink's request — the two cadences count
    // different things, frames against seconds, so they cannot line up
    // and the sink cuts at both, which is how segments came out 1.96 s
    // and 1.04 s alternating. Ten seconds never fires first in practice.
    // One name per encoder family, each guarded:
    set_prop_str_if_present(&enc, "gop-size", "240"); // nvenc, qsv
    set_prop_str_if_present(&enc, "key-int-max", "240"); // x264, x265, va
    set_prop_str_if_present(&enc, "max-keyframe-interval", "240"); // vtenc
    set_prop_str_if_present(&enc, "intra-period-length", "240"); // svtav1enc
    set_prop_str_if_present(&enc, "max-key-frame-interval", "240"); // rav1enc
    set_prop_str_if_present(&enc, "keyframe-max-dist", "240"); // av1enc
    let parse = gst::ElementFactory::make(parser).build().unwrap();
    // Parameter sets on every keyframe (independently decodable
    // segments). h26xparse only; av1parse has no such knob.
    set_prop_if_present(&parse, "config-interval", -1i32);

    // HUB-15 resolution ceiling: a RANGE capsfilter after videoscale, so
    // sources already within the ceiling pass through untouched and only
    // larger ones downscale (aspect preserved by videoscale's fixation).
    let scaler: Vec<gst::Element> = match max_height {
        Some(h) if gst::ElementFactory::find("videoscale").is_some() => {
            let scale = gst::ElementFactory::make("videoscale").build().unwrap();
            let caps = gst::Caps::builder("video/x-raw")
                .field("height", gst::IntRange::new(16i32, h as i32))
                .build();
            let filter = gst::ElementFactory::make("capsfilter")
                .property("caps", caps)
                .build()
                .unwrap();
            vec![scale, filter]
        }
        _ => vec![],
    };

    // HUB-15a: the GL segment sits in the same system-memory zone as
    // the scaler, before it — tone-map at source resolution, then
    // scale (scaling PQ-coded pixels before linearizing would blur
    // across the transfer curve; and the shader is per-pixel GPU work,
    // its cost does not care).
    // Ask about THIS target, not "can the box tone-map at all": the
    // segment's output pin comes from this encoder, so that is the only
    // question with an answer (AR-13a).
    let tonemap: Vec<gst::Element> = if tone_map && encoder.is_some_and(tonemap_into) {
        tonemap_segment(encoder.unwrap_or_default())
    } else {
        if tone_map {
            tracing::warn!(
                encoder = encoder.unwrap_or("-"),
                "tone-map requested but the GL segment cannot feed this encoder — encoding as-is"
            );
        }
        vec![]
    };

    // The scaler works on system memory: it must sit right after
    // videoconvert, BEFORE any CUDA upload — a capsfilter on raw caps
    // cannot link against CUDAMemory.
    let scale_at = converter_names
        .iter()
        .position(|n| *n == "videoconvert")
        .map(|i| i + 1)
        .unwrap_or(converters.len());
    // HUB-32b burn-in goes LAST in system memory: after the tone map
    // (subtitle white is already SDR — mapping it through the PQ curve
    // would crush it) and after the scaler (blit at output size, and
    // the rectangles scale to the frame the encoder actually sees).
    // HUB-32a ASS burn: libass renders the styled events onto the same
    // frames, in the same slot and for the same reasons. The text pad is
    // published for whichever pad-added callback needs it — see
    // `AssBurnLink`.
    let ass_el: Vec<gst::Element> = ass_burn
        .as_ref()
        .and_then(|_| {
            // wait-text: hold the picture until the subtitle stream is
            // actually there. Off by default, and off means the encode
            // wins the race every time — a 50-frame clip ran to EOS in
            // 45 ms while assrender logged "rendering disabled, doing
            // buffer passthrough" for every frame, because the text
            // caps had not landed yet.
            gst::ElementFactory::make("assrender")
                .property("wait-text", true)
                .build()
                .ok()
        })
        .into_iter()
        .collect();
    if ass_burn.is_some() && ass_el.is_empty() {
        tracing::error!("ASS burn requested but assrender is missing — encoding without subtitles");
    }
    let burn_wanted = burn.is_some();
    let burn_el: Vec<gst::Element> = burn
        .filter(|t| !t.is_empty())
        .and_then(crate::burnin::blend_element)
        .into_iter()
        .collect();
    if burn_wanted && burn_el.is_empty() {
        tracing::warn!("burn-in requested but no overlay/timeline — encoding without subtitles");
    }

    let mut chain: Vec<&gst::Element> = Vec::new();
    chain.extend(converters[..scale_at].iter());
    chain.extend(tonemap.iter());
    chain.extend(scaler.iter());
    chain.extend(burn_el.iter());
    // link_many picks a compatible pad by caps, so the raw video lands
    // on `video_sink` and never on `text_sink` (application/x-ass).
    chain.extend(ass_el.iter());
    chain.extend(converters[scale_at..].iter());
    chain.push(&enc);
    chain.push(&parse);
    pipe.add(&decode).unwrap();
    pipe.add_many(chain.iter().copied()).unwrap();
    // Shut the gate BEFORE anything can flow OR couple: an ASS burn
    // that lets frames past un-subtitled produces exactly the silent,
    // undetectable failure this tier exists to prevent, and the
    // coupling further down is what opens it again.
    if let Some(link) = &ass_burn
        && !ass_el.is_empty()
    {
        AssBurnLink::shut_gate(link, &chain[0].static_pad("sink").unwrap());
    }
    if !link_and_start(&chain, Some(&decode)) {
        return;
    }
    // AFTER link_and_start, and not a line earlier. Two ways this bites:
    // the pads must share a pipeline or the link fails outright ("no
    // common grandparent"), and `assrender`'s text pad is not ACTIVE
    // until the element has its state — an appsrc linked to an inactive
    // pad pushes once, fails, and pauses its own streaming task, which
    // looks from the outside like a text stream that simply never
    // arrived (measured: every frame logged "rendering disabled, doing
    // buffer passthrough").
    if let (Some(link), Some(el)) = (&ass_burn, ass_el.first())
        && let Some(text) = el.static_pad("text_sink")
    {
        AssBurnLink::offer_text_sink(link, pipe, text);
    }
    let out = parse.static_pad("src").unwrap();
    guard_pts(&out);
    if let Some(g) = gate {
        // A SEEKED start must still hand the muxer a keyframe FIRST.
        // Measured on VideoToolbox: start_ms=0 gives segment00000 its
        // IDR and parameter sets, start_ms=300000 gives 48 slices and
        // none of the three — undecodable, so the player wedges at the
        // resume point while the worker produces minutes of good
        // segments behind it. NVENC leads with an IDR and never showed
        // it, which is why every local test passed.
        open_on_keyframe(&out);
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
/// Canonical AAC input layouts, most channels first. `channel-mask` bits
/// are GStreamer positions: 0x3f = 5.1 (FL FR FC LFE RL RR), 0xc3f adds
/// SL SR (standard side-surround 7.1 — what DTS-HD 7.1 decodes to),
/// 0xff instead adds FLC FRC (7.1 "front wide").
const AAC_LAYOUTS: &[(u32, u64)] = &[(8, 0xc3f), (8, 0xff), (6, 0x3f), (2, 0x3), (1, 0x4)];

/// Can the AAC encoder carry this layout ALL THE WAY to the client —
/// measured once per layout by the real tail: encode, mux into MPEG-TS,
/// demux, decode, and require decoded audio to actually come out.
///
/// Every weaker probe was tried, and each one passed a broken path:
/// - *Does the link form?* The pad template lies (fdkaacenc advertises
///   `channels={1,2,3,4,5,6,8}` unconditionally, then refuses standard
///   side-surround 7.1 caps), and refused caps do not even fail the
///   link — negotiation fixates on the template's first value and the
///   encode silently becomes mono.
/// - *Does a decoder linked directly to the encoder work?* False pass:
///   the decoder reads the channel config from the caps' codec_data,
///   which never survives TS. In ADTS there is only a 3-bit config
///   field, fdk's 8-channel modes are not expressible in it, and the
///   stream decodes nowhere — "channel element 1.1 is not allocated"
///   on every frame, on every ffmpeg, on both fleets.
/// - *EOS-only checking.* Decode failures are per-buffer WARNINGS in
///   GStreamer; a pipeline whose every frame fails still ends in EOS.
///   Success is decoded BUFFERS ARRIVING, nothing less.
/// - *Probing the pin without the source.* A count-only pin passed when
///   audiotestsrc freely negotiated the encoder's favourite layout —
///   but the REAL chain's audioconvert prefers passthrough, handed the
///   encoder the source's side-surround caps it accepts-and-mis-signals,
///   and shipped the broken stream the probe had just blessed. The probe
///   therefore stages the source's own (channels, mask) upstream of the
///   pin, exactly like the pipeline it stands in for.
fn aac_accepts(enc: &str, source: (u32, u64), channels: u32, mask: Option<u64>) -> bool {
    type Key = ((u32, u64), u32, Option<u64>);
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<Key, bool>>> =
        std::sync::OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    if let Some(hit) = seen.lock().unwrap().get(&(source, channels, mask)) {
        return *hit;
    }
    // avdec_aac is libav — the same decoder family as ffmpeg and the
    // browsers, i.e. the strictness that actually matters. Other gst
    // decoders may share the encoder's dialect and false-pass.
    const DECODERS: &[&str] = &["avdec_aac", "fdkaacdec", "faad"];
    let dec = DECODERS
        .iter()
        .find(|d| gst::ElementFactory::find(d).is_some());
    let (sch, smask) = source;
    let src = if smask != 0 {
        format!("audio/x-raw,channels={sch},channel-mask=(bitmask)0x{smask:x}")
    } else {
        format!("audio/x-raw,channels={sch}")
    };
    let pin = match mask {
        Some(m) => format!("audio/x-raw,channels={channels},channel-mask=(bitmask)0x{m:x}"),
        None => format!("audio/x-raw,channels={channels}"),
    };
    let ok = match dec {
        Some(dec) => dry_run_yields_output(&format!(
            "audiotestsrc num-buffers=10 ! audioconvert ! {src} \
             ! audioconvert ! {pin} ! audioresample \
             ! {enc} ! aacparse ! mpegtsmux ! tsdemux ! aacparse ! {dec} \
             ! audio/x-raw,channels={channels} ! fakesink name=probesink"
        )),
        // No decoder at all: nothing can verify the bitstream, so only
        // layouts that every known fdk/libav build signals correctly in
        // ADTS are trusted (5.1 and below).
        None => {
            channels <= 6
                && dry_run(&format!(
                    "audiotestsrc num-buffers=5 ! audioconvert ! {src} \
                     ! audioconvert ! {pin} ! audioresample ! {enc} ! fakesink"
                ))
        }
    };
    tracing::debug!(
        encoder = enc,
        ?source,
        channels,
        ?mask,
        accepted = ok,
        "AAC layout probe"
    );
    seen.lock().unwrap().insert((source, channels, mask), ok);
    ok
}

/// [`dry_run`], plus the requirement that at least one buffer reaches the
/// sink named `probesink` — the difference between "the pipeline ended"
/// and "the pipeline produced anything" (see [`aac_accepts`]).
fn dry_run_yields_output(launch: &str) -> bool {
    let Ok(p) = gst::parse::launch(launch) else {
        return false;
    };
    let Some(pipe) = p.downcast_ref::<gst::Pipeline>() else {
        return false;
    };
    let count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let Some(sinkpad) = pipe.by_name("probesink").and_then(|s| s.static_pad("sink")) else {
        return false;
    };
    let c = count.clone();
    sinkpad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });
    if p.set_state(gst::State::Playing).is_err() {
        return false;
    }
    let eos = p
        .bus()
        .and_then(|bus| {
            bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
        })
        .is_some_and(|msg| msg.type_() == gst::MessageType::Eos);
    let _ = p.set_state(gst::State::Null);
    eos && count.load(std::sync::atomic::Ordering::Relaxed) > 0
}

/// Channel count as the phrase a viewer knows it by.
fn layout_label(channels: u32) -> String {
    match channels {
        1 => "mono".into(),
        2 => "stereo".into(),
        6 => "5.1".into(),
        8 => "7.1".into(),
        n => format!("{n}ch"),
    }
}

/// The layout to pin on the encoder's input for a decoded stream of
/// `channels`/`mask`, never above `ceiling` (the client's, HUB-15).
///
/// The source's own layout when the encoder round-trips it — a 7.1 file
/// the encoder can carry stays 7.1. Otherwise the largest layout it does
/// carry whose positions the source actually HAS, so what happens is a
/// real fold (7.1 side surround → 5.1) and never a relabel of surround
/// content into front-wide channels the source never had. Positioned
/// candidates come first; count-only ones follow, because an encoder can
/// refuse an explicit mask it is perfectly able to produce itself.
/// None = nothing round-tripped, down to mono: leave negotiation alone
/// so the encoder's own behaviour, good or bad, is what ships.
fn aac_input_layout(
    enc: &str,
    channels: u32,
    mask: u64,
    ceiling: Option<u32>,
) -> Option<(u32, Option<u64>)> {
    let bound = ceiling
        .filter(|c| *c > 0)
        .map_or(channels, |c| c.min(channels));
    // An unpositioned stream (mask 0, common for mono/stereo) claims no
    // positions, so nothing is a relabel and every small enough layout
    // is fair game.
    let has_positions = |m: u64| mask == 0 || m & mask == m;
    let positioned = std::iter::once((channels, Some(mask)))
        .filter(|_| mask != 0 && channels <= bound)
        .chain(
            AAC_LAYOUTS
                .iter()
                .filter(|(n, m)| *n <= bound && has_positions(*m))
                .map(|(n, m)| (*n, Some(*m))),
        );
    let count_only = std::iter::once(channels)
        .chain(AAC_LAYOUTS.iter().map(|(n, _)| *n))
        .filter(move |n| *n <= bound)
        .map(|n| (n, None));
    positioned
        .chain(count_only)
        .find(|(n, m)| aac_accepts(enc, (channels, mask), *n, *m))
}

/// Pin the encoder's input layout from the decoded caps, on the caps
/// event that precedes the first buffer — i.e. before the encoder
/// negotiates, which is the whole point (see [`aac_accepts`]).
fn install_layout_pin(
    pad: &gst::Pad,
    filter: &gst::Element,
    ceiling: Option<u32>,
    enc: &str,
    facts_dir: &std::path::Path,
) {
    let filter = filter.clone();
    let enc = enc.to_string();
    let facts_dir = facts_dir.to_path_buf();
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        let Some(gst::PadProbeData::Event(ev)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(c) = ev.view() else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(s) = c.caps().structure(0) else {
            return gst::PadProbeReturn::Remove;
        };
        let channels: u32 = s.get::<i32>("channels").unwrap_or(0).max(0) as u32;
        let mask = s
            .get::<gst::Bitmask>("channel-mask")
            .map(|b| *b)
            .unwrap_or(0);
        if channels == 0 {
            return gst::PadProbeReturn::Remove;
        }
        match aac_input_layout(&enc, channels, mask, ceiling) {
            Some((n, m)) => {
                let mut b = gst::Caps::builder("audio/x-raw").field("channels", n as i32);
                if let Some(m) = m {
                    b = b.field("channel-mask", gst::Bitmask::new(m));
                }
                filter.set_property("caps", b.build());
                // Logged unconditionally: the one thing this bug proved is
                // that a silent audio path is an unverifiable one.
                tracing::info!(
                    source_channels = channels,
                    source_mask = format!("0x{mask:x}"),
                    encoded_channels = n,
                    encoded_mask = m.map(|m| format!("0x{m:x}")).unwrap_or_else(|| "any".into()),
                    encoder = %enc,
                    "AAC input layout pinned"
                );
                // A fold is a fact the verdict promised nothing about.
                if n != channels {
                    crate::facts::report(
                        &facts_dir,
                        "audio",
                        format!("{} → {}", layout_label(channels), layout_label(n)),
                    );
                }
            }
            None => {
                tracing::warn!(
                    channels,
                    mask = format!("0x{mask:x}"),
                    encoder = %enc,
                    "no AAC input layout accepted; leaving negotiation to the encoder"
                );
                crate::facts::report(
                    &facts_dir,
                    "audio",
                    format!("{} has no encodable layout", layout_label(channels)),
                );
            }
        }
        gst::PadProbeReturn::Remove
    });
}

#[allow(clippy::too_many_arguments)] // one plan, spelled out
fn build_audio_encode_chain(
    pipe: &gst::Pipeline,
    from: &gst::Pad,
    sinkpad: gst::Pad,
    caps_name: &str,
    gate: &Option<Arc<SeekGate>>,
    target: AudioTarget,
    max_channels: Option<u32>,
    facts_dir: &std::path::Path,
) {
    let encoder = match target {
        AudioTarget::Aac => aac_encoder(),
        AudioTarget::Opus => opus_encoder(),
    };
    let Some(enc_name) = encoder else {
        // Planner guarantees this; guard anyway (fakesink beats a stall).
        tracing::error!(
            target = target.as_str(),
            "audio encode routed with no verified encoder"
        );
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
        build_audio_tail(
            pipe,
            from,
            sinkpad,
            enc_name,
            target,
            &[setter, dec],
            gate,
            max_channels,
            facts_dir,
        );
        return;
    }
    let decode = gst::ElementFactory::make("decodebin").build().unwrap();
    let convert = gst::ElementFactory::make("audioconvert").build().unwrap();
    // audioconvert does the remap; this capsfilter says what to remap TO,
    // filled in from the decoded caps once they are known (the HUB-15
    // client ceiling is one bound on that choice, the encoder's own
    // accepted layouts the other).
    let limiter = gst::ElementFactory::make("capsfilter").build().unwrap();
    let resample = gst::ElementFactory::make("audioresample").build().unwrap();
    let enc = gst::ElementFactory::make(enc_name).build().unwrap();
    // 192000: bit/s on fdkaacenc/avenc_aac AND opusenc — same string.
    set_prop_str_if_present(&enc, "bitrate", "192000");
    // AAC rides through aacparse (ADTS/raw negotiation with the muxer);
    // opusenc's output needs no parser.
    let parse: Option<gst::Element> = match target {
        AudioTarget::Aac => Some(gst::ElementFactory::make("aacparse").build().unwrap()),
        AudioTarget::Opus => {
            // The layout-pin machinery below exists because fdkaacenc
            // lies about 7.1; opusenc does not — its ceiling is a plain
            // caps bound (channel-mapping-family covers ≤8ch).
            opus_limit(&limiter, max_channels);
            None
        }
    };

    let mut chain: Vec<&gst::Element> = vec![&convert, &limiter, &resample, &enc];
    chain.extend(parse.iter());
    pipe.add(&decode).unwrap();
    pipe.add_many(chain.iter().copied()).unwrap();
    if !link_and_start(&chain, Some(&decode)) {
        return;
    }
    let out = chain.last().unwrap().static_pad("src").unwrap();
    guard_pts(&out);
    if let Some(g) = gate {
        g.install(&out);
    }
    if let Err(e) = out.link(&sinkpad) {
        tracing::warn!(error = %e, "remux: encode chain → muxer link failed");
    }
    let convert_sink = convert.static_pad("sink").unwrap();
    let pin_target = limiter.clone();
    let pin_enc = enc_name.to_string();
    let pin_dir = facts_dir.to_path_buf();
    decode.connect_pad_added(move |_, pad| {
        if convert_sink.is_linked() {
            return; // first decoded stream wins
        }
        if target == AudioTarget::Aac {
            install_layout_pin(pad, &pin_target, max_channels, &pin_enc, &pin_dir);
        }
        if let Err(e) = pad.link(&convert_sink) {
            tracing::warn!(error = %e, "remux: decodebin → encode chain link failed");
        }
    });
    if let Err(e) = from.link(&decode.static_pad("sink").unwrap()) {
        tracing::warn!(error = %e, "remux: → decodebin link failed");
    }
}

/// Opus channel bound: a plain caps range up to min(ceiling, 8) —
/// opusenc handles multichannel natively (channel-mapping-family 1),
/// so no accept-probe is needed, just the client's ceiling.
fn opus_limit(limiter: &gst::Element, max_channels: Option<u32>) {
    let max = max_channels.unwrap_or(8).min(8) as i32;
    limiter.set_property(
        "caps",
        gst::Caps::builder("audio/x-raw")
            .field("channels", gst::IntRange::new(1, max))
            .build(),
    );
}

/// Static front-end variant of the audio encode chain: `from` →
/// front elements → audioconvert → audioresample → encoder → aacparse →
/// muxer pad. Used when the decoder must be chosen explicitly instead of
/// trusting decodebin's caps-based autoplug.
#[allow(clippy::too_many_arguments)] // internal fan-out point: one call site
fn build_audio_tail(
    pipe: &gst::Pipeline,
    from: &gst::Pad,
    sinkpad: gst::Pad,
    enc_name: &str,
    target: AudioTarget,
    front: &[gst::Element],
    gate: &Option<Arc<SeekGate>>,
    max_channels: Option<u32>,
    facts_dir: &std::path::Path,
) {
    let convert = gst::ElementFactory::make("audioconvert").build().unwrap();
    let limiter = gst::ElementFactory::make("capsfilter").build().unwrap();
    let resample = gst::ElementFactory::make("audioresample").build().unwrap();
    let enc = gst::ElementFactory::make(enc_name).build().unwrap();
    set_prop_str_if_present(&enc, "bitrate", "192000");
    let parse: Option<gst::Element> = match target {
        AudioTarget::Aac => Some(gst::ElementFactory::make("aacparse").build().unwrap()),
        AudioTarget::Opus => {
            opus_limit(&limiter, max_channels);
            None
        }
    };

    // The explicit decoder's own src pad carries the decoded caps, so the
    // layout is pinned from there instead of a decodebin pad-added.
    if target == AudioTarget::Aac
        && let Some(src) = front.last().and_then(|el| el.static_pad("src"))
    {
        install_layout_pin(&src, &limiter, max_channels, enc_name, facts_dir);
    }

    let mut chain: Vec<&gst::Element> = front.iter().collect();
    chain.extend([&convert, &limiter, &resample, &enc]);
    chain.extend(parse.iter());
    pipe.add_many(chain.iter().copied()).unwrap();
    if !link_and_start(&chain, None) {
        return;
    }
    let out = chain.last().unwrap().static_pad("src").unwrap();
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
    /// Feed generation at request time — a Need stamped before a seek
    /// must never be served after it: with slow sources (lease/socket)
    /// a stale block can land after flush-stop and push old-position
    /// bytes into the new segment, which the demuxer then parses as
    /// garbage ("large block, file might be corrupt"). The byte count
    /// appsrc asks for is irrelevant since the prefetch ring sized its
    /// blocks already.
    Need(u64),
}

pub struct RemuxJob {
    pipeline: gst::Pipeline,
    error: Arc<Mutex<Option<String>>>,
    finished: Arc<std::sync::atomic::AtomicBool>,
    /// Unblocks pacing probes on teardown.
    stopping: Arc<std::sync::atomic::AtomicBool>,
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
            HLS_SINKS
                .iter()
                .find(|n| **n == p && gst::ElementFactory::find(n).is_some())
        })
        .or_else(|| {
            HLS_SINKS
                .iter()
                .find(|n| gst::ElementFactory::find(n).is_some())
        })
        .context("no HLS sink element (hlssink3/hlssink2) — see `kahawai doctor`")?;
    let sink = gst::ElementFactory::make(name).build()?;
    set_prop_if_present(
        &sink,
        "location",
        out_dir.join("segment%05d.ts").to_str().unwrap(),
    );
    set_prop_if_present(
        &sink,
        "playlist-location",
        out_dir.join("master.m3u8").to_str().unwrap(),
    );
    // Segments cut at the first keyframe AT OR PAST the target, so with
    // the encoders' 2 s GOP every segment runs slightly OVER 2 s — and
    // the spec requires TARGETDURATION >= ceil(max segment duration),
    // which hlssink3 writes verbatim from this property. 3 is therefore
    // the smallest spec-valid value for ~2 s segments (2 produced
    // playlists that violated it).
    // The fragment interval the sink asks keyframes at, and therefore
    // the segment length: ~2 s keeps the session-start gate (three
    // segments) at ~6 s of content.
    set_prop_if_present(&sink, "target-duration", FRAGMENT_TARGET_SECS);
    // ON, and load-bearing: the request is what puts a keyframe at the
    // START of the first fragment. Turned off for one evening to stop
    // the sink racing a frame-based GOP pin, it made segment00000
    // undecodable — the fragment opened before the encoder's first IDR,
    // so the segment carried slices with no SPS/PPS and every player
    // wedged on it at ~1 s while the worker happily produced two minutes
    // more. The double-cut it was meant to fix was cosmetic; this was
    // not. The pin above is now a far-away backstop instead, so the two
    // no longer compete.
    //
    // Copy sessions are unaffected either way: with no encoder upstream
    // there is nothing to honour a keyframe request, so their splits
    // follow the source's own keyframes.
    set_prop_if_present(&sink, "send-keyframe-requests", true);
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

/// Pacing window (§4.6): hold muxer-bound buffers whose media time runs
/// more than `window_ms` past the viewer's position (read from
/// `viewer_file`, absolute ms; absent = `floor_ms`). In-band and
/// deterministic — polling pause/resume loses to pipelines that finish
/// a file between two polls.
pub struct PaceConfig {
    pub window_ms: u64,
    pub floor_ms: u64,
    pub viewer_file: std::path::PathBuf,
    /// HUB-36: where to drop `pace.json` — the run dir, alongside
    /// `start.pos` and `viewer.pos`.
    pub out_dir: std::path::PathBuf,
}

/// HUB-36: how fast this box ACTUALLY produced content, measured only
/// while nothing was holding it back.
///
/// The trap this exists to dodge: steady-state production is throttled
/// to viewer+window by the probe below, so a session measured end to
/// end reports ~1.0× **because we paced it** — the number would say
/// "every box is realtime" and rank nothing. Honest pace is visible
/// only while the throttle is asleep: from the first buffer until the
/// window check first fails (or a cap, for a box slow enough never to
/// fill the window). Post-seek catch-up is un-throttled by the same
/// definition, and each seek-restart is a fresh worker, so it gets its
/// own sample.
struct PaceMeter {
    t0: Option<std::time::Instant>,
    first_ms: u64,
    done: bool,
}

/// A sample needs this much CONTENT to mean anything — below it the
/// ratio is dominated by preroll burst rather than throughput.
const PACE_MIN_CONTENT: u64 = 5_000;
/// …and this much WALL time, only to keep timer jitter out. It is
/// deliberately small: a fast box produces the whole pacing window in
/// under a second, and rejecting that would discard precisely the
/// boxes worth ranking highest (found by test — a local copy-remux
/// cleared a 6 s window in ~0.3 s and reported nothing at all).
const PACE_MIN_WALL: u64 = 1_000;
/// A box near 1.0× may never fill the window; stop measuring anyway.
const PACE_CAP: std::time::Duration = std::time::Duration::from_secs(60);

/// content-ms over wall-ms as a realtime multiple, or None when the
/// sample is too short on either axis to mean anything (the opening
/// seconds are preroll burst, not throughput).
pub(crate) fn pace_multiple(content_ms: u64, wall_ms: u64) -> Option<f64> {
    if wall_ms < PACE_MIN_WALL || content_ms < PACE_MIN_CONTENT {
        return None;
    }
    Some(content_ms as f64 / wall_ms as f64)
}

impl PaceMeter {
    /// Close the measurement and write it out. `produced_ms` is the
    /// position of the buffer that ended it.
    fn finish(&mut self, produced_ms: u64, out_dir: &std::path::Path) {
        self.done = true;
        let Some(t0) = self.t0 else { return };
        let wall = t0.elapsed();
        let content = produced_ms.saturating_sub(self.first_ms);
        let Some(multiple) = pace_multiple(content, wall.as_millis() as u64) else {
            return; // too short to mean anything
        };
        let path = out_dir.join("pace.json");
        let body = format!("{{\"multiple\":{multiple:.3}}}");
        if let Err(e) = std::fs::write(&path, body) {
            tracing::debug!(error = %e, "pace sample not written");
            return;
        }
        tracing::info!(
            multiple = format!("{multiple:.2}"),
            content_ms = content,
            wall_ms = wall.as_millis() as u64,
            "un-throttled production measured (HUB-36)"
        );
    }
}

fn install_pace_probe(
    pad: &gst::Pad,
    cfg: Arc<PaceConfig>,
    stopping: Arc<std::sync::atomic::AtomicBool>,
    meter: Option<Arc<Mutex<PaceMeter>>>,
) {
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        if let Some(gst::PadProbeData::Buffer(b)) = &info.data
            && let Some(pts) = b.pts()
        {
            // Raw PTS can carry arbitrary bases (x264's 1000-hour
            // epoch); running time is the honest produced-position —
            // it starts at zero for the run, so add the start offset.
            let Some(rt) = pad
                .sticky_event::<gst::event::Segment>(0)
                .and_then(|e| e.segment().downcast_ref::<gst::ClockTime>().cloned())
                .and_then(|seg| seg.to_running_time(pts))
            else {
                return gst::PadProbeReturn::Ok;
            };
            let produced_ms = cfg.floor_ms + rt.mseconds();
            // HUB-36: the clock starts at the first buffer of the run.
            if let Some(m) = &meter {
                let mut m = m.lock().unwrap();
                if !m.done && m.t0.is_none() {
                    m.t0 = Some(std::time::Instant::now());
                    m.first_ms = produced_ms;
                } else if !m.done && m.t0.is_some_and(|t| t.elapsed() >= PACE_CAP) {
                    // Never filled the window — a ~1.0x box. Its speed
                    // is exactly what the last minute showed.
                    m.finish(produced_ms, &cfg.out_dir);
                }
            }
            loop {
                if stopping.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                let viewer = std::fs::read_to_string(&cfg.viewer_file)
                    .ok()
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(cfg.floor_ms)
                    .max(cfg.floor_ms);
                if produced_ms <= viewer + cfg.window_ms {
                    break;
                }
                // THE signal: production has outrun the viewer window,
                // so from here on the throttle — not the box — sets the
                // rate. Whatever we measured up to now is the honest
                // number; everything after would read ~1.0x.
                if let Some(m) = &meter {
                    let mut m = m.lock().unwrap();
                    if !m.done {
                        m.finish(produced_ms, &cfg.out_dir);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
        gst::PadProbeReturn::Ok
    });
}

/// Full-control variant: offset plus an HLS sink override (TC-6 retry)
/// and an optional pacing window.
pub fn start_full(
    out_dir: &Path,
    plan: RemuxPlan,
    source: Box<dyn RemuxSource>,
    start_ms: u64,
    sink: Option<&str>,
) -> Result<RemuxJob> {
    start_paced(out_dir, plan, source, start_ms, sink, None)
}

/// A seekable appsrc fed from a `RemuxSource` through a PREFETCH RING:
/// a reader thread streams ahead of the pipeline into a bounded buffer,
/// and stalls when it is full — "stream until pushback", expressed
/// locally instead of as flow-control games on the shared control link
/// (AR-12: never head-of-line-block the heartbeats).
///
/// Why a ring at all: the old feeder read one 256 KB block per appsrc
/// Need, serially — and for a dispatched worker every block is a full
/// worker→transcoder→hub→lease round trip. Measured on a 4K HDR title:
/// the byte plane delivered ~2 MB/s (≈ the file's own bitrate), capping
/// EVERY session near 1.0× realtime while the same pipeline ran 4–6.6×
/// against a local file — a video COPY session crawled identically,
/// which is what convicted transport over compute. Large blocks
/// amortize the round trip; the ring overlaps fetch with the pipeline.
///
/// Seek correctness is generation-based, as before: a block read for
/// generation N is dropped once a seek bumps to N+1, so a slow in-
/// flight read can never land pre-seek bytes after flush-stop.
pub(crate) fn seekable_appsrc(mut source: Box<dyn RemuxSource>) -> AppSrc {
    /// One fetch, sized to amortize the byte-plane round trip.
    const READ_BLOCK: usize = 2 * 1024 * 1024;
    /// Ring capacity — the pushback point. 16 MB ≈ 9 s of a 4K HDR
    /// film ahead of the pipeline, bounded per part-source.
    const RING_BYTES: usize = 16 * 1024 * 1024;

    struct Ring {
        blocks: std::collections::VecDeque<(u64, Vec<u8>)>,
        bytes: usize,
        /// Bumped by seek_data; blocks and Needs from an older
        /// generation are stale and dropped.
        generation: u64,
        /// Where the reader resumes after a seek.
        seek_to: Option<u64>,
        /// Reader reached EOF (for the current generation).
        eos: bool,
        /// Reader hit a fatal read error.
        failed: bool,
    }
    let ring = Arc::new((
        Mutex::new(Ring {
            blocks: std::collections::VecDeque::new(),
            bytes: 0,
            generation: 0,
            seek_to: None,
            eos: false,
            failed: false,
        }),
        std::sync::Condvar::new(),
    ));

    let appsrc = AppSrc::builder()
        .stream_type(gstreamer_app::AppStreamType::Seekable)
        .block(true)
        .max_bytes(8 * 1024 * 1024)
        .build();
    appsrc.set_size(source.size() as i64);

    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<FeedCmd>();
    // Held by the feeder for the duration of each Need. seek_data takes
    // it after bumping the generation: any in-flight feed then finishes
    // inside the flush (its push fails Flushing — appsrc unblocks
    // producers before invoking seek_data), so a stale block can never
    // land after flush-stop.
    let busy: Arc<Mutex<()>> = Arc::new(Mutex::new(()));

    let ring_need = ring.clone();
    let ring_seek = ring.clone();
    let busy_seek = busy.clone();
    appsrc.set_callbacks(
        gstreamer_app::AppSrcCallbacks::builder()
            .need_data(move |_, _length| {
                let stamp = ring_need.0.lock().unwrap().generation;
                let _ = cmd_tx.send(FeedCmd::Need(stamp));
            })
            .seek_data(move |_, offset| {
                {
                    let (lock, cv) = &*ring_seek;
                    let mut r = lock.lock().unwrap();
                    r.generation += 1;
                    r.seek_to = Some(offset);
                    r.blocks.clear();
                    r.bytes = 0;
                    r.eos = false;
                    cv.notify_all();
                }
                drop(busy_seek.lock().unwrap());
                true
            })
            .build(),
    );

    // Reader: streams ahead until the ring pushes back. Owns the source.
    let ring_rd = ring.clone();
    std::thread::spawn(move || {
        let mut pos: u64 = 0;
        let mut my_gen: u64 = 0;
        loop {
            // Wait for room (or a reason to reposition/stop).
            {
                let (lock, cv) = &*ring_rd;
                let mut r = lock.lock().unwrap();
                loop {
                    if r.generation != my_gen {
                        my_gen = r.generation;
                        if let Some(t) = r.seek_to.take() {
                            pos = t;
                        }
                        break;
                    }
                    if r.failed || (!r.eos && r.bytes < RING_BYTES) {
                        break;
                    }
                    r = cv.wait(r).unwrap();
                }
                if r.failed {
                    return;
                }
                if r.eos {
                    continue; // parked until a seek revives us
                }
            }
            let mut buf = vec![0u8; READ_BLOCK];
            let result = source.read_at(pos, &mut buf);
            let (lock, cv) = &*ring_rd;
            let mut r = lock.lock().unwrap();
            if r.generation != my_gen {
                continue; // seek raced the read: bytes are stale
            }
            match result {
                Ok(0) => {
                    r.eos = true;
                    cv.notify_all();
                }
                Ok(n) => {
                    buf.truncate(n);
                    r.bytes += n;
                    r.blocks.push_back((pos, buf));
                    pos += n as u64;
                    cv.notify_all();
                }
                Err(e) => {
                    tracing::warn!(error = %e, "remux source read failed; ending stream");
                    r.failed = true;
                    cv.notify_all();
                }
            }
        }
    });

    // Feeder: serves appsrc Needs from the ring. No I/O of its own.
    let feeder_src = appsrc.clone();
    let ring_fd = ring;
    std::thread::spawn(move || {
        while let Ok(FeedCmd::Need(stamp)) = cmd_rx.recv() {
            let _busy = busy.lock().unwrap();
            let block = {
                let (lock, cv) = &*ring_fd;
                let mut r = lock.lock().unwrap();
                loop {
                    if r.generation != stamp {
                        break None; // stamped before a seek: stale, drop
                    }
                    if let Some((off, bytes)) = r.blocks.pop_front() {
                        r.bytes -= bytes.len();
                        cv.notify_all(); // room: wake the reader
                        break Some(Ok((off, bytes)));
                    }
                    if r.eos {
                        break Some(Err(true));
                    }
                    if r.failed {
                        break Some(Err(false));
                    }
                    r = cv.wait(r).unwrap();
                }
            };
            match block {
                None => continue,
                Some(Err(_eos_or_fail)) => {
                    let _ = feeder_src.end_of_stream();
                }
                Some(Ok((offset, bytes))) => {
                    let mut b = gst::Buffer::from_mut_slice(bytes);
                    b.get_mut().unwrap().set_offset(offset);
                    // Err = Flushing (seek in progress) or shutdown;
                    // either a new Need follows or recv fails.
                    let _ = feeder_src.push_buffer(b);
                }
            }
        }
    });
    appsrc
}

pub fn start_paced(
    out_dir: &Path,
    plan: RemuxPlan,
    source: Box<dyn RemuxSource>,
    start_ms: u64,
    sink: Option<&str>,
    pace: Option<PaceConfig>,
) -> Result<RemuxJob> {
    start_parts(
        out_dir,
        plan,
        vec![source],
        start_ms,
        sink,
        pace,
        None,
        None,
    )
}

/// One pipeline spanning a multi-part source, in timeline order.
///
/// A CD1/CD2 boundary used to be a pipeline restart: stop the worker,
/// delete the segments, start again in the next file, and let the client
/// stitch the two playlists together when the video element fired
/// `ended`. The most predictable event in the file paid the price of a
/// random seek. Here the parts are branches of ONE pipeline joined by
/// `concat`, so the boundary produces no event at all — one playlist,
/// continuous running time, no discontinuity tag.
///
/// `start_ms` applies to the FIRST part only; the rest play whole, which
/// is what makes them concatenable. Seeking is NOT done this way: concat
/// accepts a seek after preroll and then plays from zero, and refuses one
/// during playback outright (both measured — see `concat_spike`). A seek
/// therefore stays what it is today, a restart in the target part, and
/// this function is handed the parts from that point on.
#[allow(clippy::too_many_arguments)] // one pipeline, spelled out
pub fn start_parts(
    out_dir: &Path,
    plan: RemuxPlan,
    sources: Vec<Box<dyn RemuxSource>>,
    start_ms: u64,
    sink: Option<&str>,
    pace: Option<PaceConfig>,
    // HUB-32b: display sets read for us (mediahost-side). None = walk
    // the source index ourselves, affordable only for local sources.
    burn_sets: Option<&Path>,
    // HUB-32a: a user's own sidecar .ass to burn into the picture. An
    // EMBEDDED ASS burn comes from `plan.burn_ass` instead and takes the
    // demuxer's pad, because that is the only path carrying the file's
    // attached fonts; a sidecar has none and renders with system fonts.
    burn_ass_file: Option<&Path>,
) -> Result<RemuxJob> {
    crate::init()?;
    // Dolby Digital Plus reaches us as ONE container block holding an
    // AC-3 core plus its E-AC-3 extension substream. ac3parse splits
    // that block into two "frames" and interpolates a timestamp
    // between them, so a 32 ms unit leaves the parser as two buffers
    // 16 ms apart — and the audio timeline comes out at HALF the audio
    // content, i.e. sound racing the picture 2:1 (measured on a DD+ 7.1
    // title: 60 s of samples stamped across 30 s; 2815 AAC frames of
    // content muxed against 30 s of video). avdec_eac3 — which
    // build_audio_encode_chain forces for the whole AC-3 family anyway
    // — decodes the intact block correctly, and yields the full 7.1
    // that the split path silently reduces to the 5.1 core. So while
    // the audio is being decoded the parser buys nothing and costs
    // sync; demote it and let parsebin expose the block whole.
    //
    // Decode only: a COPY still needs the parser's framed caps to mux.
    // The rank is process-global, which is safe precisely because every
    // real pipeline runs in its own `remux-worker` child — one run per
    // process, both from the transcoder and from the hub.
    if plan.audio == StreamMode::Encode
        && let Some(f) = gst::ElementFactory::find("ac3parse")
    {
        f.set_rank(gst::Rank::NONE);
    }
    anyhow::ensure!(!sources.is_empty(), "no source parts to remux");
    let multipart = sources.len() > 1;
    let mut sources = sources;

    // HUB-32b burn-in: read the display-set timeline from the FIRST
    // part's own container index before its bytes are handed to the
    // pipeline (the source is random-access and stateless, so the walk
    // costs a few scattered kilobytes and leaves nothing behind). Doing
    // it up front — rather than following the demuxer's subtitle pad —
    // is what makes a session that STARTS mid-set show that set.
    let burn_timeline = match (burn_sets, plan.burn_subtitle) {
        // Handed to us: no walk at all, and correct wherever the
        // source lives.
        (Some(path), _) => match crate::burnin::timeline_from_file(path) {
            Ok(Some(t)) if !t.is_empty() => {
                tracing::info!(sets = %path.display(), entries = t.len(),
                    "burn-in: display sets loaded");
                Some(std::sync::Arc::new(t))
            }
            Ok(_) => {
                tracing::warn!(sets = %path.display(), "burn-in: display sets empty");
                None
            }
            Err(e) => {
                tracing::warn!(sets = %path.display(), error = format!("{e:#}"),
                    "burn-in: display sets unreadable");
                None
            }
        },
        (None, Some(idx)) => {
            let t0 = std::time::Instant::now();
            // Bounded: a session must start even when the timeline
            // cannot be had. Local sources finish in milliseconds; a
            // lease-backed one may not finish at all, and then the
            // encode runs without the burn and the verdict says so.
            match crate::burnin::timeline(&mut *sources[0], idx, BURN_INDEX_BUDGET) {
                Ok(Some(t)) if !t.is_empty() => {
                    tracing::info!(
                        track = idx,
                        sets = t.len(),
                        ms = t0.elapsed().as_millis(),
                        "burn-in: display-set timeline read"
                    );
                    Some(std::sync::Arc::new(t))
                }
                Ok(_) => {
                    tracing::warn!(track = idx, "burn-in: no display sets — burning nothing");
                    None
                }
                Err(e) => {
                    tracing::warn!(
                        track = idx,
                        error = format!("{e:#}"),
                        "burn-in: timeline failed"
                    );
                    None
                }
            }
        }
        (None, None) => None,
    };

    // Both halves of an ASS burn meet here — see `AssBurnLink`. Shared
    // across parts on purpose, unlike the per-part track counters: the
    // video chain is built once, from whichever part reaches it first.
    let ass_link = (plan.burn_ass.is_some() || burn_ass_file.is_some())
        .then(|| Arc::new(Mutex::new(AssBurnLink::default())));

    let pipeline = gst::Pipeline::new();
    if let (Some(path), Some(link)) = (burn_ass_file, &ass_link)
        && let Some(source) = load_ass_file(path)
    {
        AssBurnLink::offer_source(link, source);
    }
    // The segment sink pair: TS = the hlssink family (TC-6 prefer
    // override intact); fMP4 = isofmp4mux + the fmp4sink writer. The
    // sink override is a TS-crash workaround and means nothing here.
    enum SegSink {
        Ts(gst::Element),
        Fmp4(gst::Element),
    }
    let hlssink = match plan.segment_format {
        SegmentFormat::Ts => {
            let (el, _name) = make_hls_sink(out_dir, sink)?;
            pipeline.add(&el)?;
            SegSink::Ts(el)
        }
        SegmentFormat::Fmp4 => SegSink::Fmp4(crate::fmp4sink::attach(&pipeline, out_dir)?),
    };

    // The first part owns the start offset and the seek gate; later parts
    // are held by concat until it EOSes, then play from their own zero.
    let mut parsebins = Vec::with_capacity(sources.len());
    for source in sources {
        let appsrc = seekable_appsrc(source);
        let parsebin = gst::ElementFactory::make("parsebin").build()?;
        // Keep parsebin's own parser away from AV1, and only AV1.
        //
        // `autoplug-continue` is asked before each element is plugged;
        // returning false exposes the stream as the demuxer produced it.
        // Container caps are never in `parsebin_must_not_parse`, so
        // demuxing always continues.
        parsebin.connect("autoplug-continue", false, |args| {
            let caps = args[2].get::<gst::Caps>().ok()?;
            Some((!parsebin_must_not_parse(&caps)).to_value())
        });
        pipeline.add_many([appsrc.upcast_ref::<gst::Element>(), &parsebin])?;
        gst::Element::link_many([appsrc.upcast_ref::<gst::Element>(), &parsebin])?;
        parsebins.push(parsebin);
    }
    let parsebin = parsebins[0].clone();

    // Request the muxer pads *now* — splitmuxsink inside hlssink2 must see
    // them before starting or it never leaves Ready.
    anyhow::ensure!(plan.playable(), "nothing to remux");
    let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pace = pace.map(Arc::new);
    let mut pace_meter = pace.as_ref().map(|_| {
        Arc::new(Mutex::new(PaceMeter {
            t0: None,
            first_ms: 0,
            done: false,
        }))
    });
    let per_part: Vec<WaitingPads> = (0..parsebins.len())
        .map(|_| Arc::new(Mutex::new(std::collections::HashMap::new())))
        .collect();
    for kind in ["video", "audio"] {
        let wanted = if kind == "video" {
            plan.has_video()
        } else {
            plan.has_audio()
        };
        if !wanted {
            continue;
        }
        // TS sinks name their pads; the fMP4 mux counts them — the
        // loop order (video first) puts video on track 0 either way.
        let pad = match &hlssink {
            SegSink::Ts(el) => el.request_pad_simple(kind),
            SegSink::Fmp4(mux) => mux.request_pad_simple("sink_%u"),
        }
        .with_context(|| format!("requesting {kind} pad"))?;
        if let Some(cfg) = &pace {
            // HUB-36: meter the first pad only — video when the plan has
            // it (loop order), which is the stream whose production rate
            // placement cares about.
            let m = pace_meter.take();
            install_pace_probe(&pad, cfg.clone(), stopping.clone(), m);
        }
        if !multipart {
            per_part[0].lock().unwrap().insert(kind, pad);
            continue;
        }
        let concat = gst::ElementFactory::make("concat").build()?;
        pipeline.add(&concat)?;
        let src = concat.static_pad("src").context("concat has no src pad")?;
        // Same reason as every other pad feeding this sink: hlssink3
        // unwraps each fragment's first PTS and a panic in an FFI
        // callback takes the process with it.
        guard_pts(&src);
        src.link(&pad).context("linking concat to the muxer")?;
        // Request order IS play order — one pad per part, in sequence.
        for slot in per_part.iter() {
            let sink = concat
                .request_pad_simple("sink_%u")
                .context("concat sink pad")?;
            slot.lock().unwrap().insert(kind, sink);
        }
    }

    // Every parsed stream gets a queue immediately (no buffer ever hits an
    // unlinked pad); routing to the pre-requested muxer pads happens per
    // stream once its real caps flow (see plumb_parsed_pad).
    let gate = (start_ms > 0)
        .then(|| SeekGate::new(plan.has_video() as usize + plan.has_audio() as usize));
    let subs_dir = out_dir.to_path_buf();
    for (n, pb) in parsebins.iter().enumerate() {
        let pipe = pipeline.clone();
        let waiting2 = per_part[n].clone();
        // Only the first part is seeked, so only its branches are gated.
        // Gating the others would break the gate twice over: it expects
        // one branch per stream and would see one per stream PER PART,
        // and `start.pos` is the minimum stream time across gated pads —
        // a later part's branch starts at its own zero and would report
        // the whole session as starting at 0, shifting the client's
        // timeline by the entire resume offset.
        let gate2 = if n == 0 { gate.clone() } else { None };
        // Per part, NOT shared: these count tracks in demux order and the
        // count is what `plan.audio_track` / `plan.video_track` select
        // against. Track indices are a property of a file, so sharing
        // them across parts would offset part two's tracks past the
        // selection and play it silent.
        let audio_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let video_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let subs_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let subs_dir = subs_dir.clone();
        let burn_tl = burn_timeline.clone();
        let ass_link = ass_link.clone();
        pb.connect_pad_added(move |_, pad| {
            plumb_parsed_pad(
                &pipe,
                &waiting2,
                pad,
                plan,
                &gate2,
                &audio_seen,
                &video_seen,
                &subs_seen,
                &subs_dir,
                &burn_tl,
                &ass_link,
                n == 0,
            );
        });
    }

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
        anyhow::ensure!(
            parsebin.send_event(seek),
            "demuxer refused the start-offset seek"
        );
        gate.open_reporting(out_dir.join("start.pos"));
    }
    if start_ms == 0 {
        // Consistent origin reporting: zero-offset runs have a known
        // origin, write it so players can always sum base + start.pos.
        let _ = std::fs::write(out_dir.join("start.pos"), "0");
    }
    pipeline.set_state(gst::State::Playing)?;
    Ok(RemuxJob {
        pipeline,
        error,
        finished,
        stopping,
    })
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
        self.stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    /// Pacing (§4.6): hold the pipeline while the viewer catches up.
    pub fn pause(&self) {
        let _ = self.pipeline.set_state(gst::State::Paused);
    }

    pub fn resume(&self) {
        let _ = self.pipeline.set_state(gst::State::Playing);
    }

    /// Media-time position of the output (absolute — reflects offset
    /// starts), for the pacing window.
    pub fn position_ms(&self) -> Option<u64> {
        self.pipeline
            .query_position::<gst::ClockTime>()
            .map(|p| p.mseconds())
    }
}

impl Drop for RemuxJob {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod multipart {
    //! A multi-part source plays as one stream (HUB-17 / §4.6).
    use super::*;

    fn part(dir: &std::path::Path, name: &str, pattern: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        crate::testutil::render(&format!(
            "videotestsrc num-buffers=125 pattern={pattern} ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! x264enc key-int-max=25 bframes=0 ! h264parse ! matroskamux name=m audiotestsrc num-buffers=215 ! audioconvert ! fdkaacenc ! m. m. ! filesink location=\"{}\"",
            path.display()
        ));
        path
    }

    /// Resuming inside part one still reports where playback actually
    /// began. `start.pos` is the minimum stream time across the GATED
    /// pads and the client adds it to the part base, so gating a later
    /// part — whose branch starts at its own zero — reported the session
    /// as starting at 0 and shifted the whole timeline by the resume
    /// offset. Only the part being seeked is gated.
    #[test]
    fn resuming_inside_the_first_part_reports_its_own_start() {
        crate::init().unwrap();
        if !crate::testutil::has_element("fdkaacenc") {
            eprintln!("no fdkaacenc; skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let parts = [
            part(dir.path(), "a1.mkv", "smpte"),
            part(dir.path(), "a2.mkv", "ball"),
        ];
        let out = dir.path().join("hls");
        std::fs::create_dir_all(&out).unwrap();
        let sources: Vec<Box<dyn RemuxSource>> = parts
            .iter()
            .map(|p| Box::new(FileSource::open(p).unwrap()) as Box<dyn RemuxSource>)
            .collect();
        let plan = RemuxPlan {
            video: StreamMode::Copy,
            audio: StreamMode::Copy,
            audio_track: 0,
            video_track: 0,
            ..Default::default()
        };
        // 2 s into a 5 s first part.
        let job = start_parts(&out, plan, sources, 2_000, None, None, None, None).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        while !job.finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(
            job.failed().is_none(),
            "pipeline failed: {:?}",
            job.failed()
        );
        let pos: u64 = std::fs::read_to_string(out.join("start.pos"))
            .expect("no start.pos written")
            .trim()
            .parse()
            .expect("start.pos is not a number");
        // Keyframe-snapped at or before the request, never zero — zero is
        // what the second part's branch reports for itself.
        assert!(
            pos > 500,
            "start.pos {pos} — a later part's zero won the minimum"
        );
        assert!(
            pos <= 2_000,
            "start.pos {pos} is past the requested resume point"
        );
    }

    /// Two 5 s parts, one playlist, no seam: the muxer never learns the
    /// source changed file, so there is nothing for a client to stitch.
    #[test]
    fn two_parts_render_as_one_continuous_playlist() {
        crate::init().unwrap();
        if !crate::testutil::has_element("fdkaacenc") {
            eprintln!("no fdkaacenc; skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let parts = [
            part(dir.path(), "cd1.mkv", "smpte"),
            part(dir.path(), "cd2.mkv", "ball"),
        ];
        let out = dir.path().join("hls");
        std::fs::create_dir_all(&out).unwrap();

        let sources: Vec<Box<dyn RemuxSource>> = parts
            .iter()
            .map(|p| Box::new(FileSource::open(p).unwrap()) as Box<dyn RemuxSource>)
            .collect();
        let plan = RemuxPlan {
            video: StreamMode::Copy,
            audio: StreamMode::Copy,
            audio_track: 0,
            video_track: 0,
            ..Default::default()
        };
        let job = start_parts(&out, plan, sources, 0, None, None, None, None).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        while !job.finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(
            job.failed().is_none(),
            "pipeline failed: {:?}",
            job.failed()
        );
        assert!(job.finished(), "pipeline never finished");

        let playlist =
            std::fs::read_to_string(out.join("master.m3u8")).expect("no playlist written");
        let total: f64 = playlist
            .lines()
            .filter_map(|l| l.strip_prefix("#EXTINF:"))
            .filter_map(|l| l.trim_end_matches(',').parse::<f64>().ok())
            .sum();
        assert!(
            total > 9.0,
            "playlist covers {total}s — the second part never played"
        );
        assert!(
            !playlist.contains("EXT-X-DISCONTINUITY"),
            "the timeline broke at the seam"
        );
        assert!(
            playlist.contains("EXT-X-ENDLIST"),
            "playlist never finalised"
        );
    }
}

#[cfg(test)]
mod concat_spike {
    //! SPIKE (not a requirement): can one pipeline span a multi-part
    //! source, so a CD1->CD2 boundary produces no event at all? Today the
    //! boundary is implemented as a seek — tear the pipeline down, delete
    //! the segments, restart in the next file — and the client stitches
    //! it back together on `ended`.
    //!
    //! Uses the real seekable appsrc, not filesrc: production feeds bytes
    //! from a lease, and whether a seek reaches back through concat to the
    //! right appsrc is the whole question.
    use super::*;

    fn fixture(dir: &std::path::Path, name: &str, pattern: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        crate::testutil::render(&format!(
            "videotestsrc num-buffers=125 pattern={pattern} ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! x264enc key-int-max=25 bframes=0 ! h264parse ! matroskamux ! filesink location=\"{}\"",
            path.display()
        ));
        path
    }

    /// Two parts, one concat, one sink. `link` receives concat's src pad.
    fn concat_pipeline(
        parts: &[std::path::PathBuf],
        tail: &[&str],
    ) -> (gst::Pipeline, gst::Element) {
        crate::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let concat = gst::ElementFactory::make("concat").build().unwrap();
        pipeline.add(&concat).unwrap();
        let mut prev = concat.clone();
        for name in tail {
            let el = gst::ElementFactory::make(name).build().unwrap();
            pipeline.add(&el).unwrap();
            prev.link(&el).unwrap();
            prev = el;
        }
        for part in parts {
            let src = seekable_appsrc(Box::new(FileSource::open(part).unwrap()));
            let parsebin = gst::ElementFactory::make("parsebin").build().unwrap();
            pipeline
                .add_many([src.upcast_ref::<gst::Element>(), &parsebin])
                .unwrap();
            src.link(&parsebin).unwrap();
            let concat = concat.clone();
            let pipe = pipeline.downgrade();
            parsebin.connect_pad_added(move |_, pad| {
                let Some(pipe) = pipe.upgrade() else { return };
                // The queue is not optional. Without it the A/V variant
                // deadlocks outright: a demuxer pushes every stream from
                // one thread, so a branch blocked on a concat that is
                // still draining the other part stalls its siblings too.
                let queue = gst::ElementFactory::make("queue")
                    .property("max-size-buffers", 0u32)
                    .property("max-size-bytes", 0u32)
                    .property("max-size-time", 0u64)
                    .build()
                    .unwrap();
                pipe.add(&queue).unwrap();
                queue.sync_state_with_parent().unwrap();
                pad.link(&queue.static_pad("sink").unwrap()).unwrap();
                let sink = concat.request_pad_simple("sink_%u").unwrap();
                queue.static_pad("src").unwrap().link(&sink).unwrap();
            });
        }
        (pipeline, prev)
    }

    fn run_to_eos(pipeline: &gst::Pipeline, secs: u64) -> Option<gst::Message> {
        pipeline.set_state(gst::State::Playing).unwrap();
        let msg = pipeline.bus().unwrap().timed_pop_filtered(
            gst::ClockTime::from_seconds(secs),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        pipeline.set_state(gst::State::Null).unwrap();
        msg
    }

    /// Whatever the tone-map segment pins must be linkable to EVERY
    /// encoder we might place behind it. It pinned NV12 alone, which no
    /// hardware encoder minded and openh264enc (sink template: I420,
    /// nothing else) could not accept — and since the chain is linked
    /// from a pad-added callback, the failed link panicked where a
    /// panic cannot unwind: SIGABRT, no session error, on every HDR
    /// title a software-encoder box was asked to tone-map.
    #[test]
    fn tonemap_output_suits_every_encoder_we_place() {
        crate::init().unwrap();
        let mut checked = 0;
        for name in [
            "openh264enc",
            "x264enc",
            "x265enc",
            "nvh264enc",
            "nvh265enc",
            "vah264enc",
            "vah265enc",
            "vtenc_h264_hw",
            "vtenc_h265_hw",
            "svtav1enc",
        ] {
            let Some(f) = gst::ElementFactory::find(name) else {
                continue;
            };
            let sink: Vec<gst::Caps> = f
                .static_pad_templates()
                .into_iter()
                .filter(|t| t.direction() == gst::PadDirection::Sink)
                .map(|t| t.caps())
                .collect();
            if sink.is_empty() {
                continue;
            }
            // What the segment WILL pin for this encoder — not merely
            // whether the two sets overlap. A list is not a preference
            // order: offering {NV12, I420} to an encoder that takes only
            // NV12 resolved to I420 and died with not-negotiated on the
            // J5005, which is how HDR transcoding broke there for a day
            // while every "does it overlap" check stayed green.
            let mut pinned = tonemap_out_caps(name);
            pinned
                .get_mut()
                .unwrap()
                .set_features(0, Some(gst::CapsFeatures::new_any()));
            assert!(
                sink.iter().any(|c| !c.intersect(&pinned).is_empty()),
                "{name} accepts none of what the tone-map segment would pin \
                 ({pinned:?}). Its sink caps: {sink:?}"
            );
            // And every format in the pin must suit it, since the one
            // negotiation picks is not ours to choose.
            let Some(list) = pinned
                .structure(0)
                .and_then(|st| st.get::<gst::List>("format").ok())
            else {
                panic!("{name}: pinned caps carry no format list");
            };
            for fmt in list.iter() {
                let fmt = fmt.get::<String>().unwrap();
                let mut one = gst::Caps::builder("video/x-raw")
                    .field("format", &fmt)
                    .build();
                one.get_mut()
                    .unwrap()
                    .set_features(0, Some(gst::CapsFeatures::new_any()));
                assert!(
                    sink.iter().any(|c| !c.intersect(&one).is_empty()),
                    "{name} would be offered {fmt}, which it does not accept"
                );
            }
            checked += 1;
        }
        assert!(checked > 0, "no encoders on this box to check against");
    }

    /// AR-13a: the tone-map probe has to END IN THE ENCODER it claims
    /// to feed. The old one ran `videotestsrc ! segment ! fakesink`
    /// with no encoder named, so the output pin fell back to the whole
    /// format list and a sink that accepts anything swallowed the
    /// result. It reported healthy on a box where every HDR session
    /// died at negotiation.
    ///
    /// This asserts the probe agrees with what the box can really do,
    /// for each encoder the box actually has: a target the segment can
    /// feed must verify, and `tonemap_available` must be exactly
    /// "some target verified" rather than a claim of its own.
    #[test]
    fn tonemap_is_verified_against_a_real_encoder() {
        crate::init().unwrap();
        let targets: Vec<&str> = [h264_encoder(), hevc_encoder(), av1_encoder()]
            .into_iter()
            .flatten()
            .collect();
        if targets.is_empty() {
            return; // no video encoder on this box; nothing to claim
        }
        let any = targets.iter().any(|e| tonemap_into(e));
        assert_eq!(
            tonemap_available(),
            any,
            "tonemap_available must mean 'some real target verified', nothing else"
        );
        // An element that is not an encoder at all cannot be a target,
        // and must not be reported as one — the fakesink-shaped hole.
        assert!(
            !tonemap_into("fakesink"),
            "a sink that accepts anything must not count as a verified target"
        );
        // Whatever the segment would pin for a verified target has to be
        // a format that target accepts; that pairing is the thing the
        // old probe could not see.
        for enc in &targets {
            if !tonemap_into(enc) {
                continue;
            }
            let pinned = tonemap_out_caps(enc);
            let list = pinned
                .structure(0)
                .and_then(|st| st.get::<gst::List>("format").ok())
                .expect("pinned caps carry a format list");
            assert!(!list.is_empty(), "{enc}: nothing pinned");
        }
    }

    /// The FIRST segment must be independently decodable, which is the
    /// property that actually reaches a viewer: a player that cannot
    /// decode segment 0 wedges at the start of the session no matter
    /// how healthy everything downstream is.
    ///
    /// HONEST LIMIT: this asserts the invariant but does NOT reproduce
    /// the failure that motivated it. That one needed a SEEKED start on
    /// a real source through vtenc — segment00000 carried slices with no
    /// SPS/PPS while the worker produced two minutes of good segments
    /// behind it. Here x264enc's first output is an IDR whatever the
    /// sink asks for, so this passes with the regression reintroduced
    /// (verified). It guards the invariant for the plain path; catching
    /// the seeked one needs a fixture on the real start_at path.
    #[test]
    fn the_first_segment_is_independently_decodable() {
        crate::init().unwrap();
        for el in ["hlssink3", "x264enc"] {
            if gst::ElementFactory::find(el).is_none() {
                eprintln!("no {el}; skipped");
                return;
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("hls");
        std::fs::create_dir_all(&out).unwrap();
        let (sink, _) = make_hls_sink(&out, Some("hlssink3")).unwrap();
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 240i32)
            .build()
            .unwrap();
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("framerate", gst::Fraction::new(24000, 1001))
                    .field("width", 320i32)
                    .field("height", 180i32)
                    .build(),
            )
            .build()
            .unwrap();
        // No keyframe pin at all: the sink's request is what must put an
        // IDR at the head of fragment one.
        let enc = gst::ElementFactory::make("x264enc").build().unwrap();
        let parse = gst::ElementFactory::make("h264parse")
            .property("config-interval", -1i32)
            .build()
            .unwrap();
        pipeline
            .add_many([&src, &caps, &enc, &parse, &sink])
            .unwrap();
        gst::Element::link_many([&src, &caps, &enc, &parse]).unwrap();
        let pad = sink.request_pad_simple("video").unwrap();
        parse.static_pad("src").unwrap().link(&pad).unwrap();
        let msg = run_to_eos(&pipeline, 60);
        assert!(
            matches!(msg.map(|m| m.type_()), Some(gst::MessageType::Eos)),
            "pipeline did not reach EOS"
        );

        let first = out.join("segment00000.ts");
        let bytes = std::fs::read(&first).expect("no first segment");
        // Walk the TS payload for H.264 start codes. Parameter sets and
        // an IDR must BOTH be present, or the segment cannot stand on
        // its own — which is all a player gets at session start.
        let mut kinds = std::collections::HashSet::new();
        for w in bytes.windows(4) {
            if w[0] == 0 && w[1] == 0 && w[2] == 1 {
                kinds.insert(w[3] & 0x1f);
            }
        }
        assert!(kinds.contains(&7), "segment00000 has no SPS: {kinds:?}");
        assert!(kinds.contains(&8), "segment00000 has no PPS: {kinds:?}");
        assert!(kinds.contains(&5), "segment00000 has no IDR: {kinds:?}");
    }

    /// One cut source, not two. The sink's keyframe request defines the
    /// fragment; the encode chain's GOP pin is a far-away backstop that
    /// must never fire first. Pinned NEAR the fragment interval the two
    /// cadences compete — they count different things, frames against
    /// seconds — and the sink cuts at both: 1.96 s and 1.04 s segments
    /// alternating, every short one costing a keyframe. Built through
    /// `make_hls_sink` so the production configuration is under test,
    /// and with production's backstop rather than a competing pin.
    #[test]
    fn segments_run_one_gop_each() {
        crate::init().unwrap();
        for el in ["hlssink3", "x264enc"] {
            if gst::ElementFactory::find(el).is_none() {
                eprintln!("no {el}; skipped");
                return;
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("hls");
        std::fs::create_dir_all(&out).unwrap();
        let (sink, _) = make_hls_sink(&out, Some("hlssink3")).unwrap();

        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 480i32) // 20 s at 23.976
            .build()
            .unwrap();
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("framerate", gst::Fraction::new(24000, 1001))
                    .field("width", 320i32)
                    .field("height", 180i32)
                    .build(),
            )
            .build()
            .unwrap();
        // The backstop production uses: far longer than the fragment
        // interval, so the sink's request is what actually cuts.
        let enc = gst::ElementFactory::make("x264enc")
            .property("key-int-max", 240u32)
            .build()
            .unwrap();
        let parse = gst::ElementFactory::make("h264parse").build().unwrap();
        pipeline
            .add_many([&src, &caps, &enc, &parse, &sink])
            .unwrap();
        gst::Element::link_many([&src, &caps, &enc, &parse]).unwrap();
        let pad = sink.request_pad_simple("video").unwrap();
        parse
            .static_pad("src")
            .unwrap()
            .link(&pad)
            .expect("linking to the sink");

        let msg = run_to_eos(&pipeline, 60);
        assert!(
            matches!(msg.map(|m| m.type_()), Some(gst::MessageType::Eos)),
            "pipeline did not reach EOS"
        );

        let pl = std::fs::read_to_string(out.join("master.m3u8")).unwrap();
        let durs: Vec<f64> = pl
            .lines()
            .filter_map(|l| l.strip_prefix("#EXTINF:"))
            .filter_map(|l| l.trim_end_matches(',').parse().ok())
            .collect();
        assert!(durs.len() >= 4, "too few segments to judge:\n{pl}");
        // The last segment is whatever content remained at EOS; every
        // other one is a whole GOP. A short segment among them is the
        // sink cutting on its own schedule again.
        let (body, _) = durs.split_at(durs.len() - 1);
        let shortest = body.iter().cloned().fold(f64::INFINITY, f64::min);
        let longest = body.iter().cloned().fold(0.0, f64::max);
        assert!(
            longest - shortest < 0.2,
            "segments are not one GOP each (min {shortest:.3}, max {longest:.3}): {body:?}"
        );
    }

    /// HALF ONE: does concat, fed by the production appsrc, produce a
    /// single continuous HLS playlist across a part boundary?
    #[test]
    fn concat_over_appsrc_yields_one_continuous_playlist() {
        crate::init().unwrap();
        if gst::ElementFactory::find("hlssink3").is_none() {
            eprintln!("no hlssink3; skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let parts = [
            fixture(dir.path(), "p1.mkv", "smpte"),
            fixture(dir.path(), "p2.mkv", "ball"),
        ];
        let out = dir.path().join("hls");
        std::fs::create_dir_all(&out).unwrap();

        let (pipeline, tail) = concat_pipeline(&parts, &["h264parse"]);
        let sink = gst::ElementFactory::make("hlssink3")
            .property("target-duration", 2u32)
            .property("playlist-length", 0u32)
            .property("max-files", 0u32)
            .property("location", out.join("seg%05d.ts").to_str().unwrap())
            .property("playlist-location", out.join("play.m3u8").to_str().unwrap())
            .build()
            .unwrap();
        pipeline.add(&sink).unwrap();
        // hlssink3 muxes internally: elementary streams on request pads.
        let pad = sink.request_pad_simple("video").unwrap();
        // imp.rs:304 unwraps each fragment's first PTS and a panic in an
        // FFI callback kills the process, so guard here as production does.
        guard_pts(&tail.static_pad("src").unwrap());
        tail.static_pad("src").unwrap().link(&pad).unwrap();

        let msg = run_to_eos(&pipeline, 60);
        assert!(
            matches!(
                msg.as_ref().map(|m| m.view()),
                Some(gst::MessageView::Eos(_))
            ),
            "pipeline did not reach EOS: {msg:?}"
        );

        let playlist = std::fs::read_to_string(out.join("play.m3u8")).unwrap();
        let total: f64 = playlist
            .lines()
            .filter_map(|l| l.strip_prefix("#EXTINF:"))
            .filter_map(|l| l.trim_end_matches(',').parse::<f64>().ok())
            .sum();
        // Two 5 s parts arriving as one stream, and NO discontinuity tag:
        // the muxer never learns there was a boundary.
        assert!(
            total > 9.0,
            "playlist covers only {total}s — the second part is missing"
        );
        assert!(
            !playlist.contains("EXT-X-DISCONTINUITY"),
            "timeline broke at the seam"
        );
        assert!(
            playlist.contains("EXT-X-ENDLIST"),
            "playlist never finalised"
        );
    }

    /// HALF TWO: can the concatenated timeline be SEEKED, or must a seek
    /// keep restarting the pipeline in the target part as it does today?
    /// `concat`'s documentation says nothing about seeking, so this is
    /// the deciding measurement for whether one pipeline can serve a
    /// whole multi-part film.
    #[test]
    fn seeking_across_the_concat_boundary() {
        crate::init().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let parts = [
            fixture(dir.path(), "s1.mkv", "smpte"),
            fixture(dir.path(), "s2.mkv", "ball"),
        ];
        let (pipeline, tail) = concat_pipeline(&parts, &["h264parse", "fakesink"]);
        tail.set_property("sync", false);

        // First buffer PTS after the seek: where playback actually resumed.
        let seen: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let seen2 = seen.clone();
        tail.static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(gst::PadProbeData::Buffer(b)) = &info.data {
                    let mut s = seen2.lock().unwrap();
                    if s.is_none() {
                        *s = b.pts().map(|p| p.mseconds());
                    }
                }
                gst::PadProbeReturn::Ok
            });

        // (a) seek from PAUSED, after preroll.
        pipeline.set_state(gst::State::Paused).unwrap();
        let _ = pipeline.state(gst::ClockTime::from_seconds(30));
        // 7 s lands inside part two (two 5 s parts).
        let paused_ok = pipeline.seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            gst::ClockTime::from_mseconds(7_000),
        );
        let msg = run_to_eos(&pipeline, 60);
        let from_paused = *seen.lock().unwrap();
        eprintln!(
            "SPIKE paused-seek accepted={paused_ok:?} first_pts={from_paused:?}ms eos={}",
            matches!(
                msg.as_ref().map(|m| m.view()),
                Some(gst::MessageView::Eos(_))
            )
        );
        // Characterisation, deliberately pinned to today's behaviour: the
        // seek is ACCEPTED and then ignored — playback resumes at zero,
        // not at 7 s. Anything built on concat seeks would look correct in
        // a paused test and silently restart the film in a real player.
        assert!(paused_ok.is_ok(), "a paused seek used to be accepted");
        assert_eq!(
            from_paused,
            Some(0),
            "concat now honours seeks — revisit the design"
        );

        // (b) the realistic case: scrub while playing.
        let (pipeline, tail) = concat_pipeline(&parts, &["h264parse", "fakesink"]);
        tail.set_property("sync", false);
        let live: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let live2 = live.clone();
        let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let armed2 = armed.clone();
        tail.static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(gst::PadProbeData::Buffer(b)) = &info.data
                    && armed2.load(std::sync::atomic::Ordering::SeqCst)
                {
                    let mut s = live2.lock().unwrap();
                    if s.is_none() {
                        *s = b.pts().map(|p| p.mseconds());
                    }
                }
                gst::PadProbeReturn::Ok
            });
        pipeline.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        armed.store(true, std::sync::atomic::Ordering::SeqCst);
        let live_ok = pipeline.seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            gst::ClockTime::from_mseconds(7_000),
        );
        let msg2 = run_to_eos(&pipeline, 60);
        eprintln!(
            "SPIKE live-seek accepted={live_ok:?} first_pts={:?}ms eos={}",
            *live.lock().unwrap(),
            matches!(
                msg2.as_ref().map(|m| m.view()),
                Some(gst::MessageView::Eos(_))
            )
        );
        // Seeking while PLAYING is refused outright. If this ever starts
        // succeeding, one pipeline could serve seeks too and the
        // restart-per-part path could go.
        assert!(
            live_ok.is_err(),
            "concat now accepts a live seek — revisit the design"
        );
        // Recorded, not asserted: this test exists to MEASURE, and the
        // answer decides the design. Whatever it prints is the finding.
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn selects_requested_video_track() {
        crate::init().unwrap();
        // Two H.264 tracks distinguishable by resolution.
        let dir = tempfile::tempdir().unwrap();
        let mkv = dir.path().join("two-video.mkv");
        let launch = format!(
            "videotestsrc num-buffers=60 ! video/x-raw,width=64,height=48 ! x264enc ! h264parse ! mux.              videotestsrc num-buffers=60 pattern=ball ! video/x-raw,width=128,height=96 ! x264enc ! h264parse ! mux.              matroskamux name=mux ! filesink location={}",
            mkv.display()
        );
        let pipe = gst::parse::launch(&launch).unwrap();
        pipe.set_state(gst::State::Playing).unwrap();
        let msg = pipe.bus().unwrap().timed_pop_filtered(
            gst::ClockTime::from_seconds(30),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        pipe.set_state(gst::State::Null).unwrap();
        assert_eq!(
            msg.map(|m| m.type_()),
            Some(gst::MessageType::Eos),
            "fixture build failed"
        );

        for (track, want_width) in [(0usize, 64u32), (1, 128)] {
            let info = crate::discover(&mkv, std::time::Duration::from_secs(10)).unwrap();
            assert_eq!(info.video.len(), 2, "{info:?}");
            let plan = plan_streams(&info, &WEB_TARGET, 0, track);
            let out = tempfile::tempdir().unwrap();
            let job = start(out.path(), plan, Box::new(FileSource::open(&mkv).unwrap())).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !job.finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            assert!(job.failed().is_none(), "{:?}", job.failed());
            let seg = std::fs::read_dir(out.path())
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .find(|p| p.extension().is_some_and(|x| x == "ts"))
                .expect("no segment produced");
            let seg_info = crate::discover(&seg, std::time::Duration::from_secs(10)).unwrap();
            assert_eq!(seg_info.video.len(), 1, "track {track}: {seg_info:?}");
            assert_eq!(
                seg_info.video[0].width, want_width,
                "track {track} selected the wrong video: {seg_info:?}"
            );
        }
    }

    #[test]
    fn selects_requested_audio_track() {
        crate::init().unwrap();
        let Some(aac) = aac_encoder() else {
            eprintln!("skipping: no AAC encoder installed");
            return;
        };
        // Two AAC tracks distinguishable by channel count: 0 = stereo,
        // 1 = mono. Selecting track 1 must put MONO audio in the output.
        let dir = tempfile::tempdir().unwrap();
        let mkv = dir.path().join("two-audio.mkv");
        let launch = format!(
            "videotestsrc num-buffers=60 ! video/x-raw,width=64,height=48 ! x264enc ! h264parse ! mux.              audiotestsrc num-buffers=90 ! audio/x-raw,channels=2 ! audioconvert ! {aac} ! aacparse ! mux.              audiotestsrc num-buffers=90 freq=880 ! audio/x-raw,channels=1 ! audioconvert ! {aac} ! aacparse ! mux.              matroskamux name=mux ! filesink location={}",
            mkv.display()
        );
        let pipe = gst::parse::launch(&launch).unwrap();
        pipe.set_state(gst::State::Playing).unwrap();
        let msg = pipe.bus().unwrap().timed_pop_filtered(
            gst::ClockTime::from_seconds(30),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        pipe.set_state(gst::State::Null).unwrap();
        assert_eq!(
            msg.map(|m| m.type_()),
            Some(gst::MessageType::Eos),
            "fixture build failed"
        );

        for (track, want_channels) in [(0u32, 2u32), (1, 1)] {
            let info = crate::discover(&mkv, std::time::Duration::from_secs(10)).unwrap();
            assert_eq!(info.audio.len(), 2, "{info:?}");
            let plan = plan_streams(&info, &WEB_TARGET, track as usize, 0);
            let out = tempfile::tempdir().unwrap();
            let job = start(out.path(), plan, Box::new(FileSource::open(&mkv).unwrap())).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !job.finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            assert!(job.failed().is_none(), "{:?}", job.failed());
            let seg = std::fs::read_dir(out.path())
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .find(|p| p.extension().is_some_and(|x| x == "ts"))
                .expect("no segment produced");
            let seg_info = crate::discover(&seg, std::time::Duration::from_secs(10)).unwrap();
            assert_eq!(seg_info.audio.len(), 1, "track {track}: {seg_info:?}");
            assert_eq!(
                seg_info.audio[0].channels, want_channels,
                "track {track} selected the wrong audio: {seg_info:?}"
            );
        }
    }

    use super::*;
    use std::time::{Duration, Instant};

    const COPY_AV: RemuxPlan = RemuxPlan {
        video: StreamMode::Copy,
        audio: StreamMode::Copy,
        audio_track: 0,
        video_track: 0,
        video_kbps: None,
        max_height: None,
        max_channels: None,
        tone_map: false,
        burn_ass: None,
        burn_subtitle: None,
        video_codec: VideoTarget::H264,
        audio_codec: AudioTarget::Aac,
        segment_format: SegmentFormat::Ts,
    };

    /// Manual repro: REMUX_SRC=/path/to/file cargo test -p kahawai-media \
    ///   remux_file_from_env -- --ignored --nocapture
    #[test]
    #[ignore]
    fn remux_file_from_env() {
        let src = std::path::PathBuf::from(std::env::var("REMUX_SRC").expect("set REMUX_SRC"));
        let out = tempfile::tempdir().unwrap();
        let info = crate::discover(&src, Duration::from_secs(30)).unwrap();
        let plan = plan_streams(&info, &WEB_TARGET, 0, 0);
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
        eprintln!(
            "OK: {} entries in dir, ENDLIST={}",
            segs,
            playlist.contains("#EXT-X-ENDLIST")
        );
    }

    /// mp3 and AAC share the caps NAME `audio/mpeg`, so a name-level
    /// muxability check called mp3 fMP4-muxable; the copy it planned
    /// then failed to link against isofmp4mux (mpegversion 4 only) and
    /// took the pipeline down with `not-linked`. Both the codec-label
    /// side (negotiation) and the live-caps side (the worker) must say
    /// no, and both must still say yes for TS.
    #[test]
    fn mpeg_audio_muxability_is_decided_by_version_not_name() {
        crate::init().unwrap();
        for label in ["mp3", "mpeg-audio"] {
            let c = codec_to_caps("audio", label).unwrap();
            assert!(
                caps_muxable(&c, SegmentFormat::Ts),
                "{label} is MPEG-1 audio, which mpegtsmux carries"
            );
            assert!(
                !caps_muxable(&c, SegmentFormat::Fmp4),
                "{label} must not plan as an fMP4 copy"
            );
        }
        // AAC is the same caps name and must stay muxable in both.
        let aac = codec_to_caps("audio", "aac").unwrap();
        assert!(caps_muxable(&aac, SegmentFormat::Ts));
        assert!(caps_muxable(&aac, SegmentFormat::Fmp4));

        // The worker sees full caps off the demuxer, not a label.
        let live_mp3 = gst::Caps::builder("audio/mpeg")
            .field("mpegversion", 1i32)
            .field("layer", 3i32)
            .field("rate", 48000i32)
            .field("channels", 2i32)
            .build();
        assert_eq!(sink_compatible(&live_mp3, SegmentFormat::Ts), Some("audio"));
        assert_eq!(sink_compatible(&live_mp3, SegmentFormat::Fmp4), None);

        // The check must NOT tighten on fields a parser converts:
        // h264 arrives byte-stream/nal from AVI and leaves h264parse as
        // avc/au, which is what isofmp4mux's template demands.
        let live_h264 = gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "nal")
            .build();
        assert_eq!(
            sink_compatible(&live_h264, SegmentFormat::Fmp4),
            Some("video")
        );
        assert_eq!(sink_compatible(&live_h264, SegmentFormat::Ts), Some("video"));
    }

    #[test]
    fn ts_compat_follows_muxer_templates() {
        crate::init().unwrap();
        let caps = |n: &str| gst::Caps::builder(n).build();
        // Basics that any mpegtsmux supports.
        assert_eq!(
            sink_compatible(&caps("video/x-h264"), SegmentFormat::Ts),
            Some("video")
        );
        assert_eq!(
            sink_compatible(&caps("audio/mpeg"), SegmentFormat::Ts),
            Some("audio")
        );
        assert_eq!(sink_compatible(&caps("text/x-raw"), SegmentFormat::Ts), None);
        // Every answer must agree with the muxer's own template.
        let names = muxable_names(SegmentFormat::Ts);
        assert_eq!(
            sink_compatible(&caps("audio/x-eac3"), SegmentFormat::Ts).is_some(),
            names.contains("audio/x-eac3")
        );
        assert_eq!(
            sink_compatible(&caps("audio/x-dts"), SegmentFormat::Ts).is_some(),
            names.contains("audio/x-dts")
        );

        // Flags derive from the same truth: eac3-only audio yields
        // has_audio only if the muxer takes eac3 (it does not, today).
        let info = kahawai_core::media::MediaInfo {
            video: vec![kahawai_core::media::VideoStream {
                codec: "hevc".into(),
                ..Default::default()
            }],
            audio: vec![kahawai_core::media::AudioStream {
                codec: "eac3".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let plan = plan_streams(&info, &WEB_TARGET, 0, 0);
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
        p.bus()
            .unwrap()
            .timed_pop_filtered(gst::ClockTime::from_seconds(30), &[gst::MessageType::Eos])
            .unwrap();
        p.set_state(gst::State::Null).unwrap();

        let out = tempfile::tempdir().unwrap();
        let job = start(
            out.path(),
            COPY_AV,
            Box::new(FileSource::open(&src_path).unwrap()),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            job.finished(),
            "uneven-track remux deadlocked (queue-sizing regression)"
        );
        assert!(job.failed().is_none(), "remux failed: {:?}", job.failed());
        assert!(
            std::fs::read_to_string(out.path().join("master.m3u8"))
                .unwrap()
                .contains("#EXT-X-ENDLIST")
        );
    }

    /// Streams `parser_for` does NOT cover must still be parsed BY
    /// PARSEBIN, or stopping its autoplug breaks them.
    ///
    /// `autoplug-continue` answers with `parser_for`, so every codec it
    /// returns `None` for — flac, vorbis, vp8, theora, divx — keeps the
    /// old behaviour and gets parsebin's parser. Getting that backwards
    /// is silent: the muxer simply never negotiates and the session
    /// produces no playlist at all, with no error anywhere (which is
    /// exactly how the AV1 freeze presented).
    ///
    /// FLAC is the case in this library — it is what the anime rips use.
    #[test]
    fn a_codec_we_have_no_parser_for_still_remuxes() {
        crate::init().unwrap();
        // The premise: if this ever gains a parser, the test below stops
        // covering what it claims to.
        for name in ["audio/x-flac", "audio/x-vorbis", "video/x-vp8"] {
            let caps = gst::Caps::builder(name).build();
            assert!(
                parser_for(&caps).is_none(),
                "{name} now has a parser — pick another uncovered codec"
            );
        }

        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("flac.mkv");
        crate::testutil::render_h264_flac_mkv(&src_path);

        // fMP4, because MPEG-TS cannot carry FLAC at all — a TS copy of
        // it is a plan negotiation would never make, and asserting on one
        // tests the muxer's limits rather than the autoplug decision.
        let plan = RemuxPlan {
            segment_format: SegmentFormat::Fmp4,
            ..COPY_AV
        };
        let out = tempfile::tempdir().unwrap();
        let job = start(
            out.path(),
            plan,
            Box::new(FileSource::open(&src_path).unwrap()),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            job.finished(),
            "remux of an unparsed-by-us codec never finished — parsebin \
             was stopped from parsing something we do not parse either"
        );
        assert!(job.failed().is_none(), "remux failed: {:?}", job.failed());
        let playlist = std::fs::read_to_string(out.path().join("master.m3u8")).unwrap();
        assert!(playlist.contains("#EXTINF"), "no segments: {playlist}");
    }

    /// parsebin keeps its parser for everything except AV1.
    ///
    /// The general form of this rule — block wherever `parser_for` has
    /// an answer, one parser per stream — was tried and reverted: for
    /// h264 out of MP4 it makes mpegtsmux emit a bare 9-byte PPS as its
    /// own timestamp-less PES once per keyframe, which the sweep flags
    /// as `[bad dts] 1 missing` (6 of 60 files). The parser's output
    /// bytes are identical either way; the second parse re-segments
    /// them, and that is what keeps the muxer's PES boundaries honest.
    ///
    /// This guards the DECISION, not the defect. A synthetic fixture
    /// does not reproduce it — measured: `qtdemux ! h264parse !
    /// mpegtsmux` over a rendered mp4 gives 0 timestamp-less packets,
    /// over a real one 106 — so the end-to-end property belongs to the
    /// corpus sweep, which is what caught it.
    #[test]
    fn only_av1_is_kept_from_parsebins_parser() {
        crate::init().unwrap();
        let caps = |n: &str| gst::Caps::builder(n).build();
        assert!(parsebin_must_not_parse(&caps("video/x-av1")));
        for name in [
            "video/x-h264",
            "video/x-h265",
            "video/x-vp9",
            "audio/mpeg",
            "audio/x-ac3",
            // Containers: parsebin must always be free to demux.
            "video/quicktime",
            "video/x-matroska",
        ] {
            assert!(
                !parsebin_must_not_parse(&caps(name)),
                "{name} would lose parsebin's parser; only AV1 may"
            );
            // The codecs above still get a parser from us afterwards —
            // that is the double parse the h264 path depends on.
        }
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
        let job = start(
            out.path(),
            COPY_AV,
            Box::new(FileSource::open(&src_path).unwrap()),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            job.finished(),
            "moov-at-end mp4 remux did not finish (push-mode regression)"
        );
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
        if muxable_names(SegmentFormat::Ts).contains("audio/x-eac3") {
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
        let plan = plan_streams(&info, &WEB_TARGET, 0, 0);
        assert_eq!(
            plan.audio,
            StreamMode::Encode,
            "eac3 should plan as Encode: {info:?}"
        );
        assert_eq!(plan.video, StreamMode::Copy);

        let out = tempfile::tempdir().unwrap();
        let job = start(
            out.path(),
            plan,
            Box::new(FileSource::open(&src_path).unwrap()),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "transcode did not finish");
        assert!(
            job.failed().is_none(),
            "transcode failed: {:?}",
            job.failed()
        );
        assert!(
            std::fs::read_to_string(out.path().join("master.m3u8"))
                .unwrap()
                .contains("#EXT-X-ENDLIST")
        );

        // The produced segment must carry AAC audio and h264 video.
        let seg =
            crate::discover(&out.path().join("segment00000.ts"), Duration::from_secs(30)).unwrap();
        assert_eq!(
            seg.video.len(),
            1,
            "video missing from transcoded segment: {seg:?}"
        );
        assert_eq!(seg.video[0].codec, "h264");
        assert_eq!(
            seg.audio.len(),
            1,
            "audio missing from transcoded segment: {seg:?}"
        );
        assert_eq!(
            seg.audio[0].codec, "aac",
            "audio not transcoded to AAC: {seg:?}"
        );
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
        let plan = plan_streams(&info, &WEB_TARGET, 0, 0);
        assert_eq!(
            plan.video,
            StreamMode::Encode,
            "mpeg4 should plan as Encode: {info:?}"
        );
        assert_eq!(plan.audio, StreamMode::Copy, "aac should copy: {info:?}");

        let out = tempfile::tempdir().unwrap();
        let job = start(
            out.path(),
            plan,
            Box::new(FileSource::open(&src_path).unwrap()),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "video transcode did not finish");
        assert!(
            job.failed().is_none(),
            "video transcode failed: {:?}",
            job.failed()
        );
        let seg =
            crate::discover(&out.path().join("segment00000.ts"), Duration::from_secs(30)).unwrap();
        assert_eq!(
            seg.video.first().map(|v| v.codec.as_str()),
            Some("h264"),
            "{seg:?}"
        );
        assert_eq!(
            seg.audio.first().map(|a| a.codec.as_str()),
            Some("aac"),
            "{seg:?}"
        );
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
        assert!(
            job.failed().is_none(),
            "offset remux failed: {:?}",
            job.failed()
        );
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
        let plan = plan_streams(&info, &WEB_TARGET, 0, 0);
        assert_eq!(
            plan.audio,
            StreamMode::Encode,
            "flac should plan Encode: {info:?}"
        );

        let out = tempfile::tempdir().unwrap();
        // The flac fixture is ~5 s; start at 2.5 s → expect a ~2.5 s tail.
        let job = start_at(
            out.path(),
            plan,
            Box::new(FileSource::open(&src_path).unwrap()),
            2_500,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "offset encode remux did not finish");
        assert!(
            job.failed().is_none(),
            "offset encode remux failed: {:?}",
            job.failed()
        );
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

    /// HUB-15 encode parameters reach the pipeline: the produced
    /// segments obey the resolution ceiling and the channel downmix.
    #[test]
    fn encode_honors_scale_and_downmix() {
        crate::init().unwrap();
        if h264_encoder().is_none() || aac_encoder().is_none() {
            eprintln!("skipping: encoders unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("in.mkv");
        crate::testutil::render_h264_flac_mkv(&src_path); // 320x240, flac

        let info = crate::discover(&src_path, Duration::from_secs(30)).unwrap();
        let plan = RemuxPlan {
            video: StreamMode::Encode,
            audio: StreamMode::Encode,
            audio_track: 0,
            video_track: 0,
            video_kbps: Some(500),
            max_height: Some(120),
            max_channels: Some(1),
            tone_map: false,
            burn_subtitle: None,
            ..Default::default()
        };
        let _ = info;
        let out = tempfile::tempdir().unwrap();
        let job = start_at(
            out.path(),
            plan,
            Box::new(FileSource::open(&src_path).unwrap()),
            0,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "param encode did not finish");
        assert!(
            job.failed().is_none(),
            "param encode failed: {:?}",
            job.failed()
        );

        // Probe the first produced segment: the ceiling and downmix are
        // facts about the OUTPUT, not the plan.
        let seg = std::fs::read_dir(out.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().is_some_and(|x| x == "ts"))
            .expect("no segment produced");
        let seg_info = crate::discover(&seg, Duration::from_secs(30)).unwrap();
        assert!(
            seg_info.video[0].height <= 120,
            "height {} exceeds the ceiling",
            seg_info.video[0].height
        );
        assert_eq!(
            seg_info.audio[0].channels, 1,
            "downmix to mono did not happen"
        );
    }

    /// The tone-map peak probe must survive every buffer layout a
    /// decoder can hand it. It runs in a pad probe called from C, so a
    /// panic there aborts the WORKER — reported from the field as a
    /// SIGABRT at session start on an HDR title, which disappeared when
    /// the client forced HDR (no tone-map, no probe). The original code
    /// took `stride` from the CAPS while reading the mapped FRAME, so a
    /// padded buffer walked off the plane.
    #[test]
    fn peak_probe_survives_any_plane_layout() {
        let (w, h) = (1920usize, 1038); // the reported geometry
        let mut out = Vec::new();

        // Tightly packed 10-bit: stride == w*2, data exactly h rows.
        let tight = vec![0x80u8; w * 2 * h];
        sample_luma(&tight, w * 2, w, h, true, &mut out);
        assert!(!out.is_empty(), "a well-formed plane must sample");

        // PADDED: the decoder's real stride exceeds w*2 (libav pads to
        // its own alignment). Reading it with the caps stride is the
        // bug; reading it with the frame's must simply work.
        let padded_stride = w * 2 + 128;
        let padded = vec![0x80u8; padded_stride * h];
        out.clear();
        sample_luma(&padded, padded_stride, w, h, true, &mut out);
        assert!(!out.is_empty(), "padded plane must sample");

        // The crash shape: a plane sized for a TIGHT layout, read with
        // a PADDED stride. Every byte beyond the end must be declined,
        // not indexed.
        out.clear();
        sample_luma(&tight, padded_stride, w, h, true, &mut out);

        // And the reverse, plus degenerate geometry — none may panic.
        out.clear();
        sample_luma(&padded, w * 2, w, h, true, &mut out);
        out.clear();
        sample_luma(&[], w * 2, w, h, true, &mut out);
        assert!(out.is_empty());
        sample_luma(&tight, 0, w, h, true, &mut out); // stride < w*bpp
        assert!(out.is_empty(), "nonsense geometry samples nothing");
        sample_luma(&tight, w * 2, 0, 0, true, &mut out);
        assert!(out.is_empty());
        // 8-bit path, short final row (tight packing after the last row).
        let short = vec![0x40u8; w * (h - 1) + 3];
        out.clear();
        sample_luma(&short, w, w, h, false, &mut out);
        assert!(!out.is_empty());
    }

    /// HUB-36: the sample-validity rule. Short samples are preroll
    /// burst, not throughput, and must not reach the pace table.
    #[test]
    fn pace_multiple_discards_short_samples() {
        // 30 s of content in 10 s of wall = 3x realtime.
        assert_eq!(pace_multiple(30_000, 10_000), Some(3.0));
        // A box at exactly realtime.
        assert_eq!(pace_multiple(20_000, 20_000), Some(1.0));
        // Slower than realtime is a legitimate, important measurement.
        assert_eq!(pace_multiple(6_500, 10_000), Some(0.65));
        // A fast box: the whole window in ~1 s of wall. This MUST be
        // measurable — it is the case placement most wants to know.
        assert_eq!(pace_multiple(120_000, 6_000), Some(20.0));
        assert_eq!(pace_multiple(20_000, 1_000), Some(20.0));
        // Sub-second: timer jitter, not a measurement.
        assert_eq!(pace_multiple(30_000, 999), None);
        // Too little content: a stalled start says nothing either.
        assert_eq!(pace_multiple(4_999, 30_000), None);
        assert_eq!(pace_multiple(0, 0), None);
    }

    /// The write path: a finalized meter leaves a parseable pace.json
    /// where the harvesters look for it.
    #[test]
    fn pace_meter_writes_its_sample() {
        let out = tempfile::tempdir().unwrap();
        let mut m = PaceMeter {
            t0: Some(Instant::now() - Duration::from_secs(4)),
            first_ms: 1_000,
            done: false,
        };
        // 4 s of wall, 13 s of content produced since first_ms.
        m.finish(14_000, out.path());
        assert!(m.done, "a finished meter never measures twice");
        let body = std::fs::read_to_string(out.path().join("pace.json")).unwrap();
        let v: f64 = body
            .trim()
            .trim_start_matches("{\"multiple\":")
            .trim_end_matches('}')
            .parse()
            .unwrap_or_else(|e| panic!("unparseable {body:?}: {e}"));
        assert!((v - 3.25).abs() < 0.1, "expected ~3.25x, got {v} ({body})");

        // A too-short sample writes nothing rather than lying.
        let out2 = tempfile::tempdir().unwrap();
        let mut short = PaceMeter {
            t0: Some(Instant::now() - Duration::from_millis(200)),
            first_ms: 0,
            done: false,
        };
        short.finish(1_000, out2.path());
        assert!(!out2.path().join("pace.json").exists());
    }

    /// HUB-15b fMP4 path end to end: encode h264/aac into fragmented
    /// MP4 — init.mp4 + .m4s segments + an EVENT playlist whose EXTINF
    /// lines the readiness gates can read, ENDLIST at EOS, and the
    /// init+segment concatenation must decode.
    #[test]
    fn fmp4_sink_produces_init_segments_and_playlist() {
        crate::init().unwrap();
        if !crate::testutil::has_element("isofmp4mux") || h264_encoder().is_none() {
            return; // doctor territory, not this test's
        }
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("in.mkv");
        crate::testutil::render_h264_flac_mkv(&src_path); // 5 s, 320x240

        let plan = RemuxPlan {
            video: StreamMode::Encode,
            audio: StreamMode::Encode,
            video_kbps: Some(500),
            segment_format: SegmentFormat::Fmp4,
            ..Default::default()
        };
        let out = tempfile::tempdir().unwrap();
        let job = start_at(
            out.path(),
            plan,
            Box::new(FileSource::open(&src_path).unwrap()),
            0,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "fmp4 encode did not finish");
        assert!(
            job.failed().is_none(),
            "fmp4 encode failed: {:?}",
            job.failed()
        );

        let init = out.path().join("init.mp4");
        assert!(init.exists(), "no init.mp4");
        let mut segs: Vec<_> = std::fs::read_dir(out.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "m4s"))
            .collect();
        segs.sort();
        assert!(!segs.is_empty(), "no .m4s segments");

        let playlist = std::fs::read_to_string(out.path().join("master.m3u8")).unwrap();
        assert!(
            playlist.contains("#EXT-X-MAP:URI=\"init.mp4\""),
            "{playlist}"
        );
        assert!(playlist.contains("#EXT-X-ENDLIST"), "{playlist}");
        let extinf_sum: f64 = playlist
            .lines()
            .filter_map(|l| l.strip_prefix("#EXTINF:"))
            .filter_map(|l| l.trim_end_matches(',').parse::<f64>().ok())
            .sum();
        assert!(
            (4.0..7.0).contains(&extinf_sum),
            "EXTINF sum {extinf_sum} for a ~5 s source: {playlist}"
        );

        // The stream itself: init + all segments = a decodable fMP4.
        let joined = out.path().join("joined.mp4");
        let mut bytes = std::fs::read(&init).unwrap();
        for s in &segs {
            bytes.extend(std::fs::read(s).unwrap());
        }
        std::fs::write(&joined, bytes).unwrap();
        let info = crate::discover(&joined, Duration::from_secs(30)).unwrap();
        assert_eq!(info.video[0].codec, "h264", "{info:?}");
        assert_eq!(info.audio[0].codec, "aac", "{info:?}");
    }

    /// The same path with an offset start: the seek gate and start.pos
    /// machinery must work through a virgin isofmp4mux.
    #[test]
    fn fmp4_sink_offset_start() {
        crate::init().unwrap();
        if !crate::testutil::has_element("isofmp4mux") || h264_encoder().is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("in.mkv");
        crate::testutil::render_h264_flac_mkv(&src_path);
        let plan = RemuxPlan {
            video: StreamMode::Encode,
            audio: StreamMode::Encode,
            video_kbps: Some(500),
            segment_format: SegmentFormat::Fmp4,
            ..Default::default()
        };
        let out = tempfile::tempdir().unwrap();
        let job = start_at(
            out.path(),
            plan,
            Box::new(FileSource::open(&src_path).unwrap()),
            2000,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            job.finished() && job.failed().is_none(),
            "{:?}",
            job.failed()
        );
        let pos: f64 = std::fs::read_to_string(out.path().join("start.pos"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // Keyframe-snapped at or before the target, and the produced
        // content is the remainder only.
        assert!(pos <= 2000.0, "start.pos {pos}");
        let playlist = std::fs::read_to_string(out.path().join("master.m3u8")).unwrap();
        let extinf_sum: f64 = playlist
            .lines()
            .filter_map(|l| l.strip_prefix("#EXTINF:"))
            .filter_map(|l| l.trim_end_matches(',').parse::<f64>().ok())
            .sum();
        assert!(
            extinf_sum < 5.0,
            "offset run produced the whole file: {playlist}"
        );
    }

    /// An HEVC COPY into fMP4 (the container that admits it honestly)
    /// — the copy path negotiates hvc1/au with the mux during caps
    /// negotiation, no capsfilter needed.
    #[test]
    fn fmp4_sink_carries_hevc_copy() {
        crate::init().unwrap();
        if !crate::testutil::has_element("isofmp4mux") || !crate::testutil::has_element("x265enc") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("in.mkv");
        crate::testutil::render_pq_hevc_mkv(&src_path);
        let plan = RemuxPlan {
            video: StreamMode::Copy,
            audio: StreamMode::Encode,
            segment_format: SegmentFormat::Fmp4,
            ..Default::default()
        };
        let out = tempfile::tempdir().unwrap();
        let job = start_at(
            out.path(),
            plan,
            Box::new(FileSource::open(&src_path).unwrap()),
            0,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            job.finished() && job.failed().is_none(),
            "{:?}",
            job.failed()
        );
        let mut bytes = std::fs::read(out.path().join("init.mp4")).unwrap();
        let mut segs: Vec<_> = std::fs::read_dir(out.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "m4s"))
            .collect();
        segs.sort();
        for s in &segs {
            bytes.extend(std::fs::read(s).unwrap());
        }
        let joined = out.path().join("joined.mp4");
        std::fs::write(&joined, bytes).unwrap();
        let info = crate::discover(&joined, Duration::from_secs(30)).unwrap();
        assert_eq!(info.video[0].codec, "hevc", "{info:?}");
    }

    /// A script whose one event fills the LEFT HALF of a 320x240 frame
    /// with a filled rectangle. Drawing commands (`\p1`), not text, on
    /// purpose: libass needs no font for a rectangle, so what this
    /// measures is the pipeline and not the box's fontconfig.
    const ASS_SCRIPT: &str = "[Script Info]\n\
         ScriptType: v4.00+\n\
         PlayResX: 320\n\
         PlayResY: 240\n\
         \n\
         [V4+ Styles]\n\
         Format: Name, Fontname, Fontsize, PrimaryColour, Alignment, MarginL, MarginR, MarginV, Encoding\n\
         Style: Default,Sans,20,&H00FFFFFF,7,0,0,0,1\n\
         \n\
         [Events]\n\
         Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
         Dialogue: 0,0:00:00.00,0:00:10.00,Default,,0,0,0,,{\\p1}m 0 0 l 160 0 l 160 240 l 0 240{\\p0}\n";

    /// Mean luma of the first decoded frame. "In the picture" is a claim
    /// about pixels, and a flat white box costs an encoder no more bits
    /// than the flat black it covers — the burned and unburned runs came
    /// out byte-for-byte the SAME SIZE while the burn worked, so a
    /// size comparison would have been a test that cannot fail usefully.
    fn mean_luma(seg: &Path) -> f64 {
        let pipe = gst::parse::launch(&format!(
            "filesrc location=\"{}\" ! decodebin ! videoconvert ! video/x-raw,format=GRAY8 ! appsink name=out",
            seg.display()
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let out = pipe
            .by_name("out")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        pipe.set_state(gst::State::Playing).unwrap();
        let sample = out.pull_sample().expect("no frame decoded");
        let mean = {
            let buf = sample.buffer().unwrap().map_readable().unwrap();
            buf.iter().map(|b| *b as f64).sum::<f64>() / buf.len() as f64
        };
        pipe.set_state(gst::State::Null).unwrap();
        mean
    }

    /// Encode `src` to a segment and return the segment's path, with or
    /// without an ASS burn. `keep` names the copy that outlives the
    /// run's own scratch dir.
    fn encode_once(
        src: &Path,
        keep: &Path,
        burn_ass: Option<usize>,
        ass_file: Option<&Path>,
        start_ms: u64,
    ) -> std::path::PathBuf {
        let plan = RemuxPlan {
            video: StreamMode::Encode,
            audio: StreamMode::Off,
            video_kbps: Some(4000),
            burn_ass,
            ..Default::default()
        };
        let out = tempfile::tempdir().unwrap();
        let job = start_parts(
            out.path(),
            plan,
            vec![Box::new(FileSource::open(src).unwrap())],
            start_ms,
            None,
            None,
            None,
            ass_file,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "encode did not finish");
        assert!(job.failed().is_none(), "encode failed: {:?}", job.failed());
        let seg = std::fs::read_dir(out.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "ts"))
            .min()
            .expect("no segment produced");
        std::fs::copy(&seg, keep).unwrap();
        keep.to_path_buf()
    }

    fn ass_burn_testable() -> bool {
        crate::init().unwrap();
        if h264_encoder().is_none() || !crate::testutil::has_element("assrender") {
            eprintln!("skipping: no H.264 encoder or no assrender");
            return false;
        }
        true
    }

    /// HUB-32a end to end, sidecar arm: a user's own `.ass` reaches
    /// `assrender` through the appsrc branch and the rendezvous, and
    /// comes out IN THE PICTURE. A rendezvous that never coupled, an
    /// appsrc linked to an inactive pad, or a `link_many` that stole
    /// `text_sink` for the video all land here as "unchanged picture".
    #[test]
    fn a_sidecar_ass_burns_into_the_picture() {
        if !ass_burn_testable() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("black.mkv");
        crate::testutil::render(&format!(
            "videotestsrc num-buffers=50 pattern=black ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! x264enc speed-preset=ultrafast key-int-max=25 bframes=0 ! h264parse ! matroskamux ! filesink location=\"{}\"",
            src_path.display()
        ));
        let ass_path = dir.path().join("sub.ass");
        std::fs::write(&ass_path, ASS_SCRIPT).unwrap();

        let plain = mean_luma(&encode_once(
            &src_path,
            &dir.path().join("plain.ts"),
            None,
            None,
            0,
        ));
        let burned = mean_luma(&encode_once(
            &src_path,
            &dir.path().join("burned.ts"),
            None,
            Some(&ass_path),
            0,
        ));
        assert!(
            plain < 20.0,
            "black fixture is not black: mean luma {plain}"
        );
        // Half the frame turned white; anything near `plain` means the
        // subtitle never reached the encoder.
        assert!(
            burned > 60.0,
            "sidecar ASS did not reach the picture: mean luma {plain} plain vs {burned} burned"
        );
    }

    /// HUB-32a end to end, EMBEDDED arm — the path that matters most,
    /// because it is the only one that also carries a release's attached
    /// fonts. Different plumbing from the sidecar: the demuxer's own
    /// `application/x-ass` pad is intercepted in `plumb_parsed_pad`
    /// instead of being tapped to a file, so it has its own way to fail.
    #[test]
    fn an_embedded_ass_track_burns_into_the_picture() {
        if !ass_burn_testable() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("black-ass.mkv");
        // Round-trips through the sidecar splitter, which is also what
        // feeds matroskamux the payload shape a demuxer emits.
        let (header, events) = crate::subtitles::ass_file_events(ASS_SCRIPT);
        crate::testutil::render_h264_ass_mkv(&src_path, &header, &events);
        let info = crate::discover(&src_path, Duration::from_secs(30)).unwrap();
        assert_eq!(
            info.subtitles.len(),
            1,
            "fixture has no ASS track: {info:?}"
        );

        let plain = mean_luma(&encode_once(
            &src_path,
            &dir.path().join("plain.ts"),
            None,
            None,
            0,
        ));
        let burned = mean_luma(&encode_once(
            &src_path,
            &dir.path().join("burned.ts"),
            Some(0),
            None,
            0,
        ));
        assert!(
            plain < 20.0,
            "black fixture is not black: mean luma {plain}"
        );
        assert!(
            burned > 60.0,
            "embedded ASS did not reach the picture: mean luma {plain} plain vs {burned} burned"
        );

        // A SEEKED start still produces a picture: the seek gate counts
        // video and audio branches only, and a subtitle pad feeding a
        // real consumer instead of a fakesink is a third one — sparse,
        // and it has held preroll hostage before. It does not, and the
        // segment comes out.
        //
        // What it does NOT carry is the event that was already on
        // screen. Measured 2026-08-02: after the flushing seek to 1 s
        // matroskademux issues a segment starting at 1 s and then EOS on
        // the subtitle pad, because the block at t=0 lives in an earlier
        // cluster. So an embedded burn resumed mid-line shows nothing
        // until the NEXT event — the same failure HUB-32b avoided by
        // reading display sets from the container index up front, and
        // the reason that tier does not follow the demuxer either.
        // Asserted as-is rather than left silent; see the note in
        // docs/kahawai-implementation.md.
        let seeked = mean_luma(&encode_once(
            &src_path,
            &dir.path().join("seeked.ts"),
            Some(0),
            None,
            1_000,
        ));
        assert!(
            seeked < 20.0,
            "the demuxer now carries the pre-seek event ({seeked}) — good news, \
             drop this assertion and the limitation note with it"
        );
    }

    /// Regression, reported 2026-08-03: an offset start on an fMP4
    /// COPY session panicked `fmp4mux` ("Timestamps going backwards")
    /// and took the worker with it, so the session produced no segments
    /// at all and the player buffered forever. A seek is an offset
    /// start, which is why seeking reproduced it.
    ///
    /// What the measurements said, so nobody repeats them:
    ///
    /// - It is `h264timestamper`, which reconstructs the DTS Matroska
    ///   does not store. After a FLUSHING SEEK it emits DUPLICATE DTS —
    ///   `1.920, 1.960, 1.960, 2.000, 2.000, …` on a 25 fps source that
    ///   should step 40 ms a frame. Offset zero is clean; only the seek
    ///   breaks it.
    /// - `mpegtsmux` tolerates that (it rebases every stream onto its
    ///   own epoch), which is why this never showed on the TS path.
    /// - The element also rebases its whole branch onto a 1000-hour
    ///   epoch, buffers AND segment together, so stream time stays
    ///   correct. That is a red herring: normalising the base back
    ///   (`segment.start - segment.time`) leaves the panic exactly
    ///   where it was.
    /// - Simply dropping the element for fMP4 does not work either:
    ///   `isofmp4mux` then errors "Require DTS" and produces nothing.
    ///   B-frames are what make the DTS necessary at all.
    ///
    /// The fix is none of those: the seek gate moved UPSTREAM of the
    /// parser chain, so the timestamper never sees pre-seek data and
    /// the flush finds it with no state to corrupt. It still runs, and
    /// still supplies the DTS fMP4 requires — which is why this test
    /// checks the muxed output for missing and non-monotonic DTS rather
    /// than only for the absence of a panic.
    #[test]
    fn an_offset_start_does_not_panic_the_fmp4_muxer() {
        crate::init().unwrap();
        if !crate::testutil::has_element("isofmp4mux") || !crate::testutil::has_element("flacenc") {
            eprintln!("skipping: no isofmp4mux/flacenc");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("flac.mkv");
        // h264 WITH B-FRAMES + FLAC. Both matter: B-frames are what need
        // reconstructed DTS at all, and FLAC has no TS mapping, so this
        // is exactly the shape that negotiates to fMP4 in the field.
        crate::testutil::render_h264_flac_mkv(&src_path);

        let plan = RemuxPlan {
            video: StreamMode::Copy,
            audio: StreamMode::Copy,
            segment_format: SegmentFormat::Fmp4,
            ..Default::default()
        };
        let out = tempfile::tempdir().unwrap();
        let job = start_at(
            out.path(),
            plan,
            Box::new(FileSource::open(&src_path).unwrap()),
            2_000,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "offset start never finished");
        assert!(job.failed().is_none(), "{:?}", job.failed());

        // Whatever the fix turns out to be, it has to keep the property
        // the timestamper was there for: a fragment is only readable
        // behind its init segment, and its video DTS must be complete
        // and monotonic or hls.js rejects the append.
        let mut segs: Vec<_> = std::fs::read_dir(out.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "m4s"))
            .collect();
        segs.sort();
        assert!(!segs.is_empty(), "offset start produced no fragments");
        let mut bytes = std::fs::read(out.path().join("init.mp4")).unwrap();
        for s in &segs {
            bytes.extend(std::fs::read(s).unwrap());
        }
        let joined = dir.path().join("joined.mp4");
        std::fs::write(&joined, bytes).unwrap();
        let (missing, non_mono) = video_dts_defects(&joined);
        assert_eq!(missing, 0, "{missing} video packets with no DTS");
        assert_eq!(non_mono, 0, "{non_mono} non-monotonic video DTS");
    }

    /// HUB-32b burn-in end to end: a PGS source encoded with
    /// `burn_subtitle` must carry the subtitle in the PICTURE, and it
    /// must be there when the session STARTS MID-SET — the case a
    /// live subtitle pad cannot serve.
    ///
    /// Manual (needs a real image-sub file, none is synthesizable here):
    ///   BURN_SRC=/path/clip.mkv BURN_AT_MS=25500 cargo test -p kahawai-media \
    ///     burn_in_from_env -- --ignored --nocapture
    #[test]
    #[ignore]
    fn burn_in_from_env() {
        crate::init().unwrap();
        let Ok(src) = std::env::var("BURN_SRC") else {
            return;
        };
        let at: u64 = std::env::var("BURN_AT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let plan = RemuxPlan {
            video: StreamMode::Encode,
            audio: StreamMode::Off,
            audio_track: 0,
            video_track: 0,
            video_kbps: Some(8000),
            max_height: None,
            max_channels: None,
            tone_map: false,
            burn_subtitle: Some(0),
            ..Default::default()
        };
        let out = tempfile::tempdir().unwrap();
        let job = start_at(
            out.path(),
            plan,
            Box::new(FileSource::open(std::path::Path::new(&src)).unwrap()),
            at,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(300);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            job.failed().is_none(),
            "burn-in run failed: {:?}",
            job.failed()
        );
        let seg = std::fs::read_dir(out.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "ts"))
            .min()
            .expect("no segment produced");
        let keep = std::path::Path::new(
            &std::env::var("BURN_OUT").unwrap_or_else(|_| "/tmp/burn-seg.ts".into()),
        )
        .to_path_buf();
        std::fs::copy(&seg, &keep).unwrap();
        println!("first segment -> {}", keep.display());
    }

    /// HUB-15 channel ceiling: a client that accepts stereo gets
    /// STEREO off a 5.1 source. Range caps fixated to their minimum
    /// and delivered mono — invisible until a browser could declare
    /// the ceiling (the capability debug mask).
    #[test]
    fn channel_ceiling_downmixes_to_the_ceiling_not_mono() {
        crate::init().unwrap();
        if h264_encoder().is_none()
            || aac_encoder().is_none()
            || !crate::testutil::has_element("fdkaacenc")
        {
            eprintln!("skipping: encoders unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("in51.mkv");
        crate::testutil::render_h264_aac51_mkv(&src_path);
        let info = crate::discover(&src_path, Duration::from_secs(30)).unwrap();
        assert_eq!(info.audio[0].channels, 6, "fixture must be 5.1: {info:?}");

        let plan = RemuxPlan {
            video: StreamMode::Copy,
            audio: StreamMode::Encode,
            audio_track: 0,
            video_track: 0,
            video_kbps: None,
            max_height: None,
            max_channels: Some(2),
            tone_map: false,
            burn_subtitle: None,
            ..Default::default()
        };
        let out = tempfile::tempdir().unwrap();
        let job = start_at(
            out.path(),
            plan,
            Box::new(FileSource::open(&src_path).unwrap()),
            0,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "downmix encode did not finish");
        assert!(
            job.failed().is_none(),
            "downmix encode failed: {:?}",
            job.failed()
        );
        let seg = std::fs::read_dir(out.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().is_some_and(|x| x == "ts"))
            .expect("no segment produced");
        let seg_info = crate::discover(&seg, Duration::from_secs(30)).unwrap();
        assert_eq!(
            seg_info.audio[0].channels, 2,
            "stereo ceiling must produce stereo, not mono"
        );
        // The fold is also a session fact (AR-13): the supervisor reads
        // these at ready and amends the verdict with what actually
        // happened to the channel count.
        let facts = crate::facts::read(out.path());
        assert_eq!(
            facts,
            vec![crate::facts::Fact {
                kind: "audio".into(),
                detail: "5.1 → stereo".into()
            }],
            "the downmix must be reported as a fact"
        );
    }

    /// The layout search never answers with a layout the encoder refuses,
    /// never invents channel positions the source does not have, and
    /// never collapses 7.1 to something tiny — the fixation failure that
    /// shipped a DTS 7.1 track to the browser as mono (Linux fdk-aac) and
    /// as an undecodable 4.0-labelled stream (macOS fdk-aac).
    #[test]
    fn aac_layout_search_answers_with_something_the_encoder_takes() {
        crate::init().unwrap();
        let Some(enc) = aac_encoder() else {
            eprintln!("skipping: no AAC encoder");
            return;
        };
        let (n, m) = aac_input_layout(enc, 8, 0xc3f, None).expect("no layout accepted for 7.1");
        assert!(
            aac_accepts(enc, (8, 0xc3f), n, m),
            "chose a layout the encoder cannot round-trip: {n}ch/{m:?}"
        );
        if let Some(m) = m {
            assert_eq!(m & 0xc3f, m, "invented positions the source lacks: 0x{m:x}");
        }
        assert!(n >= 6, "7.1 collapsed to {n} channels");
        // The client's ceiling still bounds the choice (HUB-15).
        let (capped, _) =
            aac_input_layout(enc, 8, 0xc3f, Some(2)).expect("no layout under ceiling");
        assert_eq!(capped, 2, "stereo ceiling ignored");
    }

    /// The pipeline half, on the source material this box can actually
    /// produce: with NO ceiling — what the web client sends — a 5.1
    /// source must come out 5.1 and decode. The pin sits where the
    /// fixation happened, so a pin that chooses badly shows up here as a
    /// shrunken or undecodable stream. (Genuine 7.1 side-surround has no
    /// fixture: every encoder here refuses those caps, and Opus decodes
    /// unpositioned, which audioconvert cannot remap at all. 7.1 is
    /// verified against real content on the fleet.)
    #[test]
    fn unbounded_encode_keeps_the_source_layout_and_decodes() {
        crate::init().unwrap();
        if aac_encoder().is_none() || !crate::testutil::has_element("fdkaacenc") {
            eprintln!("skipping: encoders unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("in51.mkv");
        crate::testutil::render_h264_aac51_mkv(&src_path);
        let info = crate::discover(&src_path, Duration::from_secs(30)).unwrap();
        assert_eq!(info.audio[0].channels, 6, "fixture must be 5.1: {info:?}");

        let plan = RemuxPlan {
            video: StreamMode::Copy,
            audio: StreamMode::Encode,
            audio_track: 0,
            video_track: 0,
            video_kbps: None,
            max_height: None,
            max_channels: None, // what the web client sends: no ceiling
            tone_map: false,
            burn_subtitle: None,
            ..Default::default()
        };
        let out = tempfile::tempdir().unwrap();
        let job = start_at(
            out.path(),
            plan,
            Box::new(FileSource::open(&src_path).unwrap()),
            0,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "unbounded encode did not finish");
        assert!(
            job.failed().is_none(),
            "unbounded encode failed: {:?}",
            job.failed()
        );
        let seg = std::fs::read_dir(out.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().is_some_and(|x| x == "ts"))
            .expect("no segment produced");
        let seg_info = crate::discover(&seg, Duration::from_secs(30)).unwrap();
        assert_eq!(
            seg_info.audio[0].channels, 6,
            "5.1 source must stay 5.1 with no ceiling: {seg_info:?}"
        );
        // Labels can agree with the payload and still be a lie; decoding
        // the whole segment to EOS is what catches a mismatched channel
        // configuration ("channel element 1.1 is not allocated").
        assert!(
            dry_run(&format!(
                "filesrc location={} ! tsdemux ! aacparse ! avdec_aac ! audioconvert ! fakesink",
                seg.display()
            )),
            "segment audio does not decode: {}",
            seg.display()
        );
    }

    /// HUB-15a end to end at pipeline level: a PQ HEVC source encoded
    /// with tone_map produces segments OUR OWN probe reads as SDR —
    /// the capssetter relabel reached the encoder's VUI. (Tone QUALITY
    /// was judged on real HDR movie frames; this guards the plumbing.)
    #[test]
    fn tonemap_encode_outputs_sdr_tagged_video() {
        crate::init().unwrap();
        if h264_encoder().is_none()
            || !tonemap_available()
            || !crate::testutil::has_element("x265enc")
        {
            eprintln!("skipping: encoder, GL tone-map segment, or x265enc unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("in.mkv");
        crate::testutil::render_pq_hevc_mkv(&src_path);
        let info = crate::discover(&src_path, Duration::from_secs(30)).unwrap();
        assert_eq!(
            info.video[0].hdr.as_deref(),
            Some("hdr10"),
            "fixture must probe hdr10"
        );

        let plan = RemuxPlan {
            video: StreamMode::Encode,
            audio: StreamMode::Copy,
            audio_track: 0,
            video_track: 0,
            video_kbps: Some(500),
            max_height: None,
            max_channels: None,
            tone_map: true,
            burn_subtitle: None,
            ..Default::default()
        };
        let out = tempfile::tempdir().unwrap();
        let job = start_at(
            out.path(),
            plan,
            Box::new(FileSource::open(&src_path).unwrap()),
            0,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "tone-map encode did not finish");
        assert!(
            job.failed().is_none(),
            "tone-map encode failed: {:?}",
            job.failed()
        );
        let seg = std::fs::read_dir(out.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().is_some_and(|x| x == "ts"))
            .expect("no segment produced");
        let seg_info = crate::discover(&seg, Duration::from_secs(30)).unwrap();
        assert_eq!(
            seg_info.video[0].hdr, None,
            "output still tagged HDR — the colorimetry relabel failed"
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
        let job = start(
            out.path(),
            COPY_AV,
            Box::new(FileSource::open(&src_path).unwrap()),
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(30);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "remux did not finish in time");
        assert!(job.failed().is_none(), "remux failed: {:?}", job.failed());

        let playlist = std::fs::read_to_string(out.path().join("master.m3u8")).unwrap();
        assert!(
            playlist.contains("segment00000.ts"),
            "playlist:\n{playlist}"
        );
        assert!(
            playlist.contains("#EXT-X-ENDLIST"),
            "playlist not finalized"
        );
        if gst::ElementFactory::find("hlssink3").is_some() {
            assert!(
                playlist.contains("#EXT-X-PLAYLIST-TYPE:EVENT"),
                "hlssink3 playlists must be EVENT for in-flight seeking:\n{playlist}"
            );
        }

        // The segment still carries h264 — remux, not transcode.
        let info =
            crate::discover(&out.path().join("segment00000.ts"), Duration::from_secs(15)).unwrap();
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
            assert_eq!(
                missing,
                0,
                "{}: {missing} video packets with no DTS",
                seg.display()
            );
            assert_eq!(
                non_mono,
                0,
                "{}: {non_mono} non-monotonic video DTS",
                seg.display()
            );
        }
    }

    /// Ffprobe a segment's video packets; return `(missing_dts, non_monotonic)`.
    fn video_dts_defects(seg: &std::path::Path) -> (usize, usize) {
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
