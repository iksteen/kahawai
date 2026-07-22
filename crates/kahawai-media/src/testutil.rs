//! Test-only fixture generation (used by kahawai-media and kahawai-hub
//! integration tests). Not part of the public API.

use std::path::Path;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Render a short MKV with TS-compatible streams: h264 (I420) + AAC.
pub fn render_h264_aac_mkv(path: &Path) {
    crate::init().unwrap();
    let p = gst::parse::launch(&format!(
        "videotestsrc num-buffers=250 ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! x264enc speed-preset=ultrafast key-int-max=25 ! h264parse ! matroskamux name=m audiotestsrc num-buffers=430 ! audioconvert ! fdkaacenc ! m. m. ! filesink location=\"{}\"",
        path.display()
    ))
    .unwrap();
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
