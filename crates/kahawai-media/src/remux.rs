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

/// Streams mpegtsmux can carry without re-encoding.
fn ts_compatible(caps_name: &str) -> Option<&'static str> {
    match caps_name {
        "video/x-h264" | "video/x-h265" => Some("video"),
        "audio/mpeg" | "audio/x-ac3" | "audio/x-eac3" => Some("audio"),
        _ => None,
    }
}

/// hlssink2 pads requested up front (splitmuxsink wants them before start);
/// each is taken by the first matching parsed stream.
type WaitingPads = Arc<Mutex<std::collections::HashMap<&'static str, gst::Pad>>>;

fn link_parsed_pad(pipe: &gst::Pipeline, waiting: &WaitingPads, pad: &gst::Pad, caps_name: &str) {
    let target = ts_compatible(caps_name).and_then(|kind| waiting.lock().unwrap().remove(kind));
    match target {
        Some(sinkpad) => {
            // A queue per stream decouples the muxer from parsebin's
            // threads — without it the aggregator deadlocks.
            let queue = gst::ElementFactory::make("queue").build().unwrap();
            pipe.add(&queue).unwrap();
            queue.sync_state_with_parent().unwrap();
            let ok = pad
                .link(&queue.static_pad("sink").unwrap())
                .and_then(|_| queue.static_pad("src").unwrap().link(&sinkpad));
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

pub struct RemuxJob {
    pipeline: gst::Pipeline,
    appsrc: AppSrc,
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
    tracing::info!(sink = name, "HLS sink selected");
    Ok((sink, name))
}

/// Start a remux writing `master.m3u8` + `segment*.ts` into `out_dir`.
/// `has_video`/`has_audio` come from discovery — the muxer pads must be
/// requested before the pipeline starts, and an unfed pad would stall it.
/// Feed source-container bytes with [`RemuxJob::push`], then [`RemuxJob::finish`].
pub fn start(out_dir: &Path, has_video: bool, has_audio: bool) -> Result<RemuxJob> {
    crate::init()?;

    let pipeline = gst::Pipeline::new();
    let appsrc = AppSrc::builder()
        .stream_type(gstreamer_app::AppStreamType::Stream)
        .block(true)
        .max_bytes(8 * 1024 * 1024)
        .build();
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
        let caps_name = pad
            .stream()
            .and_then(|s| s.caps())
            .or_else(|| pad.current_caps())
            .and_then(|c| c.structure(0).map(|s| s.name().to_string()))
            .unwrap_or_default();
        link_parsed_pad(&pipe, &waiting2, pad, &caps_name);
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
    Ok(RemuxJob { pipeline, appsrc, error, finished })
}

impl RemuxJob {
    /// Push source bytes. Blocks (appsrc backpressure) — call off the
    /// async runtime. Errors once the pipeline has failed.
    pub fn push(&self, data: Vec<u8>) -> Result<()> {
        if let Some(e) = self.error.lock().unwrap().clone() {
            anyhow::bail!("remux failed: {e}");
        }
        self.appsrc
            .push_buffer(gst::Buffer::from_mut_slice(data))
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("appsrc push: {e}"))
    }

    /// Signal end of input; the pipeline finalizes the playlist.
    pub fn finish(&self) {
        let _ = self.appsrc.end_of_stream();
    }

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
        let job = start(out.path(), true, true).unwrap();
        let data = std::fs::read(&src_path).unwrap();
        for chunk in data.chunks(64 * 1024) {
            job.push(chunk.to_vec()).unwrap();
        }
        job.finish();

        let deadline = Instant::now() + Duration::from_secs(30);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(job.finished(), "remux did not finish in time");
        assert!(job.failed().is_none(), "remux failed: {:?}", job.failed());

        let playlist = std::fs::read_to_string(out.path().join("master.m3u8")).unwrap();
        assert!(playlist.contains("segment00000.ts"), "playlist:\n{playlist}");
        assert!(playlist.contains("#EXT-X-ENDLIST"), "playlist not finalized");

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
    }
}
