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
fn codec_to_caps_name(kind: &str, codec: &str) -> Option<&'static str> {
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
/// QSV), then software. Dry-run verification is what makes this list
/// safe — a hw element on a box without the driver fails the probe and
/// the next one wins (TC-1/TC-6).
pub const H264_ENCODERS: &[&str] =
    &["vah264enc", "vaapih264enc", "nvh264enc", "qsvh264enc", "x264enc", "openh264enc"];

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
/// report (TC-1): (codec, element) pairs that survived a dry run.
pub fn encoder_capabilities() -> Vec<(&'static str, &'static str)> {
    let mut caps = Vec::new();
    if let Some(el) = h264_encoder() {
        caps.push(("h264", el));
    }
    if let Some(el) = aac_encoder() {
        caps.push(("aac", el));
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

/// What happens to the audio in a session (HUB-16 decision order: copy
/// what the muxer takes, encode what it doesn't but we can decode, drop
/// the rest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioMode {
    Copy,
    /// Decode → AAC. Cheap (a few % CPU) — video still passes through.
    Encode,
    Off,
}

/// Per-kind session plan — the single source of truth shared between
/// session planning and pipeline routing, so the muxer pads requested up
/// front always match the streams that will actually be linked.
/// Video is copy-or-drop today (video encode is the transcoder module's
/// job); audio can be transcoded to AAC in-hub.
#[derive(Debug, Clone, Copy)]
pub struct RemuxPlan {
    pub video: bool,
    pub audio: AudioMode,
}

impl RemuxPlan {
    pub fn has_audio(&self) -> bool {
        self.audio != AudioMode::Off
    }
    /// Anything to produce at all?
    pub fn playable(&self) -> bool {
        self.video || self.has_audio()
    }
}

pub fn plan_streams(info: &kahawai_core::media::MediaInfo) -> RemuxPlan {
    let names = ts_muxable_names();
    let video = info
        .video
        .iter()
        .any(|v| codec_to_caps_name("video", &v.codec).is_some_and(|n| names.contains(n)));
    let audio_caps: Vec<&str> =
        info.audio.iter().filter_map(|a| codec_to_caps_name("audio", &a.codec)).collect();
    let audio = if audio_caps.iter().any(|n| names.contains(*n)) {
        AudioMode::Copy
    } else if aac_encoder().is_some() && audio_caps.iter().any(|n| can_decode(n)) {
        AudioMode::Encode
    } else {
        AudioMode::Off
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
    let video = if plan.video {
        info.video
            .iter()
            .find(|v| codec_to_caps_name("video", &v.codec).is_some_and(|n| names.contains(n)))
            .map(|v| format!("{} copy", v.codec))
            .unwrap_or_else(|| "copy".into())
    } else if info.video.is_empty() {
        "none".into()
    } else {
        format!("{} dropped (needs transcoder)", info.video[0].codec)
    };
    let audio = match plan.audio {
        AudioMode::Copy => info
            .audio
            .iter()
            .find(|a| codec_to_caps_name("audio", &a.codec).is_some_and(|n| names.contains(n)))
            .map(|a| format!("{} copy", a.codec))
            .unwrap_or_else(|| "copy".into()),
        AudioMode::Encode => {
            let src = info
                .audio
                .iter()
                .find(|a| codec_to_caps_name("audio", &a.codec).is_some_and(can_decode))
                .map(|a| a.codec.as_str())
                .unwrap_or("audio");
            format!("{src} → aac (transcoded)")
        }
        AudioMode::Off => {
            if info.audio.is_empty() {
                "none".into()
            } else {
                format!("{} dropped (needs transcoder)", info.audio[0].codec)
            }
        }
    };
    (video, audio)
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
/// Would route_stream do something useful with a stream of these caps?
fn routable(caps_name: &str, encode_audio: bool) -> bool {
    ts_compatible(caps_name).is_some()
        || (encode_audio && caps_name.starts_with("audio/") && can_decode(caps_name))
}

fn plumb_parsed_pad(pipe: &gst::Pipeline, waiting: &WaitingPads, pad: &gst::Pad, encode_audio: bool) {
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
    if routable(&name, encode_audio) {
        route_stream(pipe, waiting, &qsrc, &advertised, encode_audio);
        return;
    }

    let pipe = pipe.clone();
    let waiting = waiting.clone();
    qsrc.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |qpad, info| {
        if let Some(gst::PadProbeData::Event(ev)) = &info.data
            && let gst::EventView::Caps(c) = ev.view()
            && qpad.peer().is_none()
        {
            route_stream(&pipe, &waiting, qpad, &c.caps_owned(), encode_audio);
        }
        gst::PadProbeReturn::Ok
    });
}

/// Route a stream to the muxer (via parser/timestamper) or a fakesink,
/// now that its negotiated caps are known.
fn route_stream(
    pipe: &gst::Pipeline,
    waiting: &WaitingPads,
    from: &gst::Pad,
    caps: &gst::Caps,
    encode_audio: bool,
) {
    let caps_name = caps.structure(0).map(|s| s.name().to_string()).unwrap_or_default();
    let target = ts_compatible(&caps_name).and_then(|kind| waiting.lock().unwrap().remove(kind));
    // Not directly muxable, but the plan says transcode audio and a
    // decoder exists: claim the audio pad for the decode→AAC branch.
    if target.is_none()
        && encode_audio
        && caps_name.starts_with("audio/")
        && can_decode(&caps_name)
        && let Some(sinkpad) = waiting.lock().unwrap().remove("audio")
    {
        tracing::info!(caps = %caps_name, "remux: transcoding audio stream to AAC");
        build_audio_encode_chain(pipe, from, sinkpad);
        return;
    }
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
            // hlssink3 (≤0.15.3, imp.rs:304) unwraps the PTS of each
            // fragment's first buffer; a PTS-less frame (old AVI streams)
            // aborts the whole process — a Rust panic in an FFI callback
            // cannot unwind. Guard: borrow the DTS, or drop the buffer.
            tail.add_probe(gst::PadProbeType::BUFFER, |_, info| {
                if let Some(gst::PadProbeData::Buffer(buffer)) = &mut info.data {
                    // ponytail: pts=dts misorders B-frames (sweep flags
                    // those [bad dts] → transcoder work list); dropping
                    // instead starves fragments and trips more panics.
                    if buffer.pts().is_none() {
                        match buffer.dts() {
                            Some(dts) => buffer.make_mut().set_pts(dts),
                            None => return gst::PadProbeReturn::Drop,
                        }
                    }
                }
                gst::PadProbeReturn::Ok
            });
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

/// decodebin (auto-picks the best-ranked decoder — registry-derived, per
/// the fallback strategy) → audioconvert → audioresample → AAC encoder →
/// aacparse (raw→ADTS for the TS muxer) → muxer pad. The only decode/
/// encode work in the hub, and audio-only by design: a few % CPU.
fn build_audio_encode_chain(pipe: &gst::Pipeline, from: &gst::Pad, sinkpad: gst::Pad) {
    let Some(enc_name) = aac_encoder() else {
        // Planner guarantees this; guard anyway (fakesink beats a stall).
        tracing::error!("audio encode routed with no verified AAC encoder");
        return;
    };
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
    if let Err(e) = parse.static_pad("src").unwrap().link(&sinkpad) {
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
    Need(u32),
    Seek(u64),
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
fn make_hls_sink(out_dir: &Path) -> Result<(gst::Element, &'static str)> {
    let name = HLS_SINKS
        .iter()
        .find(|n| gst::ElementFactory::find(n).is_some())
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
pub fn start(out_dir: &Path, plan: RemuxPlan, mut source: Box<dyn RemuxSource>) -> Result<RemuxJob> {
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
    let seek_tx = cmd_tx.clone();
    appsrc.set_callbacks(
        gstreamer_app::AppSrcCallbacks::builder()
            .need_data(move |_, length| {
                let _ = cmd_tx.send(FeedCmd::Need(length));
            })
            .seek_data(move |_, offset| seek_tx.send(FeedCmd::Seek(offset)).is_ok())
            .build(),
    );
    let feeder_src = appsrc.clone();
    std::thread::spawn(move || {
        let mut pos: u64 = 0;
        let mut at_eos = false;
        while let Ok(cmd) = cmd_rx.recv() {
            match cmd {
                FeedCmd::Seek(offset) => {
                    pos = offset;
                    at_eos = false;
                }
                FeedCmd::Need(length) => {
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
    let (hlssink, _sink_name) = make_hls_sink(out_dir)?;

    pipeline.add_many([appsrc.upcast_ref::<gst::Element>(), &parsebin, &hlssink])?;
    gst::Element::link_many([appsrc.upcast_ref::<gst::Element>(), &parsebin])?;

    // Request the muxer pads *now* — splitmuxsink inside hlssink2 must see
    // them before starting or it never leaves Ready.
    anyhow::ensure!(plan.playable(), "nothing to remux");
    let waiting: WaitingPads = Arc::new(Mutex::new(std::collections::HashMap::new()));
    if plan.video {
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
    let pipe = pipeline.clone();
    let waiting2 = waiting.clone();
    let encode_audio = plan.audio == AudioMode::Encode;
    parsebin.connect_pad_added(move |_, pad| {
        plumb_parsed_pad(&pipe, &waiting2, pad, encode_audio);
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

    const COPY_AV: RemuxPlan = RemuxPlan { video: true, audio: AudioMode::Copy };

    /// Manual repro: REMUX_SRC=/path/to/file cargo test -p kahawai-media \
    ///   remux_file_from_env -- --ignored --nocapture
    #[test]
    #[ignore]
    fn remux_file_from_env() {
        let src = std::path::PathBuf::from(std::env::var("REMUX_SRC").expect("set REMUX_SRC"));
        let out = tempfile::tempdir().unwrap();
        let info = crate::discover(&src, Duration::from_secs(30)).unwrap();
        let plan = plan_streams(&info);
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
        let plan = plan_streams(&info);
        assert!(plan.video, "hevc is TS-muxable");
        if names.contains("audio/x-eac3") {
            assert_eq!(plan.audio, AudioMode::Copy);
        } else {
            // eac3 not muxable: transcode when possible, else drop.
            assert_ne!(plan.audio, AudioMode::Copy);
        }
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
        let plan = plan_streams(&info);
        assert_eq!(plan.audio, AudioMode::Encode, "eac3 should plan as Encode: {info:?}");
        assert!(plan.video);

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

    #[test]
    fn hls_sink_selection_prefers_best_available() {
        crate::init().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (_, name) = make_hls_sink(dir.path()).unwrap();
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
