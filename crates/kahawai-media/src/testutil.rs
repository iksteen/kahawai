//! Test-only fixture generation (used by kahawai-media and kahawai-hub
//! integration tests). Not part of the public API.

use std::path::Path;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Render a short MP4 (moov atom at the end — mp4mux default) with
/// h264 + AAC.
pub fn render_h264_aac_mp4(path: &Path) {
    render_av(path, "mp4mux");
}

/// Render a short MKV with TS-compatible streams: h264 (I420) + AAC.
pub fn render_h264_aac_mkv(path: &Path) {
    render_av(path, "matroskamux");
}

/// Render a short MKV with h264 video and E-AC-3 audio (not TS-muxable —
/// exercises the audio transcode path). Requires avenc_eac3; callers
/// should skip when [`has_element`] says it is missing.
pub fn render_h264_eac3_mkv(path: &Path) {
    render(&format!(
        "videotestsrc num-buffers=250 ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! x264enc bframes=3 b-adapt=false key-int-max=25 ! h264parse ! matroskamux name=m audiotestsrc num-buffers=430 ! audioconvert ! avenc_eac3 ! m. m. ! filesink location=\"{}\"",
        path.display()
    ));
}

/// Render a short MKV with h264 video and FLAC audio (web target plans
/// audio as Encode — flacenc/flacdec ship with gst-plugins-good, so this
/// fixture needs no optional packages).
pub fn render_h264_flac_mkv(path: &Path) {
    render(&format!(
        "videotestsrc num-buffers=125 ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! x264enc bframes=3 b-adapt=false key-int-max=25 ! h264parse ! matroskamux name=m audiotestsrc num-buffers=215 ! audioconvert ! flacenc ! m. m. ! filesink location=\"{}\"",
        path.display()
    ));
}

pub fn has_element(name: &str) -> bool {
    crate::init().unwrap();
    gst::ElementFactory::find(name).is_some()
}

fn render_av(path: &Path, muxer: &str) {
    render(&format!(
        "videotestsrc num-buffers=250 ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! x264enc bframes=3 b-adapt=false key-int-max=25 ! h264parse ! {muxer} name=m audiotestsrc num-buffers=430 ! audioconvert ! fdkaacenc ! m. m. ! filesink location=\"{}\"",
        path.display()
    ));
}

pub fn render(launch: &str) {
    crate::init().unwrap();
    let p = gst::parse::launch(launch).unwrap();
    p.set_state(gst::State::Playing).unwrap();
    let bus = p.bus().unwrap();
    let msg = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(30),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("fixture pipeline stalled");
    assert_eq!(msg.type_(), gst::MessageType::Eos, "fixture pipeline failed");
    p.set_state(gst::State::Null).unwrap();
}
