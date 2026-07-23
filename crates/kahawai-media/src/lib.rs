//! GStreamer wrappers (MH-3): file discovery mapped into the normalized
//! stream model. Blocking — call from `spawn_blocking` in async contexts.

pub mod doctor;
pub mod remux;
pub mod worker;
#[doc(hidden)]
pub mod testutil;

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer_pbutils::prelude::*;
use gstreamer_pbutils::{Discoverer, DiscovererInfo, DiscovererStreamInfo};
use kahawai_core::media::{AudioStream, MediaInfo, SubtitleStream, VideoStream};

pub fn init() -> Result<()> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| gst::init().map_err(|e| e.to_string()))
        .clone()
        .map_err(|e| anyhow::anyhow!("gstreamer init failed: {e}"))
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
    if result != gstreamer_pbutils::DiscovererResult::Ok {
        anyhow::bail!("discovering {}: {result:?}", path.display());
    }
    Ok(map_info(&info))
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
        let name = caps
            .as_ref()
            .and_then(|c| c.structure(0).map(|st| st.name().to_string()))
            .unwrap_or_default();
        out.video.push(VideoStream {
            codec: normalize_video_codec(&name),
            width: s.width(),
            height: s.height(),
            fps: {
                let fps = s.framerate();
                (fps.numer() > 0).then(|| (fps.numer() as u32, fps.denom() as u32))
            },
            bit_depth: (s.depth() > 0).then_some(s.depth()),
            interlaced: s.is_interlaced(),
            hdr: None,
        });
    }

    for s in info.audio_streams().into_iter().filter(is_terminal) {
        let caps = s.caps();
        let name = caps
            .as_ref()
            .and_then(|c| c.structure(0).map(|st| st.name().to_string()))
            .unwrap_or_default();
        let version = caps
            .as_ref()
            .and_then(|c| c.structure(0).and_then(|st| st.get::<i32>("mpegversion").ok()));
        let layer = caps
            .as_ref()
            .and_then(|c| c.structure(0).and_then(|st| st.get::<i32>("layer").ok()));
        out.audio.push(AudioStream {
            codec: normalize_audio_codec(&name, version, layer),
            channels: s.channels(),
            sample_rate: s.sample_rate(),
            language: s.language().map(|l| l.to_string()),
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
fn is_terminal<T: gst::glib::prelude::IsA<DiscovererStreamInfo>>(s: &T) -> bool {
    s.as_ref().next().is_none()
}

fn caps_name(s: &DiscovererStreamInfo) -> Option<String> {
    s.caps()?.structure(0).map(|st| st.name().to_string())
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

fn normalize_video_codec(caps_name: &str) -> String {
    match caps_name {
        "video/x-h264" => "h264",
        "video/x-h265" => "hevc",
        "video/x-vp8" => "vp8",
        "video/x-vp9" => "vp9",
        "video/x-av1" => "av1",
        "video/mpeg" => "mpeg",
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
        other => other,
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(info.audio.len(), 1);
        assert_eq!(info.audio[0].codec, "vorbis");
        assert!(info.duration_ms.unwrap_or(0) > 500);
    }

    #[test]
    fn unreadable_file_errors_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("garbage.mkv");
        std::fs::write(&path, b"this is not media").unwrap();
        assert!(discover(&path, Duration::from_secs(5)).is_err());
    }
}
