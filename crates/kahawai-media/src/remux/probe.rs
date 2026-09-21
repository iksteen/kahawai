use super::*;

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
pub(super) fn sink_compatible(caps: &gst::Caps, format: SegmentFormat) -> Option<&'static str> {
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
        ("audio", "aac") => b.field("mpegversion", gst::List::new([2i32, 4i32])).build(),
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
pub(super) fn verified_encoder(
    list: &[&'static str],
    dry: fn(&str) -> bool,
) -> Option<&'static str> {
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

/// The software entries of the three lists above — everything else in
/// them is hardware. One list, because two places ask the same question
/// and would drift: TC-1's `hardware` flag, which placement filters on,
/// and the per-session `video encoder selected` log that says whether
/// this session degraded (TC-6).
pub const SW_VIDEO_ENCODERS: &[&str] = &[
    "x264enc",
    "openh264enc",
    "x265enc",
    "svtav1enc",
    "rav1enc",
    "av1enc",
];

/// Best available video encoder for this box, with an optional exact output
/// size. The startup capability path has no size and picks the first encoder
/// that survives its own caps-sized probe. A session whose demuxed caps carry
/// dimensions additionally skips candidates that reject that picture size.
pub(super) fn verified_video_encoder(
    list: &[&'static str],
    dimensions: Option<(i32, i32)>,
) -> Option<&'static str> {
    static VERIFIED: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<&'static str, bool>>,
    > = std::sync::LazyLock::new(Default::default);
    let verified = &*VERIFIED;
    let _ = crate::init();
    list.iter().copied().find(|name| {
        if gst::ElementFactory::find(name).is_none() {
            return false;
        }
        if let Some((width, height)) = dimensions
            && !video_encoder_accepts_dimensions(name, width, height)
        {
            tracing::warn!(
                encoder = name,
                width,
                height,
                "encoder rejects session dimensions; trying next"
            );
            return false;
        }
        if let Some(ok) = verified.lock().unwrap().get(name).copied() {
            return ok;
        }
        let ok = dry_run_video_encoder(name);
        verified.lock().unwrap().insert(name, ok);
        if !ok {
            tracing::warn!(encoder = name, "encoder failed dry-run; trying next");
        }
        ok
    })
}

/// Best available H.264 encoder, dry-run-verified once. None → this box
/// cannot transcode video.
pub fn h264_encoder() -> Option<&'static str> {
    static VERIFIED: std::sync::LazyLock<Option<&'static str>> =
        std::sync::LazyLock::new(|| verified_video_encoder(H264_ENCODERS, None));
    *VERIFIED
}

pub fn hevc_encoder() -> Option<&'static str> {
    static VERIFIED: std::sync::LazyLock<Option<&'static str>> =
        std::sync::LazyLock::new(|| verified_video_encoder(HEVC_ENCODERS, None));
    *VERIFIED
}

pub fn av1_encoder() -> Option<&'static str> {
    static VERIFIED: std::sync::LazyLock<Option<&'static str>> =
        std::sync::LazyLock::new(|| verified_video_encoder(AV1_ENCODERS, None));
    *VERIFIED
}

/// TC-6 CPU share: the thread ceiling a worker was configured with, or
/// None for the encoder's own default.
///
/// Process-global and set once at worker startup, exactly like
/// `demote_elements` and for the same reason: it is a fact about this
/// box, not a decision about this session, and a worker process runs one
/// session anyway. Threading it through the plan would have it cross
/// four call sites to say the same thing.
pub(super) static ENCODER_THREADS: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

pub fn set_encoder_threads(n: u32) {
    let _ = ENCODER_THREADS.set(n);
}

/// Apply that ceiling to a SOFTWARE encoder. Hardware encoders are left
/// alone — their concurrency is the driver's, and none of them carries a
/// thread property to set anyway.
///
/// One property name per encoder, each read off `gst-inspect` on a box
/// that has the element rather than from memory. x265enc is the odd one:
/// it exposes no thread property at all, only libx265's `option-string`,
/// where `pools` is the total-thread knob. x264enc has an
/// `option-string` too and must NOT be set through it — it has a real
/// `threads` property, and writing the string would discard whatever
/// else a future caller put there.
pub(super) fn set_encoder_threads_on(enc: &gst::Element, name: &str, n: u32) {
    match name {
        "x264enc" | "av1enc" | "rav1enc" => set_prop_str_if_present(enc, "threads", &n.to_string()),
        "openh264enc" => set_prop_str_if_present(enc, "multi-thread", &n.to_string()),
        // SVT-AV1 renamed this when its 3.0 API landed. GStreamer 1.28
        // exposes `logical-processors`; newer releases expose
        // `level-of-parallelism`. Prefer the new spelling, but support
        // the pinned release image as well.
        "svtav1enc" => {
            if let Some(prop) = svtav1_thread_property(enc) {
                set_prop_str_if_present(enc, prop, &n.to_string());
            }
        }
        "x265enc" => set_prop_str_if_present(enc, "option-string", &format!("pools={n}")),
        // Hardware, or an encoder we have never measured a knob on.
        _ => return,
    }
    tracing::info!(
        encoder = name,
        threads = n,
        "software encoder thread ceiling applied"
    );
}

pub(super) fn svtav1_thread_property(enc: &gst::Element) -> Option<&'static str> {
    ["level-of-parallelism", "logical-processors"]
        .into_iter()
        .find(|name| enc.find_property(name).is_some())
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
    let mut caps = Vec::new();
    for (codec, el) in [
        ("h264", h264_encoder()),
        ("hevc", hevc_encoder()),
        ("av1", av1_encoder()),
    ] {
        if let Some(el) = el {
            caps.push((codec, el, !SW_VIDEO_ENCODERS.contains(&el)));
        }
    }
    for (codec, el) in [("aac", aac_encoder()), ("opus", opus_encoder())] {
        if let Some(el) = el {
            caps.push((codec, el, false));
        }
    }
    caps
}

pub(super) const VIDEO_PROBE_WIDTH: i32 = 640;
pub(super) const VIDEO_PROBE_HEIGHT: i32 = 480;

/// System-memory raw-video caps an encoder accepts. The real chain reaches
/// every non-NV encoder through `videoconvert`; NV additionally has its CUDA
/// converter path, but also advertises this fallback. A device-specific VA
/// factory carries the active driver's limits here — including radeonsi's
/// 384-pixel minimum for HEVC on gfx1200.
pub(super) fn video_encoder_sink_caps(name: &str) -> Option<gst::Caps> {
    let factory = gst::ElementFactory::find(name)?;
    let raw = gst::Caps::builder("video/x-raw").build();
    let mut accepted = gst::Caps::new_empty();
    for template in factory
        .static_pad_templates()
        .into_iter()
        .filter(|template| template.direction() == gst::PadDirection::Sink)
    {
        accepted.merge(template.caps().intersect(&raw));
    }
    (!accepted.is_empty()).then_some(accepted)
}

/// A cheap, ordinary probe size inside one encoder's advertised limits.
/// 640x480 keeps the five-buffer startup probe small; fixation moves either
/// axis to the nearest supported value when a driver requires something else.
pub(super) fn video_probe_dimensions_from_caps(caps: &gst::Caps) -> Option<(i32, i32)> {
    let mut fixed = caps.copy();
    fixed.truncate();
    let structure = fixed.get_mut()?.structure_mut(0)?;
    for (field, preferred) in [("width", VIDEO_PROBE_WIDTH), ("height", VIDEO_PROBE_HEIGHT)] {
        if structure.has_field(field) {
            structure.fixate_field_nearest_int(field, preferred);
        } else {
            structure.set(field, preferred);
        }
    }
    let width = structure.get::<i32>("width").ok()?;
    let height = structure.get::<i32>("height").ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

pub(super) fn video_caps_accept_dimensions(caps: &gst::Caps, width: i32, height: i32) -> bool {
    let dimensions = gst::Caps::builder("video/x-raw")
        .field("width", width)
        .field("height", height)
        .build();
    caps.can_intersect(&dimensions)
}

pub(super) fn video_encoder_accepts_dimensions(name: &str, width: i32, height: i32) -> bool {
    video_encoder_sink_caps(name)
        .is_some_and(|caps| video_caps_accept_dimensions(&caps, width, height))
}

pub(super) fn dry_run_video_encoder(name: &str) -> bool {
    let Some(caps) = video_encoder_sink_caps(name) else {
        tracing::warn!(
            encoder = name,
            "encoder has no system-memory raw-video sink caps"
        );
        return false;
    };
    let Some((width, height)) = video_probe_dimensions_from_caps(&caps) else {
        tracing::warn!(encoder = name, "encoder has no usable video dimensions");
        return false;
    };
    // No forced pixel format: encoders differ (x264enc takes I420,
    // nvh264enc only NV12/RGBA-family) — videoconvert lets negotiation
    // pick whatever the encoder accepts, exactly like the real pipeline.
    let launch = format!(
        "videotestsrc num-buffers=5 ! video/x-raw,width={width},height={height} \
         ! videoconvert ! {name} ! fakesink"
    );
    match dry_run_result(&launch) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                encoder = name,
                width,
                height,
                error = format!("{error:#}"),
                "video encoder dry-run failed"
            );
            false
        }
    }
}

pub(super) fn dry_run_encoder(name: &str) -> bool {
    dry_run(&format!(
        "audiotestsrc num-buffers=5 ! audioconvert ! audioresample ! {name} ! fakesink"
    ))
}

pub(super) fn dry_run(launch: &str) -> bool {
    dry_run_result(launch).is_ok()
}

pub(super) fn dry_run_result(launch: &str) -> Result<()> {
    let p = gst::parse::launch(launch).context("parsing dry-run pipeline")?;
    if let Err(error) = p.set_state(gst::State::Playing) {
        let _ = p.set_state(gst::State::Null);
        anyhow::bail!("starting dry-run pipeline: {error}");
    }
    let result = match p.bus().and_then(|bus| {
        bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(5),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
    }) {
        Some(message) => match message.view() {
            gst::MessageView::Eos(_) => Ok(()),
            gst::MessageView::Error(error) => Err(anyhow::anyhow!(
                "{}: {} ({:?})",
                error
                    .src()
                    .map(|source| source.name().to_string())
                    .unwrap_or_else(|| "unknown".into()),
                error.error(),
                error.debug()
            )),
            _ => unreachable!("filtered for EOS and error"),
        },
        None => Err(anyhow::anyhow!("dry-run pipeline timed out")),
    };
    let _ = p.set_state(gst::State::Null);
    result
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

pub(super) fn probe_ass_burn_into(encoder: &str) -> bool {
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

pub(super) fn probe_tonemap_into(encoder: &str) -> bool {
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
