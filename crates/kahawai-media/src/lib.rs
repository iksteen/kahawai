//! GStreamer wrappers (MH-3): file discovery mapped into the normalized
//! stream model. Blocking — call from `spawn_blocking` in async contexts.

pub mod assraster;
pub mod bench;
pub mod burnin;
pub mod doctor;
pub mod facts;
pub mod fmp4sink;
pub mod imagesubs;
pub mod loudness;
pub mod negotiate;
pub mod remux;
pub mod selected_decode;
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
use kahawai_core::media::{
    AudioStream, Chapter, MediaInfo, SubtitleStream, VideoGeometry, VideoStream,
};

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
    let info = match discoverer.discover_uri(&uri) {
        Ok(info) => info,
        // The synchronous binding returns the GError and drops the
        // GstDiscovererInfo, and the info is the whole question: a
        // decode chain that failed on one track still describes the
        // rest of the file. The signal hands over both.
        Err(sync_err) => discover_uri_partial(&uri, timeout)
            .with_context(|| format!("discovering {}: {}", path.display(), sync_err.message()))?,
    };
    // discover_uri returns Ok even on timeout/missing-plugin results —
    // don't let those masquerade as valid-but-empty media (a slow NAS
    // would scan whole libraries as streamless files).
    let result = info.result();
    // Two results are worth a second look rather than a refusal, because
    // both can arrive with the streams already described:
    //
    //   MissingPlugins — one unmappable track (S_DVBSUB →
    //   application/x-subtitle-unknown), not a broken file.
    //
    //   Error — the discoverer DECODES, and a decode chain can fail on a
    //   track we would never decode. Measured: an MKV whose Atmos E-AC-3
    //   fails to negotiate (`transform could not transform
    //   audio/x-eac3, channels=6 in anything`) errors the whole
    //   discovery, while parsebin — what the remux path actually
    //   builds — walks the same file without complaint.
    //
    // Timeout is NOT in this list. Partial information from a slow read
    // is indistinguishable from a complete answer once mapped, and a
    // stalling NAS would quietly scan a library as short files.
    let partial = result_is_partial(result);
    if result != gstreamer_pbutils::DiscovererResult::Ok && !partial {
        anyhow::bail!("discovering {}: {result:?}", path.display());
    }
    let mapped = map_info(&info);
    // The core is the evidence: audio and video both mapped means the
    // discoverer got far enough to describe the file, whatever it
    // tripped over on the way.
    if partial && (mapped.video.is_empty() || mapped.audio.is_empty()) {
        anyhow::bail!("discovering {}: {result:?}", path.display());
    }
    if partial {
        tracing::warn!(
            path = %path.display(),
            ?result,
            video = mapped.video.len(),
            audio = mapped.audio.len(),
            subtitles = mapped.subtitles.len(),
            "discovery reported a problem but described a usable file; using what it found"
        );
    }
    Ok(mapped)
}

/// Inspect only display geometry for one exact source. This deliberately uses
/// the same Discoverer facts as a scan without becoming a scan: no directory
/// walk, reconciliation, hashes, sidecars or catalogue writes.
pub fn probe_video_geometry(path: &Path, timeout: Duration) -> Result<Vec<VideoGeometry>> {
    init()?;
    let uri = gst::glib::filename_to_uri(path, None)
        .with_context(|| format!("building uri for {}", path.display()))?;
    let discoverer = Discoverer::new(gst::ClockTime::from_mseconds(timeout.as_millis() as u64))?;
    let info = discoverer
        .discover_uri(&uri)
        .with_context(|| format!("probing video geometry for {}", path.display()))?;
    Ok(info
        .video_streams()
        .into_iter()
        .filter(is_terminal)
        .map(|stream| geometry(&stream, orientation_for(&info, &stream)))
        .collect())
}

/// Whether a non-Ok discovery may still be believed if it described a
/// usable audio/video core.
fn result_is_partial(result: gstreamer_pbutils::DiscovererResult) -> bool {
    matches!(
        result,
        gstreamer_pbutils::DiscovererResult::MissingPlugins
            | gstreamer_pbutils::DiscovererResult::Error
    )
}

/// Run the discoverer through its signal so the info survives an error.
///
/// `gst_discoverer_discover_uri` returns the info AND sets a GError; the
/// Rust binding keeps only the error. The async form emits both to
/// `discovered`, so this is the same discovery, read differently — not a
/// second, weaker attempt.
fn discover_uri_partial(uri: &str, timeout: Duration) -> Result<DiscovererInfo> {
    use gstreamer::glib;

    let ctx = glib::MainContext::new();
    let run = || -> Result<DiscovererInfo> {
        let discoverer =
            Discoverer::new(gst::ClockTime::from_mseconds(timeout.as_millis() as u64))?;
        let main_loop = glib::MainLoop::new(Some(&ctx), false);
        let found: std::sync::Arc<std::sync::Mutex<Option<DiscovererInfo>>> = Default::default();
        {
            let (found, ml) = (found.clone(), main_loop.clone());
            discoverer.connect_discovered(move |_, info, _err| {
                *found.lock().unwrap() = Some(info.clone());
                ml.quit();
            });
        }
        // Backstop only: the discoverer's own timeout fires first, and
        // this exists so a signal that never arrives cannot hang a scan.
        {
            let ml = main_loop.clone();
            glib::timeout_add_local_once(timeout + Duration::from_secs(5), move || ml.quit());
        }
        discoverer.start();
        discoverer.discover_uri_async(uri)?;
        main_loop.run();
        discoverer.stop();
        let info = found.lock().unwrap().take();
        info.context("the discoverer returned neither information nor an error")
    };
    ctx.with_thread_default(run)
        .context("acquiring a main context for discovery")?
}

fn orientation_for(
    info: &DiscovererInfo,
    stream: &gstreamer_pbutils::DiscovererVideoInfo,
) -> String {
    // GStreamer primary docs define these as the clockwise transform to apply
    // for display, with `flip` meaning horizontal mirroring:
    // https://gstreamer.freedesktop.org/documentation/gstreamer/gsttaglist.html#GST_TAG_IMAGE_ORIENTATION
    stream
        .tags()
        .or_else(|| info.tags())
        .and_then(|tags| tags.get::<gst::tags::ImageOrientation>())
        .map(|value| value.get().to_string())
        .filter(|value| {
            matches!(
                value.as_str(),
                "rotate-0"
                    | "rotate-90"
                    | "rotate-180"
                    | "rotate-270"
                    | "flip-rotate-0"
                    | "flip-rotate-90"
                    | "flip-rotate-180"
                    | "flip-rotate-270"
            )
        })
        .unwrap_or_else(|| "rotate-0".into())
}

fn geometry(stream: &gstreamer_pbutils::DiscovererVideoInfo, orientation: String) -> VideoGeometry {
    // Discoverer exposes PAR as numerator/denominator (primary API docs):
    // https://gstreamer.freedesktop.org/documentation/pbutils/gstdiscoverer.html#gst_discoverer_video_info_get_par_num
    let par = stream.par();
    let (mut n, mut d) = (par.numer().max(1) as u32, par.denom().max(1) as u32);
    let gcd = |mut a: u32, mut b: u32| {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a.max(1)
    };
    let g = gcd(n, d);
    (n, d) = (n / g, d / g);
    let mut width = ((stream.width() as u64 * n as u64 + d as u64 / 2) / d as u64)
        .clamp(1, u32::MAX as u64) as u32;
    let mut height = stream.height().max(1);
    if matches!(
        orientation.as_str(),
        "rotate-90" | "rotate-270" | "flip-rotate-90" | "flip-rotate-270"
    ) {
        (width, height) = (height, width);
    }
    VideoGeometry {
        pixel_aspect_ratio: (n, d),
        orientation,
        display_width: width,
        display_height: height,
    }
}

/// A container's table of contents, flattened to chapters in start order.
/// Editions and other grouping entries are containers for the chapters
/// underneath them, not chapters themselves.
fn chapters_of(toc: gst::Toc) -> Vec<Chapter> {
    fn walk(entry: &gst::TocEntry, out: &mut Vec<Chapter>, depth: usize) {
        // The same cap as the sparse Matroska reader, for the same reason: a
        // crafted file nesting ~40k levels in under a megabyte overflows the
        // stack, and in Rust that ABORTS the scanning process. Any demuxer
        // that mirrors container nesting into its TOC re-opens the hole here.
        if depth > 16 {
            return;
        }
        if entry.entry_type() == gst::TocEntryType::Chapter
            && let Some((start, stop)) = entry.start_stop_times()
        {
            out.push(Chapter {
                start_ms: (start.max(0) / 1_000_000) as u64,
                // stop >= 0 too: a negative demuxer time sign-wrapped the
                // cast into an end eighteen trillion ms out.
                end_ms: (stop > start && stop >= 0).then_some((stop / 1_000_000) as u64),
                title: entry
                    .tags()
                    .and_then(|t| {
                        t.get::<gst::tags::Title>()
                            .map(|v| v.get().trim().to_string())
                    })
                    .filter(|t| !t.is_empty()),
            });
        }
        for sub in entry.sub_entries() {
            walk(&sub, out, depth + 1);
        }
    }
    let mut out = Vec::new();
    for entry in toc.entries() {
        walk(&entry, &mut out, 0);
    }
    out.sort_by_key(|c| c.start_ms);
    dedup_chapters(&mut out);
    out
}

/// One chapter per (start, title): duplicates go, but two DIFFERENT titles
/// on one timestamp are both information — a grouping "Feature" atom and its
/// nested "Intro" both start at zero, and keying the dedup on the start
/// alone silently ate whichever sorted second, which for the skip analyzer
/// was the one that mattered. An untitled twin yields to a titled one.
pub(crate) fn dedup_chapters(out: &mut Vec<Chapter>) {
    out.dedup_by(|next, kept| {
        if next.start_ms != kept.start_ms {
            return false;
        }
        if kept.title.is_none() {
            std::mem::swap(kept, next);
        } else if next.title.is_some() && next.title != kept.title {
            return false;
        }
        // The dropped twin may be the one carrying the container's stated
        // end — "a fact about the file" the survivor must not lose.
        kept.end_ms = kept.end_ms.or(next.end_ms);
        true
    });
}

fn map_info(info: &DiscovererInfo) -> MediaInfo {
    let mut out = MediaInfo {
        container: info
            .stream_info()
            .and_then(|s| caps_name(&s))
            .map(|n| normalize_container(&n)),
        duration_ms: info.duration().map(|d| d.mseconds()),
        // Empty rather than absent: discovery ran, so the question
        // was asked, and `None` is reserved for rows that predate it.
        // Matroska is read properly by the caller — see `declare_chapters`
        // — because the demuxer does not always post a TOC.
        chapters: Some(info.toc().map(chapters_of).unwrap_or_default()),
        ..Default::default()
    };

    // The *_streams() lists flatten parse chains: a mislabeled track
    // (E-AC-3 tag, AC-3 bitstream) yields one entry per link. Keep only
    // terminal entries — the real bitstream type, and the caps the remux
    // pipeline will actually see.
    for s in info.video_streams().into_iter().filter(is_terminal) {
        let geometry = geometry(&s, orientation_for(info, &s));
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
        let mpeg_version = caps.as_ref().and_then(|c| {
            c.structure(0)
                .and_then(|st| st.get::<i32>("mpegversion").ok())
        });
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
            pixel_aspect_ratio: Some(geometry.pixel_aspect_ratio),
            orientation: Some(geometry.orientation),
            display_width: Some(geometry.display_width),
            display_height: Some(geometry.display_height),
        });
    }
    out.video_geometry_probed = true;

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
        // ReplayGain (HUB-19). GStreamer has already turned whatever the
        // container spells it — Vorbis comments in FLAC, TXXX frames in
        // MP3, APE items — into these five, so this reads one shape for
        // every format kahawai serves.
        let gain = |tag_name: &str| tags.generic(tag_name).and_then(|v| v.get::<f64>().ok());
        out.replay_gain = kahawai_core::media::ReplayGain {
            track_gain_db: gain(gst::tags::TrackGain::TAG_NAME),
            track_peak: gain(gst::tags::TrackPeak::TAG_NAME),
            album_gain_db: gain(gst::tags::AlbumGain::TAG_NAME),
            album_peak: gain(gst::tags::AlbumPeak::TAG_NAME),
            reference_level_db: gain(gst::tags::ReferenceLevel::TAG_NAME),
        }
        .some();
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

    /// Which discovery verdicts may be believed on their evidence.
    ///
    /// `Error` joined this list because the discoverer DECODES: an MKV
    /// whose Atmos E-AC-3 would not negotiate errored the whole file
    /// while parsebin — what the remux path builds — read it happily,
    /// so a playable episode was invisible to the library.
    ///
    /// `Timeout` must stay out. A slow read returns a partial answer
    /// that looks exactly like a complete one once mapped, so trusting
    /// it would let a stalling NAS rewrite a library as short files —
    /// and unlike the others, the evidence for that cannot be checked.
    #[test]
    fn only_a_described_file_survives_a_bad_discovery() {
        use gstreamer_pbutils::DiscovererResult as R;
        assert!(result_is_partial(R::MissingPlugins));
        assert!(result_is_partial(R::Error));
        assert!(!result_is_partial(R::Timeout));
        assert!(!result_is_partial(R::Busy));
        assert!(!result_is_partial(R::UriInvalid));
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
        assert!(info.video_geometry_probed);
        assert_eq!(info.video[0].pixel_aspect_ratio, Some((1, 1)));
        assert_eq!(info.video[0].orientation.as_deref(), Some("rotate-0"));
        assert_eq!(
            (info.video[0].display_width, info.video[0].display_height),
            (Some(320), Some(240))
        );
        assert_eq!(
            probe_video_geometry(&path, Duration::from_secs(15)).unwrap(),
            vec![VideoGeometry {
                pixel_aspect_ratio: (1, 1),
                orientation: "rotate-0".into(),
                display_width: 320,
                display_height: 240,
            }]
        );
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
        assert!(!info.video_geometry_probed);
        assert_eq!(
            (
                v.pixel_aspect_ratio,
                v.orientation.as_deref(),
                v.display_width,
                v.display_height
            ),
            (None, None, None, None)
        );
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

    /// ReplayGain (HUB-19) is read as the file states it, and absent
    /// when the file says nothing.
    ///
    /// The point of reading GStreamer's normalised tags rather than the
    /// container's own is that FLAC's Vorbis comments, MP3's TXXX frames
    /// and APE items all arrive here in one shape. The fixture is a
    /// FLAC because that is what this library is made of.
    #[test]
    fn replay_gain_is_read_when_the_file_states_it() {
        init().unwrap();
        if !crate::testutil::require_elements(&["audiotestsrc", "taginject", "flacenc"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mux = |name: &str, tags: &str| {
            let path = dir.path().join(name);
            let inject = if tags.is_empty() {
                String::new()
            } else {
                format!("taginject tags=\"{tags}\" ! ")
            };
            let pipeline = gstreamer::parse::launch(&format!(
                "audiotestsrc num-buffers=20 ! audioconvert ! {inject}flacenc ! \
                 filesink location={}",
                path.display()
            ))
            .expect("pipeline");
            pipeline.set_state(gstreamer::State::Playing).unwrap();
            pipeline
                .bus()
                .unwrap()
                .timed_pop_filtered(
                    gstreamer::ClockTime::from_seconds(30),
                    &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
                )
                .expect("muxing timed out");
            pipeline.set_state(gstreamer::State::Null).unwrap();
            path
        };

        let tagged = mux(
            "tagged.flac",
            "replaygain-track-gain=(double)-11.28,replaygain-track-peak=(double)0.9,\
             replaygain-album-gain=(double)-10.5,replaygain-album-peak=(double)1.0",
        );
        let rg = discover(&tagged, std::time::Duration::from_secs(30))
            .expect("discover")
            .replay_gain
            .expect("the file states ReplayGain");
        assert_eq!(rg.track_gain_db, Some(-11.28));
        assert_eq!(rg.track_peak, Some(0.9));
        assert_eq!(rg.album_gain_db, Some(-10.5));
        assert_eq!(rg.album_peak, Some(1.0));

        // Untagged is None, not a shell of nulls: a client asking "does
        // this file state its loudness" gets one answer, not five.
        let bare = mux("bare.flac", "");
        assert_eq!(
            discover(&bare, std::time::Duration::from_secs(30))
                .expect("discover")
                .replay_gain,
            None
        );
    }
}
