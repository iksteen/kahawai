//! Pulling a time window out of a media file with GStreamer: interleaved
//! `i16` audio, luma planes, or keyframe timestamps, whichever the analyzer
//! needs.
//!
//! Kahawai's decode stack is GStreamer, so this is what intro detection uses —
//! it also means the comparison against intro-skipper (which shells out to
//! ffmpeg) exercises two decoders, and a disagreement that traces back to
//! decoding is a finding rather than a rounding error.
//!
//! Stream SELECTION diverges too, deliberately undramatically: the first
//! exposed pad of the wanted type wins, where ffmpeg's default picks the
//! highest-resolution video and most-channels audio. A file whose first
//! audio track is a stereo commentary is fingerprinted on the commentary
//! here and on the main 5.1 there — same season, different fingerprints,
//! parity quietly off for that file. Known and accepted until a real
//! library shows commentary-first muxes; ranking pads means buffering
//! every pad until no-more-pads.

use std::str::FromStr;

use anyhow::{Context, Result, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSink;
use gstreamer_video::VideoFrameExt;

/// A decoded audio window, in the file's own sample rate and channel layout —
/// resampling is Chromaprint's job, and doing it here would put a different
/// resampler in front of the fingerprint than `fpcalc` uses.
pub struct AudioWindow {
    pub rate: u32,
    pub channels: u32,
    /// Interleaved samples.
    pub samples: Vec<i16>,
}

/// Where an episode's bytes come from.
///
/// A local path when there is one; otherwise anything the caller can open for
/// random access — which in the hub is a mediahost lease, the same one the
/// remuxer and the subtitle extractor read through. Analysis opens a source
/// per pass, so this hands out a new one each time rather than holding one.
#[derive(Clone)]
pub enum Media {
    Path(std::path::PathBuf),
    Remote {
        /// For logs and reports; a path the analyzer must not try to open.
        name: String,
        #[allow(clippy::type_complexity)]
        open: std::sync::Arc<
            dyn Fn() -> Result<Box<dyn kahawai_media::remux::RemuxSource>> + Send + Sync,
        >,
    },
}

impl Media {
    /// A name to put in a report or a log line.
    pub fn name(&self) -> String {
        match self {
            Media::Path(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Media::Remote { name, .. } => name.clone(),
        }
    }

    fn source_element(&self) -> Result<gst::Element> {
        match self {
            Media::Path(path) => gst::ElementFactory::make("filesrc")
                .property("location", path)
                .build()
                .context("filesrc"),
            Media::Remote { open, .. } => {
                Ok(kahawai_media::remux::seekable_appsrc(open()?).upcast())
            }
        }
    }
}

impl std::fmt::Debug for Media {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

impl From<&std::path::Path> for Media {
    fn from(path: &std::path::Path) -> Self {
        Media::Path(path.to_path_buf())
    }
}
/// Caps that still describe a container rather than one elementary stream.
/// `decodebin` must autoplug these far enough to expose the tracks before we
/// can stop the tracks the current analyzer does not consume.
fn is_container_caps(caps: &gst::CapsRef) -> bool {
    caps.structure(0).is_some_and(|s| {
        matches!(
            s.name().as_str(),
            "application/mxf"
                | "application/ogg"
                | "audio/x-matroska"
                | "audio/webm"
                | "video/quicktime"
                | "video/webm"
                | "video/x-flv"
                | "video/x-matroska"
                | "video/x-ms-asf"
                | "video/x-msvideo"
                | "video/mpegts"
        ) || (s.name() == "video/mpeg" && s.get::<bool>("systemstream").unwrap_or(false))
    })
}

/// Continue autoplugging this stream only when it can still reveal tracks or
/// when it is a media type this analyzer requested. Returning false exposes an
/// unwanted compressed elementary stream immediately, so parking it below
/// does not first instantiate and run its decoder.
fn should_autoplug(caps: &gst::CapsRef, want: &[&str]) -> bool {
    if is_container_caps(caps) {
        return true;
    }
    let Some(name) = caps.structure(0).map(|s| s.name()) else {
        return true;
    };
    if name.starts_with("audio/") {
        return want.iter().any(|w| w.starts_with("audio/"));
    }
    if name.starts_with("video/") || name.starts_with("image/") {
        return want
            .iter()
            .any(|w| w.starts_with("video/") || w.starts_with("image/"));
    }
    true
}

/// Build `<source> ! <opener> ! <chain…> ! appsink`, linking only the streams
/// whose caps start with one of `want` and parking the rest on fakesinks.
///
/// Unwanted elementary streams stop autoplugging before their decoder. Parking
/// only the decoded pad made an audio fingerprint decode the video's first
/// quarter too (and made a luma probe decode audio); on Silence that turned a
/// local 1080p season into six minutes of work.
fn open(
    media: &Media,
    stop_at: Option<gst::Caps>,
    want: &'static [&'static str],
    chain: &[&str],
    caps: Option<gst::Caps>,
) -> Result<(
    gst::Pipeline,
    AppSink,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
)> {
    kahawai_media::init()?;

    let pipeline = gst::Pipeline::new();
    let src = media.source_element()?;
    // `decodebin` demuxes the container and, given `caps`, stops before the
    // decoder — which is how the keyframe scan gets encoded buffers while
    // still being able to seek by time.
    let mut opener = gst::ElementFactory::make("decodebin");
    if let Some(stop_at) = stop_at {
        opener = opener.property("caps", stop_at);
    }
    let opener_element = opener.build().context("decodebin")?;
    let autoplug_want = want;
    opener_element.connect("autoplug-continue", false, move |args| {
        let caps = args[2].get::<gst::Caps>().ok()?;
        Some(should_autoplug(&caps, autoplug_want).to_value())
    });

    // `max-buffers`: with sync=false nothing else throttles the decoder,
    // and appsink's default queue is UNLIMITED — a consumer doing per-frame
    // work (the luma scans) fell behind and the queue held raw video, tens
    // of MB per 4K frame, inside the hub process. Four buffers block the
    // producer instead: backpressure, no fidelity cost.
    let sink = match &caps {
        Some(caps) => AppSink::builder()
            .sync(false)
            .max_buffers(4)
            .caps(caps)
            .build(),
        None => AppSink::builder().sync(false).max_buffers(4).build(),
    };

    pipeline
        .add_many([&src, &opener_element, sink.upcast_ref::<gst::Element>()])
        .context("assembling the analysis pipeline")?;
    src.link(&opener_element).context("source → decoder")?;

    let mut elements: Vec<gst::Element> = Vec::new();
    for name in chain {
        let mut builder = gst::ElementFactory::make(name);
        if *name == "videoconvert" {
            // Change the pixel format and nothing else. Left to itself,
            // videoconvert also converts colorimetry, and on an HDR10 source
            // (BT.2020, PQ) that would rewrite the very luma the black-frame
            // threshold is a raw value of. Belt and braces: the caps below ask
            // for formats a decoder already produces, so this is normally
            // passthrough anyway.
            builder = builder
                .property_from_str("matrix-mode", "none")
                .property_from_str("gamma-mode", "none")
                .property_from_str("primaries-mode", "none");
        }
        let element = builder
            .build()
            .with_context(|| format!("{name} is not installed"))?;
        pipeline.add(&element)?;
        elements.push(element);
    }
    for pair in elements.windows(2) {
        pair[0].link(&pair[1])?;
    }
    if let Some(last) = elements.last() {
        last.link(&sink)?;
    }

    let head = elements
        .first()
        .cloned()
        .unwrap_or_else(|| sink.clone().upcast());
    // Raised when the demuxer has shown every pad it has and none was the
    // wanted type: a file with no such track NEVER prerolls (the appsink
    // waits for data that cannot come), and without this signal the probe
    // burned its full preroll timeout to learn it — per probe, eight times
    // per black-frame search, for something a theme.mp3 in the season
    // folder makes routine.
    let no_wanted_track = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (head, flag) = (head.clone(), no_wanted_track.clone());
        opener_element.connect_no_more_pads(move |_| {
            let linked = head.static_pad("sink").is_some_and(|pad| pad.is_linked());
            if !linked {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }
    let weak = pipeline.downgrade();
    opener_element.connect_pad_added(move |_, pad| {
        let wanted = pad
            .current_caps()
            .and_then(|c| {
                c.structure(0)
                    .map(|s| want.iter().any(|w| s.name().starts_with(w)))
            })
            .unwrap_or(false);
        let target = head.static_pad("sink").expect("chain head has a sink pad");
        if wanted && !target.is_linked() {
            let _ = pad.link(&target);
            return;
        }
        // Everything else has to go somewhere or the pipeline stalls on an
        // unlinked pad. `sync=false` because a sink that honours the clock
        // throttles its branch to real time, and the demuxer feeding both
        // branches then blocks: measured on a 22-minute episode, the audio
        // stopped arriving after 28 seconds of video had been swallowed at
        // playback speed. `async=false` so this sink is not part of preroll.
        if let Some(pipeline) = weak.upgrade()
            && let Ok(fake) = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .property("async", false)
                .build()
        {
            let _ = pipeline.add(&fake);
            let _ = fake.sync_state_with_parent();
            if let Some(pad_sink) = fake.static_pad("sink") {
                let _ = pad.link(&pad_sink);
            }
        }
    });

    Ok((pipeline, sink, no_wanted_track))
}

/// Land exactly on the requested time. Right for audio and for reading buffer
/// flags; wrong for pixels, see below.
const ACCURATE: gst::SeekFlags = gst::SeekFlags::from_bits_truncate(
    gst::SeekFlags::FLUSH.bits() | gst::SeekFlags::ACCURATE.bits(),
);

/// Seconds of video decoded before the window that is actually wanted.
///
/// A decoder handed the middle of a GOP emits pictures until it has the
/// references to reconstruct one, and those pictures are dark. Measured on an
/// HDR episode: the first frames of a window read 92% black where the file's
/// own frame is 4% black — which is exactly the signal the credits search
/// looks for, so every 2-second probe was measuring the decoder. One second of
/// lead-in was enough there; two is the margin. A calibration knob, not a
/// constant of nature: a decoder that settles more slowly needs more.
const LEAD_IN: f64 = 2.0;

fn seek_window(
    pipeline: &gst::Pipeline,
    start: f64,
    end: f64,
    flags: gst::SeekFlags,
) -> Result<()> {
    pipeline
        .seek(
            1.0,
            flags,
            gst::SeekType::Set,
            gst::ClockTime::from_nseconds((start.max(0.0) * 1e9) as u64),
            gst::SeekType::Set,
            gst::ClockTime::from_nseconds((end.max(0.0) * 1e9) as u64),
        )
        .context("seeking to the analysis window")?;
    Ok(())
}

/// Run a built pipeline over `[start, end)`, handing every sample to
/// `on_sample` until end of stream.
fn drain(
    pipeline: &gst::Pipeline,
    sink: &AppSink,
    no_wanted_track: &std::sync::atomic::AtomicBool,
    start: f64,
    end: f64,
    flags: gst::SeekFlags,
    mut on_sample: impl FnMut(&gst::Sample) -> Result<()>,
) -> Result<()> {
    tracing::debug!(start, end, "analysis pipeline: prerolling");
    // ONE exit: every path below, error or not, must reach the Null at the
    // bottom — a pipeline dropped while PAUSED keeps its streaming threads
    // and file handles, and the fast-fail paths run once per unreadable
    // file in a sweep, not once in a blue moon.
    let result = (|| -> Result<()> {
        pipeline
            .set_state(gst::State::Paused)
            .context("pausing the analysis pipeline")?;
        // Preroll before seeking: a seek on a pipeline that has not negotiated yet
        // is dropped, and the window would silently start at zero. In slices, so
        // a file with no wanted track fails in about a second instead of the
        // whole timeout. And a preroll that TIMES OUT is a failure, not a pass:
        // `state()` answers Ok(Async) when preroll has not finished, and taking
        // that as success dropped the seek — the probe then measured the head of
        // the file and recorded its answer as the requested window's.
        let mut waited = 0u32;
        loop {
            let (res, _, _) = pipeline.state(gst::ClockTime::from_seconds(1));
            match res.context("prerolling the analysis pipeline")? {
                gst::StateChangeSuccess::Success | gst::StateChangeSuccess::NoPreroll => break,
                gst::StateChangeSuccess::Async => {
                    if no_wanted_track.load(std::sync::atomic::Ordering::Relaxed) {
                        anyhow::bail!("the file has no track of the wanted type");
                    }
                    waited += 1;
                    if waited >= 30 {
                        anyhow::bail!(
                            "preroll did not finish in 30 s; the seek would be dropped \
                         and the probe would silently read the wrong window"
                        );
                    }
                }
            }
        }

        tracing::debug!("analysis pipeline: prerolled, seeking");
        seek_window(pipeline, start, end, flags)?;
        pipeline
            .set_state(gst::State::Playing)
            .context("starting the analysis pipeline")?;
        tracing::debug!("analysis pipeline: playing");

        let bus = pipeline.bus().expect("a pipeline always has a bus");
        // Any error already on the bus, said now rather than never.
        let bus_error = |bus: &gst::Bus| -> Result<()> {
            while let Some(msg) =
                bus.timed_pop_filtered(gst::ClockTime::ZERO, &[gst::MessageType::Error])
            {
                if let gst::MessageView::Error(err) = msg.view() {
                    anyhow::bail!(
                        "gstreamer: {} ({})",
                        err.error(),
                        err.debug().unwrap_or_default()
                    );
                }
            }
            Ok(())
        };
        let mut samples = 0usize;
        let mut quiet = 0usize;
        loop {
            // Never the blocking pull: a decoder that errors mid-stream can
            // pause the streaming thread WITHOUT pushing EOS downstream, and
            // pull_sample then waits for a sample that is never coming — the
            // sweep hung inside one probe, holding the between-episodes gate
            // viewers depend on.
            // Short waits in a loop, so a decoder ERROR surfaces within
            // seconds while SILENCE gets five minutes: the sweep shares the
            // byte plane with viewers by design, and a starved lease under a
            // live transcode is contention, not a hang — but the
            // decoder-error-without-EOS deadlock this loop exists for should
            // not cost five minutes per probe to notice.
            match sink.try_pull_sample(gst::ClockTime::from_seconds(10)) {
                Some(sample) => {
                    quiet = 0;
                    samples += 1;
                    on_sample(&sample)?;
                }
                None if sink.is_eos() => break,
                None => {
                    bus_error(&bus)?;
                    quiet += 1;
                    if quiet >= 30 {
                        anyhow::bail!("analysis pipeline produced nothing for 300 s");
                    }
                }
            }
        }
        tracing::debug!(samples, "analysis pipeline: drained");
        // An error can also arrive WITH the EOS; the bus says so.
        bus_error(&bus)?;
        Ok(())
    })();

    let _ = pipeline.set_state(gst::State::Null);
    result
}

/// Decode `[start, end)` of the file's first audio track.
///
/// Stereo is forced the way intro-skipper's `-ac 2` forces it, so a mono source
/// is fingerprinted as the same duplicated pair on both sides.
pub fn audio_window(media: &Media, start: f64, end: f64) -> Result<AudioWindow> {
    kahawai_media::init()?;
    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("layout", "interleaved")
        .field("channels", 2i32)
        .build();
    let (pipeline, sink, missing) = open(media, None, &["audio/"], &["audioconvert"], Some(caps))?;

    let mut window = AudioWindow {
        rate: 0,
        channels: 2,
        samples: Vec::new(),
    };
    drain(&pipeline, &sink, &missing, start, end, ACCURATE, |sample| {
        if window.rate == 0
            && let Some(s) = sample.caps().and_then(|c| c.structure(0))
            && let (Ok(rate), Ok(channels)) = (s.get::<i32>("rate"), s.get::<i32>("channels"))
        {
            window.rate = rate as u32;
            window.channels = channels as u32;
        }
        let Some(buffer) = sample.buffer() else {
            return Ok(());
        };
        let map = buffer.map_readable().context("mapping an audio buffer")?;
        window.samples.extend(
            map.chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]])),
        );
        Ok(())
    })?;

    if window.rate == 0 {
        bail!("{}: no audio track was decoded", media.name());
    }
    Ok(window)
}

/// The encoded video formats a keyframe flag can be read from. `decodebin`
/// stops here instead of decoding, so the scan costs a demux rather than a
/// decode — and, unlike `parsebin`, it still knows how to seek by time.
const ENCODED_VIDEO: &str = "video/x-h264; video/x-h265; video/mpeg; video/x-vp8; \
     video/x-vp9; video/x-av1; video/x-msmpeg; video/x-divx; video/x-theora; \
     video/x-h263; video/x-wmv; image/jpeg";

/// Keyframe timestamps inside `[start, end)`, read from the demuxed stream —
/// no decoding, the flag the container sets is the answer.
///
/// intro-skipper reads these from `ffmpeg -skip_frame nokey -vf showinfo` to
/// snap an intro's end to a seekable point.
pub fn keyframes_window(media: &Media, start: f64, end: f64) -> Result<Vec<f64>> {
    kahawai_media::init()?;
    let encoded = gst::Caps::from_str(ENCODED_VIDEO).context("keyframe caps")?;
    let (pipeline, sink, missing) = open(media, Some(encoded), &["video/", "image/"], &[], None)?;

    let mut times = Vec::new();
    let mut raw = false;
    drain(&pipeline, &sink, &missing, start, end, ACCURATE, |sample| {
        // A codec outside the list above reaches us decoded, where every buffer
        // claims to be a keyframe. Saying so beats snapping to any frame at all.
        if let Some(s) = sample.caps().and_then(|c| c.structure(0))
            && s.name() == "video/x-raw"
        {
            raw = true;
            return Ok(());
        }
        if let Some(buffer) = sample.buffer()
            && !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT)
            && let Some(pts) = buffer.pts()
        {
            times.push(pts.nseconds() as f64 / 1e9);
        }
        Ok(())
    })?;

    if raw {
        bail!(
            "{}: keyframes are not readable for this codec",
            media.name()
        );
    }
    times.retain(|t| *t >= start && *t <= end);
    Ok(times)
}

/// One decoded video frame reduced to what black-frame detection needs.
pub struct LumaFrame {
    /// Seconds from the start of the file.
    pub time: f64,
    /// Share of pixels whose luma is below the threshold, 0-100.
    pub black_percentage: f64,
    /// Mean luma, as stored. Not used by the analyzers; it is what makes a
    /// disagreement with another decoder diagnosable.
    pub mean_luma: f64,
}

/// Formats whose first plane is luma we can read directly. Asking for these
/// rather than for I420 keeps `videoconvert` in passthrough on everything a
/// decoder normally produces — and a conversion here is not free of meaning:
/// converting an HDR10 frame to plain I420 also converts its colorimetry, which
/// rewrites the luma the black-frame threshold is measured against.
const LUMA_FORMATS: &str = "video/x-raw, format = { I420, YV12, NV12, NV21, Y42B, Y444, \
     I420_10LE, I422_10LE, Y444_10LE, P010_10LE }";

/// How the first plane stores a luma sample, and what it takes to read it as
/// the 0-255 value intro-skipper's thresholds are written in.
fn luma_shift(format: gstreamer_video::VideoFormat) -> (bool, u32) {
    use gstreamer_video::VideoFormat::*;
    match format {
        // 10-bit in the low bits of a 16-bit word.
        I42010le | I42210le | Y44410le => (true, 2),
        // P010 keeps the value in the *high* bits of the word.
        P01010le => (true, 8),
        _ => (false, 0),
    }
}

/// Decode `[start, end)` of the video track, reporting each frame's black
/// pixel share against `threshold`.
///
/// The luma plane is read as stored — limited range stays limited, PQ stays PQ
/// — which is what ffmpeg's `blackframe` filter measures too. Anything deeper
/// than 8 bits is shifted down to the 0-255 scale the threshold is written in.
pub fn luma_window(media: &Media, start: f64, end: f64, threshold: u8) -> Result<Vec<LumaFrame>> {
    kahawai_media::init()?;
    let caps = gst::Caps::from_str(LUMA_FORMATS).context("luma caps")?;
    let (pipeline, sink, missing) = open(media, None, &["video/"], &["videoconvert"], Some(caps))?;

    let mut frames = Vec::new();
    drain(
        &pipeline,
        &sink,
        &missing,
        start - LEAD_IN,
        end,
        ACCURATE,
        |sample| {
            let (Some(buffer), Some(caps)) = (sample.buffer(), sample.caps()) else {
                return Ok(());
            };
            // The seek landed on the keyframe before the window; those frames are
            // the decoder finding its feet, not content.
            if buffer
                .pts()
                .map(|p| p.nseconds() as f64 / 1e9)
                .unwrap_or(0.0)
                < start
            {
                return Ok(());
            }
            let info = gstreamer_video::VideoInfo::from_caps(caps).context("video caps")?;
            let frame = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
                .context("mapping a video frame")?;
            let (w, h) = (frame.width() as usize, frame.height() as usize);
            let stride = frame.plane_stride()[0] as usize;
            let luma = frame.plane_data(0).context("luma plane")?;
            let (wide, shift) = luma_shift(info.format());

            let (mut black, mut total) = (0usize, 0u64);
            for row in 0..h {
                let row_start = row * stride;
                let values: Vec<u8> = if wide {
                    luma[row_start..row_start + w * 2]
                        .chunks_exact(2)
                        .map(|b| (u16::from_le_bytes([b[0], b[1]]) >> shift).min(255) as u8)
                        .collect()
                } else {
                    luma[row_start..row_start + w].to_vec()
                };
                black += values.iter().filter(|&&v| v < threshold).count();
                total += values.iter().map(|&v| v as u64).sum::<u64>();
            }

            frames.push(LumaFrame {
                time: buffer
                    .pts()
                    .map(|p| p.nseconds() as f64 / 1e9)
                    .unwrap_or(f64::NAN),
                black_percentage: 100.0 * black as f64 / (w * h) as f64,
                mean_luma: total as f64 / (w * h) as f64,
            });
            Ok(())
        },
    )?;

    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn init() {
        kahawai_media::init().unwrap();
    }

    #[test]
    fn an_audio_probe_stops_video_before_its_decoder() {
        init();
        let video = gst::Caps::builder("video/x-h265").build();
        let audio = gst::Caps::builder("audio/x-eac3").build();
        assert!(!should_autoplug(&video, &["audio/"]));
        assert!(should_autoplug(&audio, &["audio/"]));
    }

    #[test]
    fn a_video_probe_stops_audio_before_its_decoder() {
        init();
        let video = gst::Caps::builder("video/x-h264").build();
        let audio = gst::Caps::builder("audio/x-ac3").build();
        assert!(should_autoplug(&video, &["video/"]));
        assert!(!should_autoplug(&audio, &["video/"]));
    }

    #[test]
    fn container_caps_keep_autoplugging_until_tracks_exist() {
        init();
        for name in ["video/x-matroska", "video/quicktime", "application/ogg"] {
            let caps = gst::Caps::builder(name).build();
            assert!(should_autoplug(&caps, &["audio/"]), "{name}");
            assert!(should_autoplug(&caps, &["video/"]), "{name}");
        }
        let mpeg = gst::Caps::builder("video/mpeg")
            .field("systemstream", true)
            .build();
        assert!(should_autoplug(&mpeg, &["audio/"]));
    }
}
