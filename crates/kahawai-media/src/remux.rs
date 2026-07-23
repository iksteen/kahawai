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
        ("audio", "dts") => "audio/x-dts",
        ("audio", "opus") => "audio/x-opus",
        _ => return None,
    })
}

/// `(has_video, has_audio)` for a remux of this source — the single source
/// of truth shared with session planning, so the muxer pads requested
/// up front always match the streams that will actually be linked.
pub fn ts_stream_flags(info: &kahawai_core::media::MediaInfo) -> (bool, bool) {
    let names = ts_muxable_names();
    let has_video = info
        .video
        .iter()
        .any(|v| codec_to_caps_name("video", &v.codec).is_some_and(|n| names.contains(n)));
    let has_audio = info
        .audio
        .iter()
        .any(|a| codec_to_caps_name("audio", &a.codec).is_some_and(|n| names.contains(n)));
    (has_video, has_audio)
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

fn link_parsed_pad(pipe: &gst::Pipeline, waiting: &WaitingPads, pad: &gst::Pad, caps: &gst::Caps) {
    let caps_name = caps.structure(0).map(|s| s.name().to_string()).unwrap_or_default();
    let target = ts_compatible(&caps_name).and_then(|kind| waiting.lock().unwrap().remove(kind));
    match target {
        Some(sinkpad) => {
            // queue: decouples the muxer from parsebin's threads (the
            // aggregator deadlocks without it). Default queue limits
            // (1 MiB / 1 s) are far too small: the HLS sink holds one
            // branch back while waiting for a keyframe-aligned cut on the
            // other, and files with uneven track ends or high bitrates
            // deadlock (corpus-sweep finding). Bound by bytes only —
            // generous enough for real interleave skew, still OOM-safe.
            let queue = gst::ElementFactory::make("queue")
                .property("max-size-bytes", 64u32 * 1024 * 1024)
                .property("max-size-buffers", 0u32)
                .property("max-size-time", 0u64)
                .build()
                .unwrap();
            pipe.add(&queue).unwrap();
            queue.sync_state_with_parent().unwrap();
            let mut tail = queue.clone();
            // queue → parser → timestamper, each present only when it
            // applies; every hop is pure repackaging, no decode.
            for name in [parser_for(caps), timestamper_for(caps)].into_iter().flatten() {
                let el = gst::ElementFactory::make(name).build().unwrap();
                pipe.add(&el).unwrap();
                el.sync_state_with_parent().unwrap();
                tail.link(&el).unwrap();
                tail = el;
            }
            // hlssink3 (≤0.15.3, imp.rs:304) unwraps the PTS of each
            // fragment's first buffer; a PTS-less frame (old AVI streams)
            // aborts the whole process — a Rust panic in an FFI callback
            // cannot unwind. Guard: borrow the DTS, or drop the buffer.
            let src = tail.static_pad("src").unwrap();
            src.add_probe(gst::PadProbeType::BUFFER, |_, info| {
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
            let ok = pad
                .link(&queue.static_pad("sink").unwrap())
                .and_then(|_| src.link(&sinkpad));
            if let Err(e) = ok {
                tracing::warn!(caps = %caps_name, error = %e, "remux: pad link failed");
            }
        }
        None => {
            tracing::info!(caps = %caps_name, "remux: dropping stream (not TS-compatible or duplicate)");
            let fake = gst::ElementFactory::make("fakesink").build().unwrap();
            pipe.add(&fake).unwrap();
            fake.sync_state_with_parent().unwrap();
            pad.link(&fake.static_pad("sink").unwrap()).unwrap();
        }
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

/// Start a remux writing `master.m3u8` + `segment*.ts` into `out_dir`,
/// pulling bytes from `source` on demand (seeks included). `has_video`/
/// `has_audio` come from discovery — the muxer pads must be requested
/// before the pipeline starts, and an unfed pad would stall it.
pub fn start(
    out_dir: &Path,
    has_video: bool,
    has_audio: bool,
    mut source: Box<dyn RemuxSource>,
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
    anyhow::ensure!(has_video || has_audio, "nothing to remux");
    let waiting: WaitingPads = Arc::new(Mutex::new(std::collections::HashMap::new()));
    if has_video {
        let pad = hlssink.request_pad_simple("video").context("requesting video pad")?;
        waiting.lock().unwrap().insert("video", pad);
    }
    if has_audio {
        let pad = hlssink.request_pad_simple("audio").context("requesting audio pad")?;
        waiting.lock().unwrap().insert("audio", pad);
    }

    // Link parsed elementary streams to the pre-requested pads; one per
    // kind, linked synchronously inside pad-added so no buffer ever hits an
    // unlinked pad. Pad caps may not be set yet at this point, but the
    // GstStream parsebin attaches to the pad already knows them.
    let pipe = pipeline.clone();
    let waiting2 = waiting.clone();
    parsebin.connect_pad_added(move |_, pad| {
        let caps = pad
            .stream()
            .and_then(|s| s.caps())
            .or_else(|| pad.current_caps())
            .unwrap_or_else(gst::Caps::new_empty);
        link_parsed_pad(&pipe, &waiting2, pad, &caps);
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

    /// Manual repro: REMUX_SRC=/path/to/file cargo test -p kahawai-media \
    ///   remux_file_from_env -- --ignored --nocapture
    #[test]
    #[ignore]
    fn remux_file_from_env() {
        let src = std::path::PathBuf::from(std::env::var("REMUX_SRC").expect("set REMUX_SRC"));
        let out = tempfile::tempdir().unwrap();
        let info = crate::discover(&src, Duration::from_secs(30)).unwrap();
        let (has_video, has_audio) = ts_stream_flags(&info);
        eprintln!("flags: video={has_video} audio={has_audio}");
        let job = start(
            out.path(),
            has_video,
            has_audio,
            Box::new(FileSource::open(&src).unwrap()),
        )
        .unwrap();
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
        let (v, a) = ts_stream_flags(&info);
        assert!(v, "hevc is TS-muxable");
        assert_eq!(a, names.contains("audio/x-eac3"));
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
        let job = start(out.path(), true, true, Box::new(FileSource::open(&src_path).unwrap())).unwrap();
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
        let job = start(
            out.path(),
            true,
            true,
            Box::new(FileSource::open(&src_path).unwrap()),
        )
        .unwrap();
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
        let job = start(
            out.path(),
            true,
            true,
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
