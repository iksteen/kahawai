//! Test-only fixture generation (used by kahawai-media and kahawai-hub
//! integration tests). Not part of the public API.

use std::io::Write;
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

/// Render a short black MKV carrying an EMBEDDED ASS subtitle track —
/// the shape a real anime release has, and the only way to exercise the
/// demuxer-pad burn path (HUB-32a), which is the one that also carries
/// attached fonts. `events` are `(start_ms, end_ms, text)`.
///
/// The script is fed through an `appsrc` because no element turns a
/// subtitle file into the `application/x-ass` stream matroskamux wants.
/// It is pushed while the pipeline is still NULL on purpose: the buffers
/// queue in the appsrc and go out once everything is active, so nothing
/// races an inactive pad.
pub fn render_h264_ass_mkv(path: &Path, header: &str, events: &[(u64, u64, String)]) {
    crate::init().unwrap();
    let pipe = gst::parse::launch(&format!(
        "videotestsrc num-buffers=50 pattern=black ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 \
         ! x264enc speed-preset=ultrafast key-int-max=25 bframes=0 ! h264parse ! matroskamux name=m \
         ! filesink location=\"{}\"",
        path.display()
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let mux = pipe.by_name("m").unwrap();
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
    src.static_pad("src")
        .unwrap()
        .link(&mux.request_pad_simple("subtitle_%u").unwrap())
        .unwrap();
    for (start, end, line) in events {
        let mut buf = gst::Buffer::from_slice(line.clone().into_bytes());
        let b = buf.get_mut().unwrap();
        b.set_pts(gst::ClockTime::from_mseconds(*start));
        b.set_duration(gst::ClockTime::from_mseconds(end.saturating_sub(*start)));
        src.push_buffer(buf).unwrap();
    }
    src.end_of_stream().unwrap();
    pipe.set_state(gst::State::Playing).unwrap();
    let bus = pipe.bus().unwrap();
    let msg = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(60),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    pipe.set_state(gst::State::Null).unwrap();
    assert!(
        matches!(msg.map(|m| m.type_()), Some(gst::MessageType::Eos)),
        "ASS fixture did not render"
    );
}

pub fn has_element(name: &str) -> bool {
    require_elements(&[name])
}

/// Gate a media test on a runtime prerequisite. The normal distro-stack
/// suite reports and skips unavailable paths; the pinned release suite sets
/// `KAHAWAI_MEDIA_TEST_STRICT=1`, turning that same absence into a failure.
pub fn require(available: bool, description: &str) -> bool {
    if available {
        return true;
    }
    missing_prerequisite(description, strict_mode(), skip_report().as_deref());
    false
}

pub fn require_elements(names: &[&str]) -> bool {
    crate::init().unwrap();
    let missing: Vec<_> = names
        .iter()
        .copied()
        .filter(|name| gst::ElementFactory::find(name).is_none())
        .collect();
    require(
        missing.is_empty(),
        &format!("GStreamer element(s): {}", missing.join(", ")),
    )
}

/// Raw availability query for compound prerequisites that should produce one
/// combined strict-mode error rather than one message per operand.
pub fn elements_available(names: &[&str]) -> bool {
    crate::init().unwrap();
    names
        .iter()
        .all(|name| gst::ElementFactory::find(name).is_some())
}

pub fn require_h264_aac_fixture() -> bool {
    require(
        elements_available(&["x264enc"]) && crate::remux::aac_encoder().is_some(),
        "x264enc and a verified AAC encoder for generated fixtures",
    )
}

/// Record a passing-but-inapplicable regression without treating it as a
/// missing runtime prerequisite. This remains allowed in strict mode.
pub fn not_applicable(description: &str) {
    report("NOT APPLICABLE", description, skip_report().as_deref());
}

fn strict_mode() -> bool {
    std::env::var_os("KAHAWAI_MEDIA_TEST_STRICT").is_some_and(|v| v == "1")
}

fn skip_report() -> Option<std::path::PathBuf> {
    std::env::var_os("KAHAWAI_MEDIA_SKIP_FILE").map(Into::into)
}

fn missing_prerequisite(description: &str, strict: bool, report_path: Option<&Path>) {
    if strict {
        panic!("required media prerequisite unavailable: {description}");
    }
    report("SKIP", description, report_path);
}

fn report(kind: &str, description: &str, report_path: Option<&Path>) {
    eprintln!("{kind}: {description}");
    if let Some(path) = report_path {
        // Tests report from several libtest worker threads. A single
        // formatted writeln can become several writes, so O_APPEND alone
        // does not keep records intact.
        static REPORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = REPORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap_or_else(|e| panic!("opening media skip report {}: {e}", path.display()));
        writeln!(file, "{kind}: {description}").unwrap();
    }
}

/// 5.1 fixture: h264 + 6-channel AAC, for the HUB-15 channel ceiling
/// (a stereo client must get stereo, not the range's mono).
pub fn render_h264_aac51_mkv(path: &Path) {
    render(&format!(
        "videotestsrc num-buffers=50 ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! x264enc ! h264parse ! matroskamux name=m audiotestsrc num-buffers=90 ! audio/x-raw,channels=6,channel-mask=(bitmask)0x3f,rate=48000 ! audioconvert ! fdkaacenc ! aacparse ! m. m. ! filesink location=\"{}\"",
        path.display()
    ));
}

/// HDR10 fixture (HUB-15a): HEVC Main-10, PQ colorimetry — probes as
/// hdr10. Caller gates on `has_element("x265enc")`.
pub fn render_pq_hevc_mkv(path: &Path) {
    render(&format!(
        "videotestsrc num-buffers=75 ! video/x-raw,format=I420_10LE,width=320,height=240,framerate=25/1,colorimetry=bt2100-pq ! x265enc bitrate=500 speed-preset=ultrafast key-int-max=25 ! h265parse ! matroskamux name=m audiotestsrc num-buffers=130 ! audioconvert ! fdkaacenc ! m. m. ! filesink location=\"{}\"",
        path.display()
    ));
}

fn render_av(path: &Path, muxer: &str) {
    let aac = crate::remux::aac_encoder().expect("fixture requires a verified AAC encoder");
    render(&format!(
        "videotestsrc num-buffers=250 ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! x264enc bframes=3 b-adapt=false key-int-max=25 ! h264parse ! {muxer} name=m audiotestsrc num-buffers=430 ! audioconvert ! {aac} ! m. m. ! filesink location=\"{}\"",
        path.display()
    ));
}

/// Render a short AVI with h264 video and MP3 audio — the shape that
/// exposes DTS-only video. avidemux hands out h264-in-AVI with no PTS
/// on most buffers (only a DTS), because AVI carries no per-frame
/// presentation times. Needs lamemp3enc; callers should skip when
/// [`has_element`] says it is missing.
pub fn render_h264_mp3_avi(path: &Path, frames: u32) {
    render(&format!(
        "videotestsrc num-buffers={frames} ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! x264enc bframes=3 b-adapt=false key-int-max=25 ! h264parse ! avimux name=m audiotestsrc num-buffers={audio} ! audioconvert ! lamemp3enc ! m. m. ! filesink location=\"{}\"",
        path.display(),
        frames = frames,
        audio = frames * 43 / 25,
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
    assert_eq!(
        msg.type_(),
        gst::MessageType::Eos,
        "fixture pipeline failed"
    );
    p.set_state(gst::State::Null).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "required media prerequisite unavailable")]
    fn strict_prerequisites_fail_instead_of_skipping() {
        missing_prerequisite("deliberately absent", true, None);
    }

    #[test]
    fn best_effort_prerequisites_leave_a_machine_readable_record() {
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("skips.txt");
        missing_prerequisite("deliberately absent", false, Some(&report));
        assert_eq!(
            std::fs::read_to_string(report).unwrap(),
            "SKIP: deliberately absent\n"
        );
    }

    #[test]
    fn concurrent_prerequisites_leave_whole_report_records() {
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("skips.txt");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let threads: Vec<_> = (0..16)
            .map(|i| {
                let report = report.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    missing_prerequisite(&format!("prerequisite {i}"), false, Some(&report));
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }

        let mut actual: Vec<_> = std::fs::read_to_string(report)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        actual.sort();
        let mut expected: Vec<_> = (0..16).map(|i| format!("SKIP: prerequisite {i}")).collect();
        expected.sort();
        assert_eq!(actual, expected);
    }
}
