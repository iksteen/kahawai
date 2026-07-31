//! HUB-15b fMP4 segment sink: `isofmp4mux` + an appsink that writes
//! `init.mp4`, `segment%05d.m4s` and an EVENT `master.m3u8` — the same
//! on-disk shape the TS sinks produce, so serving, readiness gates
//! (Σ EXTINF), pacing and seek-restart semantics carry over untouched.
//!
//! Built by hand instead of `hlscmafsink` because CMAF is one track
//! per stream: the ready-made sink would force two segment families
//! plus a multivariant playlist with alternate-audio renditions — and
//! still only accepts h264/h265/aac. `isofmp4mux` muxes video+audio
//! into ONE fragment stream and additionally carries av1/vp9/opus,
//! which is the entire point of the fMP4 path (TS cannot).
//!
//! Muxer output contract (gst-plugins-rs `mux/isobmff` examples,
//! verified against the shipped 1.28 plugin): every sample is a full
//! fragment as a `BufferList`; a first buffer flagged DISCONT|HEADER
//! is the media header (`ftyp`+`moov` → `init.mp4`), possibly alone in
//! its list; a fragment's first buffer is flagged HEADER (the `moof`)
//! and carries the fragment's `duration()` — the EXTINF value.
//!
//! Fragments close at the first keyframe after `fragment-duration`
//! (2 s), exactly like the TS sinks split segments — GOP 48 encodes
//! and source keyframes on copies both land ~2 s segments, so the
//! session-start gate behaves identically. `send-force-keyunit` stays
//! off: extra requested keyunits would fight the encoder's own GOP.

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

struct WriterState {
    out_dir: PathBuf,
    init_written: bool,
    /// (basename, duration in seconds) per finished segment.
    segments: Vec<(String, f64)>,
    ended: bool,
}

impl WriterState {
    /// Rewrite the playlist atomically (tmp + rename): a reader — the
    /// hub's readiness gate, hls.js — always sees a complete file.
    fn write_playlist(&self) -> std::io::Result<()> {
        let mut out = String::from(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:3\n\
             #EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:EVENT\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n",
        );
        for (name, dur) in &self.segments {
            out.push_str(&format!("#EXTINF:{dur:.3},\n{name}\n"));
        }
        if self.ended {
            out.push_str("#EXT-X-ENDLIST\n");
        }
        let tmp = self.out_dir.join(".master.m3u8.tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(tmp, self.out_dir.join("master.m3u8"))
    }
}

/// Add the fMP4 sink pair (mux + writer appsink) to the pipeline and
/// return the MUX — the element the session requests `sink_%u` pads
/// from (video first = track 0, matching the request order upstream).
pub fn attach(pipeline: &gst::Pipeline, out_dir: &Path) -> Result<gst::Element> {
    let mux = gst::ElementFactory::make("isofmp4mux")
        .property("fragment-duration", gst::ClockTime::from_seconds(2))
        .build()
        .context("isofmp4mux missing — see `kahawai doctor` (mux fmp4/cmaf)")?;
    let appsink = gstreamer_app::AppSink::builder()
        .buffer_list(true)
        // Not a live consumer: never let the sink pace the pipeline —
        // the byte-plane pace probes own that, same as the TS path.
        .sync(false)
        .build();
    appsink.set_property("async", false);

    let state = Mutex::new(WriterState {
        out_dir: out_dir.to_path_buf(),
        init_written: false,
        segments: Vec::new(),
        ended: false,
    });
    let state = std::sync::Arc::new(state);
    let eos_state = state.clone();
    appsink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let Some(mut list) = sample.buffer_list_owned() else {
                    return Ok(gst::FlowSuccess::Ok);
                };
                if list.is_empty() {
                    return Ok(gst::FlowSuccess::Ok);
                }
                let mut st = state.lock().unwrap();
                let mut first = list.get(0).unwrap();
                // Media header (ftyp+moov): write init.mp4 once. With
                // header-update-mode at its default there is no updated
                // header at EOS to overwrite it.
                if first
                    .flags()
                    .contains(gst::BufferFlags::DISCONT | gst::BufferFlags::HEADER)
                {
                    if !st.init_written {
                        let map = first.map_readable().map_err(|_| gst::FlowError::Error)?;
                        if std::fs::write(st.out_dir.join("init.mp4"), &map).is_err() {
                            return Err(gst::FlowError::Error);
                        }
                        st.init_written = true;
                    }
                    list.make_mut().remove(0..1);
                    if list.is_empty() {
                        return Ok(gst::FlowSuccess::Ok);
                    }
                    first = list.get(0).unwrap();
                }
                // One fragment: moof (HEADER-flagged, carries the
                // fragment duration) + mdat buffers, all into one file.
                let dur = first
                    .duration()
                    .map(|d| d.nseconds() as f64 / 1e9)
                    .unwrap_or(2.0);
                let name = format!("segment{:05}.m4s", st.segments.len());
                let write = || -> std::io::Result<()> {
                    let mut f = std::fs::File::create(st.out_dir.join(&name))?;
                    for buffer in &*list {
                        let map = buffer
                            .map_readable()
                            .map_err(|_| std::io::Error::other("unmappable buffer"))?;
                        f.write_all(&map)?;
                    }
                    Ok(())
                };
                if let Err(e) = write() {
                    tracing::warn!(error = %e, segment = %name, "fmp4 segment write failed");
                    return Err(gst::FlowError::Error);
                }
                st.segments.push((name, dur));
                if let Err(e) = st.write_playlist() {
                    tracing::warn!(error = %e, "fmp4 playlist write failed");
                    return Err(gst::FlowError::Error);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .eos(move |_| {
                let mut st = eos_state.lock().unwrap();
                st.ended = true;
                if let Err(e) = st.write_playlist() {
                    tracing::warn!(error = %e, "fmp4 playlist ENDLIST write failed");
                }
            })
            .build(),
    );

    pipeline.add_many([&mux, appsink.upcast_ref::<gst::Element>()])?;
    mux.link(appsink.upcast_ref::<gst::Element>())
        .context("linking isofmp4mux to its writer")?;
    Ok(mux)
}
