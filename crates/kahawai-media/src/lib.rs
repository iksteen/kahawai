//! GStreamer wrappers (MH-3): file discovery mapped into the normalized
//! stream model. Blocking — call from `spawn_blocking` in async contexts.

pub mod assraster;
pub mod bench;
pub mod burnin;
pub mod doctor;
pub mod facts;
pub mod fmp4sink;
pub mod imagesubs;
pub mod negotiate;
pub mod remux;
pub mod subindex;
pub mod subtitles;
#[doc(hidden)]
pub mod testutil;
pub mod vobsub_file;
pub mod worker;

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer_pbutils::prelude::*;
use gstreamer_pbutils::{Discoverer, DiscovererAudioInfo, DiscovererInfo, DiscovererStreamInfo};
use kahawai_core::media::{AudioStream, MediaInfo, SubtitleStream, VideoStream};

pub fn init() -> Result<()> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| {
        gst::init().map_err(|e| e.to_string())?;
        // macOS: vtdec builds a GL texture cache at start and SIGSEGVs
        // without an AppKit main loop — which a headless worker never
        // has. Demote it so decodebin picks software decoders; vtenc
        // (encode) is headless-safe and stays preferred.
        // ponytail: re-enable under gst_macos_main if hw decode matters.
        #[cfg(target_os = "macos")]
        for name in ["vtdec_hw", "vtdec"] {
            if let Some(f) = gst::ElementFactory::find(name) {
                use gst::prelude::PluginFeatureExt;
                f.set_rank(gst::Rank::NONE);
            }
        }
        // `av1parse` must never be AUTO-PLUGGED, or AV1 cannot be
        // copied at all.
        //
        // `parsebin` plugs it and parses AV1 to `alignment=frame`,
        // losing the buffer timestamps in the process: only the first
        // buffer keeps a PTS. `isofmp4mux` accepts `alignment=tu` and
        // nothing else, so the parser downstream has to regroup frames
        // into temporal units with nothing to stamp them from —
        // `guard_pts` then drops every one (16796 in 30 s), the muxer's
        // video pad starves, the audio queue fills behind it, and the
        // session dies with "remux produced no playlist in time". Every
        // AV1 file, at any GOP length.
        //
        // Demoting it leaves the demuxer's own temporal units intact all
        // the way to the muxer, which is exactly what
        // `matroskademux ! av1parse ! isofmp4mux` does outside our
        // pipeline — the control that works. Nothing loses a parser it
        // needed: `route_stream` still builds `av1parse` EXPLICITLY by
        // name for the branches that want one, and rank only governs
        // auto-plugging. Verified both ways on a real AV1 source: copy
        // produces segments (it produced none before), and the transcode
        // path still decodes.
        if let Some(f) = gst::ElementFactory::find("av1parse") {
            use gst::prelude::PluginFeatureExt;
            f.set_rank(gst::Rank::NONE);
        }
        Ok(())
    })
    .clone()
    .map_err(|e| anyhow::anyhow!("gstreamer init failed: {e}"))
}

/// Demote decoder elements below every software decoder (rank NONE)
/// so decodebin stops picking them. The per-box calibration knob for
/// hardware decode paths that are broken or pathologically slow.
pub fn demote_elements(names: &[String]) -> Result<()> {
    init()?;
    for name in names {
        match gst::ElementFactory::find(name) {
            Some(f) => {
                f.set_rank(gst::Rank::NONE);
                tracing::info!(element = %name, "decoder demoted by config");
            }
            None => tracing::warn!(element = %name, "demote_decoders: element not found"),
        }
    }
    Ok(())
}

/// Discover a media file's technical metadata.
pub fn discover(path: &Path, timeout: Duration) -> Result<MediaInfo> {
    init()?;
    let uri = gst::glib::filename_to_uri(path, None)
        .with_context(|| format!("building uri for {}", path.display()))?;
    let discoverer = Discoverer::new(gst::ClockTime::from_mseconds(timeout.as_millis() as u64))?;
    let info = discoverer
        .discover_uri(&uri)
        .with_context(|| format!("discovering {}", path.display()))?;
    // discover_uri returns Ok even on timeout/missing-plugin results —
    // don't let those masquerade as valid-but-empty media (a slow NAS
    // would scan whole libraries as streamless files).
    let result = info.result();
    let missing_plugins = result == gstreamer_pbutils::DiscovererResult::MissingPlugins;
    if result != gstreamer_pbutils::DiscovererResult::Ok && !missing_plugins {
        anyhow::bail!("discovering {}: {result:?}", path.display());
    }
    let mapped = map_info(&info);
    // MissingPlugins with a usable A/V core is one unmappable track
    // (e.g. S_DVBSUB → application/x-subtitle-unknown), not a broken
    // file — record what we found instead of failing the whole file.
    if missing_plugins && (mapped.video.is_empty() || mapped.audio.is_empty()) {
        anyhow::bail!("discovering {}: {result:?}", path.display());
    }
    Ok(mapped)
}

fn map_info(info: &DiscovererInfo) -> MediaInfo {
    let mut out = MediaInfo {
        container: info
            .stream_info()
            .and_then(|s| caps_name(&s))
            .map(|n| normalize_container(&n)),
        duration_ms: info.duration().map(|d| d.mseconds()),
        ..Default::default()
    };

    // The *_streams() lists flatten parse chains: a mislabeled track
    // (E-AC-3 tag, AC-3 bitstream) yields one entry per link. Keep only
    // terminal entries — the real bitstream type, and the caps the remux
    // pipeline will actually see.
    for s in info.video_streams().into_iter().filter(is_terminal) {
        let caps = s.caps();
        let st_get = |field: &str| {
            caps.as_ref()
                .and_then(|c| c.structure(0).and_then(|st| st.get::<&str>(field).ok()))
                .map(str::to_string)
        };
        let name = caps
            .as_ref()
            .and_then(|c| c.structure(0).map(|st| st.name().to_string()))
            .unwrap_or_default();
        let hdr = classify_hdr(st_get("colorimetry").as_deref());
        // `video/mpeg` is three different codecs wearing one caps name;
        // the version is a FIELD, exactly as it is for audio.
        let mpeg_version = caps
            .as_ref()
            .and_then(|c| c.structure(0).and_then(|st| st.get::<i32>("mpegversion").ok()));
        out.video.push(VideoStream {
            codec: normalize_video_codec(&name, mpeg_version),
            width: s.width(),
            height: s.height(),
            fps: {
                let fps = s.framerate();
                (fps.numer() > 0).then(|| (fps.numer() as u32, fps.denom() as u32))
            },
            bit_depth: (s.depth() > 0).then_some(s.depth()),
            interlaced: s.is_interlaced(),
            hdr,
            profile: st_get("profile"),
            level: st_get("level"),
            bitrate_kbps: (s.bitrate() > 0).then(|| s.bitrate() / 1000),
            // GStreamer discovery does not expose keyframe spacing; the
            // mediahost fills this from the container index after the
            // probe (scan.rs).
            max_keyframe_interval_ms: None,
        });
    }

    for s in info.audio_streams().into_iter().filter(is_terminal) {
        // Codec, width and layout come from the widest link; language and
        // bitrate from the parsed one, which is usually the only link
        // carrying them (the container entry above reports neither).
        let s = &s;
        let widest = widest_audio(s);
        let caps = widest.caps();
        let name = caps
            .as_ref()
            .and_then(|c| c.structure(0).map(|st| st.name().to_string()))
            .unwrap_or_default();
        let version = caps.as_ref().and_then(|c| {
            c.structure(0)
                .and_then(|st| st.get::<i32>("mpegversion").ok())
        });
        let layer = caps
            .as_ref()
            .and_then(|c| c.structure(0).and_then(|st| st.get::<i32>("layer").ok()));
        let layout = caps.as_ref().and_then(|c| {
            c.structure(0)
                .and_then(|st| st.get::<gst::Bitmask>("channel-mask").ok())
                .map(|m| format!("{:#x}", m.0))
        });
        out.audio.push(AudioStream {
            codec: normalize_audio_codec(&name, version, layer),
            channels: widest.channels(),
            sample_rate: widest.sample_rate(),
            language: s
                .language()
                .or_else(|| widest.language())
                .map(|l| l.to_string()),
            bitrate_kbps: (s.bitrate() > 0)
                .then(|| s.bitrate() / 1000)
                .or_else(|| (widest.bitrate() > 0).then(|| widest.bitrate() / 1000)),
            layout,
        });
    }

    for s in info.subtitle_streams().into_iter().filter(is_terminal) {
        let name = s
            .caps()
            .and_then(|c| c.structure(0).map(|st| st.name().to_string()))
            .unwrap_or_default();
        out.subtitles.push(SubtitleStream {
            format: normalize_subtitle_format(&name),
            language: s.language().map(|l| l.to_string()),
        });
    }

    if let Some(tags) = info.tags() {
        for (name, tag_name) in [
            ("title", gst::tags::Title::TAG_NAME),
            ("artist", gst::tags::Artist::TAG_NAME),
            ("album", gst::tags::Album::TAG_NAME),
        ] {
            if let Some(v) = tags.generic(tag_name).and_then(|v| v.get::<String>().ok()) {
                out.tags.insert(name.into(), v);
            }
        }
        for (name, tag_name) in [
            ("track_number", gst::tags::TrackNumber::TAG_NAME),
            ("disc_number", gst::tags::AlbumVolumeNumber::TAG_NAME),
        ] {
            if let Some(v) = tags.generic(tag_name).and_then(|v| v.get::<u32>().ok()) {
                out.tags.insert(name.into(), v.to_string());
            }
        }
    }

    out
}

/// A stream info is terminal when nothing further re-types it (its
/// `next()` is the end of the parse chain).
/// HDR from colorimetry (MH-3): the transfer function names the
/// standard — PQ = HDR10-family, HLG = HLG. Caps carry either a
/// canonical shorthand ("bt2100-pq") or the colon-separated numeric
/// form whose 3rd field serializes GstVideoTransferFunction
/// (video-color.h: 14 = SMPTE2084/PQ, 15 = ARIB STD-B67/HLG — and 16
/// is BT601, NOT PQ). These are gst enum values, not ISO H.273 codes;
/// an earlier map used 16/14/18 from H.273 and read every
/// numerically-tagged PQ file as hlg.
fn classify_hdr(colorimetry: Option<&str>) -> Option<String> {
    let c = colorimetry?;
    let transfer = c.split(':').nth(2);
    if c.contains("bt2100-pq") || transfer == Some("14") {
        Some("hdr10".to_string())
    } else if c.contains("bt2100-hlg") || transfer == Some("15") {
        Some("hlg".to_string())
    } else {
        None
    }
}

fn is_terminal<T: gst::glib::prelude::IsA<DiscovererStreamInfo>>(s: &T) -> bool {
    s.as_ref().next().is_none()
}

fn caps_name(s: &DiscovererStreamInfo) -> Option<String> {
    s.caps()?.structure(0).map(|st| st.name().to_string())
}

/// The WIDEST entry in an audio parse chain — the one that describes the
/// whole stream rather than a piece of it.
///
/// Terminal-entry-wins is right for a mislabeled track (container tagged
/// E-AC-3 over an AC-3 bitstream: the parser knows better), but wrong for
/// Dolby Digital Plus. A DD+ 7.1 track arrives as an `E-AC-3, 8 channels`
/// container entry with ac3parse's view nested underneath it — `AC-3,
/// 6 channels`, the independent substream ALONE, because the parser
/// splits each block into core plus extension and only describes the
/// core. Taking the terminal entry reported "ac3 5.1" for a 7.1 E-AC-3
/// track, which is what the session verdict then told the user.
///
/// A parser can only ever describe a SUBSET of a multi-substream stream,
/// never a superset, so the widest link in the chain is the stream
/// itself. Ties keep the terminal entry, which leaves the mislabeled
/// case correct: there both links carry the same channel count, and the
/// parser's codec name is the one that matches the bitstream.
fn widest_audio(s: &DiscovererAudioInfo) -> DiscovererAudioInfo {
    let mut best = s.clone();
    let mut cur = AsRef::<DiscovererStreamInfo>::as_ref(s).previous();
    while let Some(p) = cur {
        if let Ok(a) = p.clone().downcast::<DiscovererAudioInfo>()
            && a.channels() > best.channels()
        {
            best = a;
        }
        cur = p.previous();
    }
    best
}

fn normalize_container(caps_name: &str) -> String {
    match caps_name {
        "video/x-matroska" => "matroska",
        "video/webm" => "webm",
        "video/quicktime" => "mp4",
        "application/ogg" => "ogg",
        "audio/x-flac" => "flac",
        "audio/mpeg" => "mp3",
        "video/mpegts" => "mpegts",
        "audio/x-wav" => "wav",
        other => other,
    }
    .to_string()
}

/// `mpeg_version` splits the `video/mpeg` caps name the way the audio
/// normalizer below already splits `audio/mpeg` into mp3 and aac.
///
/// It matters because the three are not interchangeable to a client:
/// Android lists MPEG-4 Part 2 as a MANDATORY decoder on every version
/// and does not list MPEG-2 at all. Collapsing them meant a client
/// declaring "mpeg" — meaning the one its platform guarantees — could
/// be handed a copy of the one it does not, which is a wrong-codec bug
/// that has nothing to do with why this function was revisited.
///
/// Unknown version keeps the old flat name rather than guessing: it is
/// what pre-existing rows say, and it degrades toward a transcode.
fn normalize_video_codec(caps_name: &str, mpeg_version: Option<i32>) -> String {
    match caps_name {
        "video/x-h264" => "h264",
        "video/x-h265" => "hevc",
        "video/x-vp8" => "vp8",
        "video/x-vp9" => "vp9",
        "video/x-av1" => "av1",
        "video/mpeg" => match mpeg_version {
            Some(1) => "mpeg1",
            Some(2) => "mpeg2",
            // Part 2 — DivX/Xvid. NOT Part 10, which is h264 above.
            Some(4) => "mpeg4part2",
            _ => "mpeg",
        },
        "video/x-theora" => "theora",
        other => other,
    }
    .to_string()
}

fn normalize_audio_codec(caps_name: &str, mpeg_version: Option<i32>, layer: Option<i32>) -> String {
    match caps_name {
        "audio/mpeg" => match (mpeg_version, layer) {
            (Some(1), Some(3)) => "mp3".into(),
            (Some(1), _) => "mpeg-audio".into(),
            (Some(2 | 4), _) => "aac".into(),
            _ => "mpeg-audio".into(),
        },
        "audio/x-vorbis" => "vorbis".into(),
        "audio/x-opus" => "opus".into(),
        "audio/x-flac" => "flac".into(),
        "audio/x-ac3" => "ac3".into(),
        "audio/x-eac3" => "eac3".into(),
        "audio/x-dts" => "dts".into(),
        "audio/x-true-hd" => "truehd".into(),
        "audio/x-raw" => "pcm".into(),
        other => other.to_string(),
    }
}

fn normalize_subtitle_format(caps_name: &str) -> String {
    match caps_name {
        "text/x-raw" | "application/x-subtitle" => "text",
        "application/x-ssa" | "application/x-ass" => "ass",
        "subpicture/x-pgs" => "pgs",
        "subpicture/x-dvd" => "vobsub",
        "application/x-subtitle-vtt" => "webvtt",
        // Codec the demuxer couldn't map (e.g. S_DVBSUB): the track is
        // declared but unserveable; players see it and skip it.
        "application/x-subtitle-unknown" => "unknown",
        other => other,
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both caps colorimetry forms classify, and the gst-enum/H.273
    /// confusion stays dead: 14 is PQ, 15 is HLG, 16 is BT601 (SD!).
    #[test]
    fn hdr_classification_reads_gst_enum_not_h273() {
        assert_eq!(classify_hdr(Some("bt2100-pq")).as_deref(), Some("hdr10"));
        assert_eq!(classify_hdr(Some("bt2100-hlg")).as_deref(), Some("hlg"));
        assert_eq!(
            classify_hdr(Some("0:6:14:7")).as_deref(),
            Some("hdr10"),
            "14 = SMPTE2084"
        );
        assert_eq!(
            classify_hdr(Some("0:6:15:7")).as_deref(),
            Some("hlg"),
            "15 = ARIB STD-B67"
        );
        assert_eq!(classify_hdr(Some("0:6:16:7")), None, "16 = BT601, not PQ");
        assert_eq!(classify_hdr(Some("bt709")), None);
        assert_eq!(classify_hdr(None), None);
    }

    /// Build a tiny fixture by running a gst-launch-style pipeline to EOS.
    fn render(pipeline: &str) {
        init().unwrap();
        let p = gst::parse::launch(pipeline).unwrap();
        p.set_state(gst::State::Playing).unwrap();
        let bus = p.bus().unwrap();
        let msg = bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(30),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        p.set_state(gst::State::Null).unwrap();
        match msg {
            Some(m) if m.type_() == gst::MessageType::Eos => {}
            other => panic!("pipeline did not reach EOS: {other:?}"),
        }
    }

    #[test]
    fn discovers_mkv_with_h264_and_vorbis() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.mkv");
        render(&format!(
            "videotestsrc num-buffers=30 ! video/x-raw,width=320,height=240,framerate=25/1 \
             ! x264enc speed-preset=ultrafast ! h264parse ! matroskamux name=m \
             audiotestsrc num-buffers=50 ! audioconvert ! vorbisenc ! m. \
             m. ! filesink location={}",
            path.display()
        ));

        let info = discover(&path, Duration::from_secs(15)).unwrap();
        assert_eq!(info.container.as_deref(), Some("matroska"));
        assert_eq!(info.video.len(), 1);
        assert_eq!(info.video[0].codec, "h264");
        assert_eq!((info.video[0].width, info.video[0].height), (320, 240));
        assert_eq!(info.video[0].fps, Some((25, 1)));
        // MH-3 extension: x264enc+h264parse emit profile/level in caps.
        assert!(
            info.video[0].profile.is_some(),
            "caps profile must be extracted"
        );
        assert!(
            info.video[0].level.is_some(),
            "caps level must be extracted"
        );
        assert_eq!(info.video[0].hdr, None, "SDR testsrc must not read as HDR");
        assert_eq!(info.audio.len(), 1);
        assert_eq!(info.audio[0].codec, "vorbis");
        assert!(info.duration_ms.unwrap_or(0) > 500);
    }

    /// The graceful-degradation contract (HUB-14): a streams_json row
    /// probed BEFORE the MH-3 extension deserializes with every new
    /// field None — negotiation treats those as unknown-permissive.
    #[test]
    fn pre_extension_rows_deserialize_with_unknowns() {
        let old = r#"{
            "container": "matroska", "duration_ms": 1000,
            "video": [{"codec":"h264","width":1920,"height":1080,
                       "fps":[24,1],"bit_depth":null,"interlaced":false,"hdr":null}],
            "audio": [{"codec":"aac","channels":6,"sample_rate":48000,"language":"en"}],
            "subtitles": [{"format":"ass","language":"en"}]
        }"#;
        let info: kahawai_core::media::MediaInfo = serde_json::from_str(old).unwrap();
        let v = &info.video[0];
        assert_eq!(
            (
                v.profile.as_deref(),
                v.level.as_deref(),
                v.bitrate_kbps,
                v.hdr.as_deref()
            ),
            (None, None, None, None)
        );
        let a = &info.audio[0];
        assert_eq!(
            (a.bitrate_kbps, a.layout.as_deref(), a.channels),
            (None, None, 6)
        );
    }

    #[test]
    fn unreadable_file_errors_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("garbage.mkv");
        std::fs::write(&path, b"this is not media").unwrap();
        assert!(discover(&path, Duration::from_secs(5)).is_err());
    }
}
