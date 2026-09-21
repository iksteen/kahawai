#[test]
fn mp4_vobsub_clut_event_becomes_rgb_palette() {
    crate::init().unwrap();
    let values = [
        0x0010_8080u32,
        0x00e9_8181,
        0x00bf_8080,
        0x0093_8080,
        0x0048_d779,
        0x0029_cb7a,
        0x0060_6dd7,
        0x003e_70cb,
        0x00b6_3e32,
        0x0087_473d,
        0x00da_2a88,
        0x00a6_3687,
        0x006c_c3cf,
        0x0048_bac4,
        0x00c2_942a,
        0x0092_9136,
    ];
    let mut structure = gst::Structure::new_empty("application/x-gst-dvd");
    structure.set("event", "dvd-spu-clut-change");
    for (i, value) in values.into_iter().enumerate() {
        structure.set(format!("clut{i:02}"), value as i32);
    }
    let event = gst::event::CustomDownstream::new(structure);
    let palette = super::vobsub_palette_event(&event).expect("DVD CLUT event");
    assert_eq!(palette.len(), 16);
    assert_eq!(palette[0], [0, 0, 0]);
    assert_eq!(palette[4], [204, 0, 51]);
}

#[test]
fn hd_authored_vobsub_uses_video_canvas_without_overriding_idx_size() {
    let hd = crate::imagesubs::ImageObject {
        x: 752,
        y: 971,
        w: 417,
        h: 64,
        rgba: Vec::new(),
    };
    assert_eq!(
        super::vobsub_canvas(None, Some((1920, 1036)), &hd),
        (1920, 1036)
    );
    assert_eq!(
        super::vobsub_canvas(Some((720, 480)), Some((1920, 1036)), &hd),
        (720, 480),
        "an explicit idx canvas always wins"
    );
    let dvd = crate::imagesubs::ImageObject {
        x: 150,
        y: 420,
        w: 420,
        h: 48,
        rgba: Vec::new(),
    };
    assert_eq!(
        super::vobsub_canvas(None, Some((1920, 1080)), &dvd),
        (720, 576),
        "DVD-coordinate subtitles keep their DVD scale"
    );
}

/// A gfx1200 radeonsi HEVC encoder advertises width >= 384. The old
/// universal 320x240 probe therefore rejected working hardware before a
/// session could use it. Probe dimensions come from the encoder's own caps:
/// the chosen size must be ordinary, accepted, and not the rejected 320px.
#[test]
fn video_probe_respects_driver_dimension_limits() {
    crate::init().unwrap();
    let amd_hevc = gst::Caps::builder("video/x-raw")
        .field("width", gst::IntRange::new(384i32, 8192))
        .field("height", gst::IntRange::new(128i32, 4352))
        .build();
    assert!(!super::video_caps_accept_dimensions(&amd_hevc, 320, 240));
    let dimensions =
        super::video_probe_dimensions_from_caps(&amd_hevc).expect("usable HEVC dimensions");
    assert_eq!(dimensions, (640, 480));
    assert!(super::video_caps_accept_dimensions(
        &amd_hevc,
        dimensions.0,
        dimensions.1
    ));
}

/// TC-6: the thread ceiling has to LAND, on every software encoder
/// this box actually has.
///
/// The failure mode it exists for is silence: `set_prop_str_if_present`
/// skips a property that is not there, so one wrong name in the match
/// means the ceiling quietly does nothing and the transcode still
/// completes, looking identical. Reading the value back is the only
/// thing that distinguishes "applied" from "ignored".
///
/// Elements absent from this box are skipped rather than failed — the
/// same shape the sink tests use — and the count is asserted so a
/// build with no software encoders at all cannot pass vacuously.
#[test]
fn the_thread_ceiling_lands_on_every_software_encoder_present() {
    use gst::glib::prelude::ObjectExt;
    crate::init().unwrap();
    // Property names are from `gst-inspect` per element, not memory.
    // x265enc carries no thread property at all — libx265's
    // `option-string` is the only way in, and `pools` is its
    // total-thread knob.
    let cases: &[(&str, &str, &str)] = &[
        ("x264enc", "threads", "3"),
        ("openh264enc", "multi-thread", "3"),
        ("av1enc", "threads", "3"),
        ("rav1enc", "threads", "3"),
        // Resolved below: GStreamer 1.28 uses `logical-processors`,
        // while releases built against SVT-AV1 3 use
        // `level-of-parallelism`.
        ("svtav1enc", "", "3"),
        ("x265enc", "option-string", "pools=3"),
    ];
    let mut checked = 0;
    for (name, configured_prop, want) in cases {
        let Ok(enc) = gst::ElementFactory::make(name).build() else {
            crate::testutil::not_applicable(&format!(
                "optional software encoder {name} is not installed"
            ));
            continue;
        };
        let prop = if *name == "svtav1enc" {
            super::svtav1_thread_property(&enc)
                .expect("svtav1enc has no recognized thread-control property")
        } else {
            *configured_prop
        };
        super::set_encoder_threads_on(&enc, name, 3);
        let v = enc.property_value(prop);
        // The numeric properties are variously uint and int across
        // these elements, so compare what the value SAYS rather than
        // guessing a Rust type per element.
        let got = v
            .get::<u32>()
            .map(|n| n.to_string())
            .or_else(|_| v.get::<i32>().map(|n| n.to_string()))
            .or_else(|_| v.get::<String>())
            .unwrap_or_else(|_| format!("{v:?}"));
        assert_eq!(
            &got, want,
            "{name}.{prop} was not set — wrong property name in the match?"
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "no software encoder present at all; the ceiling was never exercised"
    );
    // A hardware encoder has no such knob and must be left alone
    // rather than panicked over.
    if let Ok(enc) = gst::ElementFactory::make("vah264enc").build() {
        super::set_encoder_threads_on(&enc, "vah264enc", 3);
    }
}

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
        crate::testutil::require(false, "verified AAC encoder");
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
    stereo_gain_db: None,
    native_gain_db: None,
    loudness_source_channels: None,
    loudness_gains: [None; crate::loudness::MAX_LAYOUT_GAINS],
    tone_map: false,
    deinterlace: false,
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
    let has_fmp4 = crate::testutil::require_elements(&["isofmp4mux"]);
    for label in ["mp3", "mpeg-audio"] {
        let c = codec_to_caps("audio", label).unwrap();
        assert!(
            caps_muxable(&c, SegmentFormat::Ts),
            "{label} is MPEG-1 audio, which mpegtsmux carries"
        );
        if has_fmp4 {
            assert!(
                !caps_muxable(&c, SegmentFormat::Fmp4),
                "{label} must not plan as an fMP4 copy"
            );
        }
    }
    // AAC is the same caps name and must stay muxable in both.
    let aac = codec_to_caps("audio", "aac").unwrap();
    assert!(caps_muxable(&aac, SegmentFormat::Ts));
    if has_fmp4 {
        assert!(caps_muxable(&aac, SegmentFormat::Fmp4));
    }

    // The worker sees full caps off the demuxer, not a label.
    let live_mp3 = gst::Caps::builder("audio/mpeg")
        .field("mpegversion", 1i32)
        .field("layer", 3i32)
        .field("rate", 48000i32)
        .field("channels", 2i32)
        .build();
    assert_eq!(sink_compatible(&live_mp3, SegmentFormat::Ts), Some("audio"));
    if has_fmp4 {
        assert_eq!(sink_compatible(&live_mp3, SegmentFormat::Fmp4), None);
    }

    // The check must NOT tighten on fields a parser converts:
    // h264 arrives byte-stream/nal from AVI and leaves h264parse as
    // avc/au, which is what isofmp4mux's template demands.
    let live_h264 = gst::Caps::builder("video/x-h264")
        .field("stream-format", "byte-stream")
        .field("alignment", "nal")
        .build();
    if has_fmp4 {
        assert_eq!(
            sink_compatible(&live_h264, SegmentFormat::Fmp4),
            Some("video")
        );
    }
    assert_eq!(
        sink_compatible(&live_h264, SegmentFormat::Ts),
        Some("video")
    );
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
    assert_eq!(
        sink_compatible(&caps("text/x-raw"), SegmentFormat::Ts),
        None
    );
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
    if !crate::testutil::require_elements(&["x264enc"]) {
        return;
    }
    let Some(aac) = aac_encoder() else {
        crate::testutil::require(false, "verified AAC encoder");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("uneven.mkv");
    let p = gst::parse::launch(&format!(
        "videotestsrc num-buffers=100 ! video/x-raw,format=I420,width=320,height=240 ! x264enc speed-preset=ultrafast ! h264parse ! matroskamux name=m audiotestsrc num-buffers=200 ! audioconvert ! {aac} ! m. m. ! filesink location=\"{}\"",
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
    if !crate::testutil::require_elements(&["isofmp4mux", "x264enc", "flacenc"]) {
        return;
    }
    // The premise: if this ever gains a parser, the test below stops
    // covering what it claims to.
    for name in ["audio/x-flac", "audio/x-vorbis", "video/x-vp8"] {
        let caps = gst::Caps::builder(name).build();
        assert!(
            parser_for(&caps, SegmentFormat::Fmp4).is_none(),
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

/// A source smaller than one read block must still demux.
///
/// gst_avi_demux_chain() advances one state per BUFFER: START
/// parses the RIFF header and returns, HEADER waits for the next
/// buffer. Hand it the whole file at once — which the ring did for
/// anything under READ_BLOCK — and it never leaves the header, then
/// errors at EOS with "didn't receive a complete header object".
/// Proven independent of kahawai: the same bytes through a bare
/// appsrc fail as one buffer and succeed as two.
#[test]
fn a_source_smaller_than_one_read_block_still_demuxes() {
    crate::init().unwrap();
    if !crate::testutil::require_elements(&["lamemp3enc", "x264enc"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("tiny.avi");
    crate::testutil::render_h264_mp3_avi(&src_path, 250);
    let size = std::fs::metadata(&src_path).unwrap().len();
    assert!(
        size < 2 * 1024 * 1024,
        "fixture grew past a read block ({size} bytes); it no longer covers the case"
    );

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
    assert!(job.finished(), "remux of a sub-block source never finished");
    assert!(job.failed().is_none(), "remux failed: {:?}", job.failed());
    let playlist = std::fs::read_to_string(out.path().join("master.m3u8")).unwrap();
    assert!(playlist.contains("#EXTINF"), "no segments: {playlist}");
}

/// A DTS-only video stream must still reach the muxer.
///
/// AVI has no per-frame presentation times, so avidemux emits h264
/// with a DTS and no PTS. h264parse holds such buffers instead of
/// passing them on, so the video pad stays silent forever and
/// splitmuxsink waits on it — audio flows, no segment is ever
/// written. The guard has to run on the way INTO the parser chain,
/// not just on the way out.
#[test]
fn a_dts_only_video_stream_still_reaches_the_muxer() {
    crate::init().unwrap();
    if !crate::testutil::require_elements(&["lamemp3enc", "x264enc"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("dtsonly.avi");
    crate::testutil::render_h264_mp3_avi(&src_path, 1000);

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
    assert!(job.finished(), "remux of a DTS-only stream never finished");
    assert!(job.failed().is_none(), "remux failed: {:?}", job.failed());
    let playlist = std::fs::read_to_string(out.path().join("master.m3u8")).unwrap();
    assert!(
        playlist.contains("#EXTINF"),
        "no segments written — the video pad never produced a buffer: {playlist}"
    );
}

/// parsebin keeps its parser except for AV1 and the separately tested
/// fMP4-ready HEVC shape.
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
fn parsebin_parser_exceptions_stay_narrow() {
    crate::init().unwrap();
    let caps = |n: &str| gst::Caps::builder(n).build();
    assert!(parsebin_must_not_parse(
        &caps("video/x-av1"),
        SegmentFormat::Ts
    ));
    assert!(parsebin_must_not_parse(
        &caps("video/x-av1"),
        SegmentFormat::Fmp4
    ));
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
        for format in [SegmentFormat::Ts, SegmentFormat::Fmp4] {
            assert!(
                !parsebin_must_not_parse(&caps(name), format),
                "{name} would lose parsebin's parser in {format:?}"
            );
        }
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
    if !crate::testutil::require_h264_aac_fixture() {
        return;
    }
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
    if !crate::testutil::require_elements(&["avenc_eac3"]) {
        return;
    }
    if muxable_names(SegmentFormat::Ts).contains("audio/x-eac3") {
        crate::testutil::not_applicable("mpegtsmux accepts E-AC-3 natively");
        return;
    }
    if !crate::testutil::require(aac_encoder().is_some(), "verified AAC encoder") {
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
    if !crate::testutil::require_elements(&["avenc_mpeg4"]) {
        return;
    }
    if !crate::testutil::require(h264_encoder().is_some(), "verified H.264 encoder") {
        return;
    }
    let Some(aac) = aac_encoder() else {
        crate::testutil::require(false, "verified AAC encoder");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("divx.mkv");
    crate::testutil::render(&format!(
        "videotestsrc num-buffers=125 ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! avenc_mpeg4 ! matroskamux name=m audiotestsrc num-buffers=215 ! audioconvert ! {aac} ! m. m. ! filesink location=\"{}\"",
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

/// One requested seek must reach the demuxer once, even when parsebin
/// exposes both audio and video. Broadcasting through the bin races the
/// duplicate against the demuxer's asynchronous index/cluster seeks.
#[test]
fn offset_seek_is_delivered_once() {
    crate::init().unwrap();
    if !crate::testutil::require_h264_aac_fixture() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("in.mkv");
    crate::testutil::render_h264_aac_mkv(&source);
    let pipeline = gst::Pipeline::new();
    let src = seekable_appsrc(Box::new(FileSource::open(&source).unwrap()));
    let parsebin = gst::ElementFactory::make("parsebin").build().unwrap();
    pipeline.add_many([src.upcast_ref(), &parsebin]).unwrap();
    src.link(&parsebin).unwrap();
    let deliveries = Arc::new(Mutex::new(Vec::new()));
    let observed = deliveries.clone();
    let weak = pipeline.downgrade();
    let (ready, pads_ready) = std::sync::mpsc::channel();
    parsebin.connect_pad_added(move |_, pad| {
        let observed = observed.clone();
        pad.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |pad, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data
                && event.type_() == gst::EventType::Seek
            {
                observed.lock().unwrap().push(pad.name().to_string());
                // Consume at the routing boundary so the test observes
                // every delivery without depending on flush timing.
                return gst::PadProbeReturn::Handled;
            }
            gst::PadProbeReturn::Ok
        });
        let pipeline = weak.upgrade().unwrap();
        let queue = gst::ElementFactory::make("queue").build().unwrap();
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();
        pipeline.add_many([&queue, &sink]).unwrap();
        queue.link(&sink).unwrap();
        pad.link(&queue.static_pad("sink").unwrap()).unwrap();
        sink.sync_state_with_parent().unwrap();
        queue.sync_state_with_parent().unwrap();
        ready.send(()).unwrap();
    });
    pipeline.set_state(gst::State::Paused).unwrap();
    for _ in 0..2 {
        pads_ready.recv_timeout(Duration::from_secs(10)).unwrap();
    }
    let (state, current, _) = pipeline.state(gst::ClockTime::from_seconds(10));
    let pads = parsebin.src_pads().len();
    let accepted = seek_parsed_stream(
        &parsebin,
        gst::event::Seek::new(
            1.0,
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            gst::SeekType::Set,
            gst::ClockTime::from_seconds(6),
            gst::SeekType::None,
            gst::ClockTime::NONE,
        ),
    );
    pipeline.set_state(gst::State::Null).unwrap();
    state.unwrap();
    assert_eq!(current, gst::State::Paused);
    assert_eq!(pads, 2, "fixture must expose both audio and video");
    assert!(accepted);
    assert_eq!(deliveries.lock().unwrap().len(), 1, "one initial seek");
}

/// §6 seek story: starting at an offset produces only the tail.
#[test]
fn starts_at_offset() {
    crate::init().unwrap();
    if !crate::testutil::require_h264_aac_fixture() {
        return;
    }
    for video in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("in.mkv");
        if video {
            crate::testutil::render_h264_aac_mkv(&src_path); // 10 s fixture
        } else {
            crate::testutil::render(&format!(
                "audiotestsrc num-buffers=430 ! audioconvert ! {} ! matroskamux ! filesink location=\"{}\"",
                aac_encoder().unwrap(),
                src_path.display()
            ));
        }

        let out = tempfile::tempdir().unwrap();
        let job = start_at(
            out.path(),
            RemuxPlan {
                video: if video {
                    StreamMode::Copy
                } else {
                    StreamMode::Off
                },
                ..COPY_AV
            },
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
}

/// The crashing combo from the field: offset start + encode branch.
/// splitmuxsink aborts on flushes after data; the seek gate must
/// keep it virgin until the initial seek lands.
#[test]
fn starts_at_offset_with_encode_branch() {
    crate::init().unwrap();
    if !crate::testutil::require(aac_encoder().is_some(), "verified AAC encoder") {
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
    if !crate::testutil::require(
        h264_encoder().is_some() && aac_encoder().is_some(),
        "verified H.264 and AAC encoders",
    ) {
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
    if !crate::testutil::require(
        crate::testutil::elements_available(&["isofmp4mux"]) && h264_encoder().is_some(),
        "isofmp4mux and a verified H.264 encoder",
    ) {
        return;
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
    if !crate::testutil::require(
        crate::testutil::elements_available(&["isofmp4mux"]) && h264_encoder().is_some(),
        "isofmp4mux and a verified H.264 encoder",
    ) {
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
    if !crate::testutil::require_elements(&["isofmp4mux", "x265enc"])
        || !crate::testutil::require_h264_aac_fixture()
    {
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

/// A Dolby Vision profile 8.1 source exposed the already parsed
/// `hvc1/au` caps below. Running it through another h265parse changed
/// its multilayer SPS codec_data, and isofmp4mux then refused the stream
/// with `not-negotiated`. fMP4 needs only the timestamper in this case;
/// TS still needs h265parse to convert the stream to Annex B.
#[test]
fn fmp4_does_not_reparse_packetized_hevc() {
    crate::init().unwrap();
    let ready = gst::Caps::builder("video/x-h265")
        .field("stream-format", "hvc1")
        .field("alignment", "au")
        .field("profile", "main-10")
        .field("width", 3840i32)
        .field("height", 2160i32)
        .build();
    assert!(parsebin_must_not_parse(&ready, SegmentFormat::Fmp4));
    assert!(!parsebin_must_not_parse(&ready, SegmentFormat::Ts));
    assert_eq!(parser_for(&ready, SegmentFormat::Fmp4), None);
    assert_eq!(parser_for(&ready, SegmentFormat::Ts), Some("h265parse"));

    let needs_conversion = gst::Caps::builder("video/x-h265")
        .field("stream-format", "byte-stream")
        .field("alignment", "nal")
        .build();
    assert!(!parsebin_must_not_parse(
        &needs_conversion,
        SegmentFormat::Fmp4
    ));
    assert_eq!(
        parser_for(&needs_conversion, SegmentFormat::Fmp4),
        Some("h265parse")
    );
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
    if !crate::testutil::require(
        h264_encoder().is_some() && crate::testutil::elements_available(&["assrender"]),
        "verified H.264 encoder and assrender",
    ) {
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
    if !crate::testutil::require_elements(&["isofmp4mux", "flacenc"]) {
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

#[test]
fn full_required_loudness_boost_fits_the_volume_property() {
    crate::init().unwrap();
    let volume = gst::ElementFactory::make("volume").build().unwrap();
    set_loudness_volume(&volume, Some(82.0));
    let applied = volume.property::<f64>("volume-full-range");
    let expected = crate::loudness::gain_multiplier(82.0);
    assert!(
        ((applied - expected) / expected).abs() < 1e-6,
        "the measured gain was rejected: {applied}"
    );
}

/// HUB-15 channel ceiling: a client that accepts stereo gets
/// STEREO off a 5.1 source. Range caps fixated to their minimum
/// and delivered mono — invisible until a browser could declare
/// the ceiling (the capability debug mask).
#[test]
fn channel_ceiling_downmixes_to_the_ceiling_not_mono() {
    crate::init().unwrap();
    if !crate::testutil::require(
        h264_encoder().is_some()
            && aac_encoder().is_some()
            && crate::testutil::elements_available(&["fdkaacenc"]),
        "verified H.264/AAC encoders and fdkaacenc",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("in51.mkv");
    crate::testutil::render_h264_aac51_mkv(&src_path);
    let info = crate::discover(&src_path, Duration::from_secs(30)).unwrap();
    assert_eq!(info.audio[0].channels, 6, "fixture must be 5.1: {info:?}");
    let source_layout = crate::loudness::AudioLayout::from_stream(
        info.audio[0].channels,
        info.audio[0].layout.as_deref(),
    );
    let source_loudness =
        crate::loudness::measure_file(&src_path, 0, source_layout, || Ok(())).unwrap();

    let mut plan = RemuxPlan {
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
    plan.loudness_gains[0] = Some(crate::loudness::AudioLayoutGain {
        layout: crate::loudness::AudioLayout::new(2, 0x3),
        gain_db: 6.0,
    });
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
    let output_layout = crate::loudness::AudioLayout::from_stream(
        seg_info.audio[0].channels,
        seg_info.audio[0].layout.as_deref(),
    );
    let output_loudness = crate::loudness::measure_file(&seg, 0, output_layout, || Ok(())).unwrap();
    let raised = output_loudness.get(output_layout).unwrap().integrated_lufs
        - source_loudness
            .get(crate::loudness::AudioLayout::new(2, 0x3))
            .unwrap()
            .integrated_lufs;
    assert!((4.0..=7.5).contains(&raised), "gain was {raised:.2} LU");
    // The fold is also a session fact (AR-13): the supervisor reads
    // these at ready and amends the verdict with what actually
    // happened to the channel count.
    let facts = crate::facts::read(out.path());
    assert!(
        facts
            .iter()
            .any(|fact| fact.kind == "audio" && fact.detail == "5.1 → stereo"),
        "the downmix must be reported: {facts:?}"
    );
    assert!(
        facts
            .iter()
            .any(|fact| fact.kind == "audio" && fact.detail == "loudness +6.00 dB"),
        "the loudness gain must be reported: {facts:?}"
    );
}

#[test]
fn seven_one_to_five_one_uses_the_measured_five_one_gain() {
    crate::init().unwrap();
    if !crate::testutil::require(
        h264_encoder().is_some()
            && aac_encoder().is_some()
            && crate::testutil::elements_available(&["fdkaacenc", "flacenc"]),
        "verified H.264/AAC encoders, fdkaacenc and flacenc",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("in71.mkv");
    let fixture = gst::parse::launch(&format!(
        "videotestsrc num-buffers=50 ! \
         video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! \
         x264enc ! h264parse ! matroskamux name=m \
         audiotestsrc num-buffers=90 volume=0.02 ! \
         audio/x-raw,channels=8,channel-mask=(bitmask)0xc3f,rate=48000 ! \
         audioconvert ! flacenc ! m. m. ! filesink location={}",
        src_path.display()
    ))
    .unwrap();
    fixture.set_state(gst::State::Playing).unwrap();
    let message = fixture.bus().unwrap().timed_pop_filtered(
        gst::ClockTime::from_seconds(30),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    fixture.set_state(gst::State::Null).unwrap();
    assert!(message.is_some_and(|message| message.type_() == gst::MessageType::Eos));

    let source_layout = crate::loudness::AudioLayout::new(8, 0xc3f);
    let five_one = crate::loudness::AudioLayout::new(6, 0x3f);
    let source_loudness =
        crate::loudness::measure_file(&src_path, 0, source_layout, || Ok(())).unwrap();
    assert!(source_loudness.get(five_one).is_some());

    let mut plan = RemuxPlan {
        video: StreamMode::Copy,
        audio: StreamMode::Encode,
        max_channels: Some(6),
        ..Default::default()
    };
    plan.loudness_gains[0] = Some(crate::loudness::AudioLayoutGain {
        layout: five_one,
        gain_db: 4.0,
    });
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
    assert!(job.finished(), "7.1 → 5.1 encode did not finish");
    assert!(job.failed().is_none(), "{:?}", job.failed());
    let seg = std::fs::read_dir(out.path())
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| path.extension().is_some_and(|extension| extension == "ts"))
        .expect("no segment produced");
    let output_info = crate::discover(&seg, Duration::from_secs(30)).unwrap();
    assert_eq!(output_info.audio[0].channels, 6);
    let output_loudness = crate::loudness::measure_file(&seg, 0, five_one, || Ok(())).unwrap();
    let raised = output_loudness
        .get(output_loudness.source)
        .unwrap()
        .integrated_lufs
        - source_loudness.get(five_one).unwrap().integrated_lufs;
    assert!((2.5..=5.5).contains(&raised), "5.1 gain was {raised:.2} LU");
    assert!(
        crate::facts::read(out.path())
            .iter()
            .any(|fact| fact.kind == "audio" && fact.detail == "loudness +4.00 dB")
    );
}

#[test]
fn preserved_multichannel_encode_uses_native_loudness_gain() {
    crate::init().unwrap();
    if !crate::testutil::require(
        h264_encoder().is_some()
            && aac_encoder().is_some()
            && crate::testutil::elements_available(&["fdkaacenc"]),
        "verified H.264/AAC encoders and fdkaacenc",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("in51.mkv");
    crate::testutil::render_h264_aac51_mkv(&src_path);
    let source_layout = crate::loudness::AudioLayout::new(6, 0x3f);
    let source_loudness =
        crate::loudness::measure_file(&src_path, 0, source_layout, || Ok(())).unwrap();
    assert_eq!(source_loudness.source, source_layout);

    let mut plan = RemuxPlan {
        video: StreamMode::Copy,
        audio: StreamMode::Encode,
        audio_track: 0,
        video_track: 0,
        max_channels: Some(6),
        ..Default::default()
    };
    plan.loudness_gains[0] = Some(crate::loudness::AudioLayoutGain {
        layout: source_layout,
        gain_db: 3.0,
    });
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
    assert!(job.finished(), "5.1 encode did not finish");
    assert!(
        job.failed().is_none(),
        "5.1 encode failed: {:?}",
        job.failed()
    );
    let seg = std::fs::read_dir(out.path())
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| path.extension().is_some_and(|extension| extension == "ts"))
        .expect("no segment produced");
    let info = crate::discover(&seg, Duration::from_secs(30)).unwrap();
    assert_eq!(info.audio[0].channels, 6, "encode must preserve 5.1");
    let output_layout = crate::loudness::AudioLayout::from_stream(
        info.audio[0].channels,
        info.audio[0].layout.as_deref(),
    );
    let output_loudness = crate::loudness::measure_file(&seg, 0, output_layout, || Ok(())).unwrap();
    let five_one = crate::loudness::AudioLayout::new(6, 0x3f);
    let raised = output_loudness.get(five_one).unwrap().integrated_lufs
        - source_loudness.get(five_one).unwrap().integrated_lufs;
    assert!(
        (1.5..=4.5).contains(&raised),
        "native gain was {raised:.2} LU"
    );
    let facts = crate::facts::read(out.path());
    assert!(
        facts
            .iter()
            .any(|fact| fact.kind == "audio" && fact.detail == "loudness +3.00 dB"),
        "the native loudness gain must be reported: {facts:?}"
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
        crate::testutil::require(false, "verified AAC encoder");
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
    assert_eq!(
        opus_output_layout(crate::loudness::AudioLayout::new(6, 0), None),
        crate::loudness::AudioLayout::new(6, 0x3f),
        "unpositioned canonical layouts must pin predictably"
    );
    assert!(n >= 6, "7.1 collapsed to {n} channels");
    // The client's ceiling still bounds the choice (HUB-15).
    let (capped, _) = aac_input_layout(enc, 8, 0xc3f, Some(2)).expect("no layout under ceiling");
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
    if !crate::testutil::require(
        aac_encoder().is_some() && crate::testutil::elements_available(&["fdkaacenc"]),
        "verified AAC encoder and fdkaacenc",
    ) {
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
    if !crate::testutil::require(
        h264_encoder().is_some()
            && aac_encoder().is_some()
            && tonemap_available()
            && crate::testutil::elements_available(&["x265enc"]),
        "verified H.264 and AAC encoders, GL tone-map segment, and x265enc",
    ) {
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
    if !crate::testutil::require_h264_aac_fixture() {
        return;
    }
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
