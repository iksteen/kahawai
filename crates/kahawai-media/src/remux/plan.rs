use super::*;

/// What happens to one stream kind in a session (HUB-16 decision order:
/// copy what the client and muxer both take, encode what they don't but
/// a decoder can read, drop the rest).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StreamMode {
    Copy,
    /// Decode → re-encode to the target codec (h264 video / AAC audio).
    Encode,
    #[default]
    Off,
}

/// What the receiving client can actually decode (HUB-14). Muxability
/// alone lies: mpegtsmux happily carries MPEG-4 Part 2 and DTS, but no
/// browser plays either — copy must satisfy the client AND the muxer.
pub struct Target {
    pub video: &'static [&'static str],
    pub audio: &'static [&'static str],
}

/// hls.js/MSE baseline: H.264 video; AAC or MP3 audio.
pub const WEB_TARGET: Target = Target {
    video: &["h264"],
    audio: &["aac", "mp3"],
};

/// HUB-15b encode targets and the segment container that carries them.
/// Small Copy enums because RemuxPlan crosses the worker CLI and the
/// protobuf wire; unknown strings parse to the legacy default so an
/// old sender never breaks a new binary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VideoTarget {
    #[default]
    H264,
    Hevc,
    Av1,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AudioTarget {
    #[default]
    Aac,
    Opus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SegmentFormat {
    /// MPEG-TS segments via hlssink3/hlssink2 — the proven path; every
    /// h264/aac session rides it unchanged.
    #[default]
    Ts,
    /// Fragmented-MP4 segments via isofmp4mux + the fmp4sink writer —
    /// the only container that carries hevc/av1/opus to browsers.
    Fmp4,
}

#[allow(clippy::should_implement_trait)] // infallible, legacy-defaulting — not the trait
impl VideoTarget {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "hevc" => Self::Hevc,
            "av1" => Self::Av1,
            _ => Self::H264,
        }
    }
}

#[allow(clippy::should_implement_trait)] // infallible, legacy-defaulting — not the trait
impl AudioTarget {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Aac => "aac",
            Self::Opus => "opus",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "opus" => Self::Opus,
            _ => Self::Aac,
        }
    }
}

#[allow(clippy::should_implement_trait)] // infallible, legacy-defaulting — not the trait
impl SegmentFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ts => "ts",
            Self::Fmp4 => "fmp4",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "fmp4" => Self::Fmp4,
            _ => Self::Ts,
        }
    }
}

/// Per-kind session plan — the single source of truth shared between
/// session planning and pipeline routing, so the muxer pads requested up
/// front always match the streams that will actually be linked.
#[derive(Debug, Clone, Copy, Default)]
pub struct RemuxPlan {
    pub video: StreamMode,
    pub audio: StreamMode,
    /// Which audio stream to carry, indexed over the file's audio
    /// streams in discovery/demux order (HUB-27 track selection).
    pub audio_track: usize,
    /// Same for video (dual-video muxes: clean + hardsubbed fansub
    /// releases, sample tracks next to the feature).
    pub video_track: usize,
    /// Encode-branch parameters (HUB-14/15). None = the historical
    /// fixed values: 6000 kbit video, no scaling, no downmix.
    pub video_kbps: Option<u32>,
    pub max_height: Option<u32>,
    /// HUB-15a: run the GL PQ→SDR tone-map segment in the video encode
    /// chain. Only set when the executing box reported the capability.
    pub tone_map: bool,
    /// Deinterlace before the encoder, because the source carries fields.
    ///
    /// Only meaningful with an encode: a copy hands the client whatever
    /// the file holds. Set from the source's own `interlaced` flag, and
    /// not unconditionally, because the element works in system memory
    /// and cannot take the 10-bit device memory a hardware decoder hands
    /// an NVENC encode — inserting it always would cost every HDR encode
    /// a download and a conversion to buy nothing.
    ///
    /// It is also what makes those sources encode at all here: nvh264enc
    /// fails at the first frame with `Failed to lock bitstream,
    /// NV_ENC_ERR_INVALID_PARAM` when handed field-flagged buffers (an
    /// RTX 5070 Ti, driver 610.43.03; measured, and relabelling the caps
    /// progressive with capssetter is enough to make it pass, so it is
    /// the field flags and not the pixels).
    pub deinterlace: bool,
    /// HUB-32b last resort: burn this image subtitle track (the `e{n}`
    /// index) into the picture, for clients that cannot composite one
    /// themselves. Forces the video encode that carries it.
    pub burn_subtitle: Option<usize>,
    /// HUB-32a: burn this EMBEDDED text subtitle track (the same `e{n}`
    /// index space as `burn_subtitle`) into the picture with libass, for
    /// clients that cannot render ASS themselves and whose user prefers
    /// faithful typesetting over a flattened VTT. The demuxer's own
    /// `application/x-ass` pad is used because it is the only source
    /// that carries the file's attached fonts. Forces the video encode.
    /// A user's SIDECAR .ass burns from a file instead (`burn_ass_file`
    /// on `start_parts`) and has no fonts to attach.
    pub burn_ass: Option<usize>,
    pub max_channels: Option<u32>,
    /// Static EBU R128 gain measured on the exact stereo fold. `None` means
    /// unmeasured; applied only when the selected encoder layout is stereo.
    pub stereo_gain_db: Option<f64>,
    /// Static gain measured on the untouched decoded layout. Applied only
    /// when the encoded output preserves `loudness_source_channels`.
    pub native_gain_db: Option<f64>,
    pub loudness_source_channels: Option<u32>,
    /// Static gains keyed by the exact post-conversion channel layout.
    /// Protocol-4 workers use this map; scalar fields cannot express a
    /// downmix's exact layout-specific gain.
    pub loudness_gains: crate::loudness::AudioLayoutGains,
    /// HUB-15b: what the encode arms produce and which segment
    /// container carries the session. Defaults = the historical
    /// h264/aac/TS behavior.
    pub video_codec: VideoTarget,
    pub audio_codec: AudioTarget,
    pub segment_format: SegmentFormat,
}

/// Fallback declaration when nothing better is known: the historical
/// value, which is honest only for an encode (whose GOP we set) or a
/// short-GOP source.
pub const DEFAULT_TARGET_DURATION_SECS: u32 = 2;

/// The fragment length the sink asks for. Segments are whole GOPs
/// packed until this is passed, so the longest segment is bounded by
/// `FRAGMENT_TARGET + max keyframe gap` — the formula the declaration
/// is computed from, verified against real files (GOP 10.43 s produced
/// segments of 10.22-10.58 s).
pub const FRAGMENT_TARGET_SECS: u32 = 2;

impl RemuxPlan {
    pub fn has_video(&self) -> bool {
        self.video != StreamMode::Off
    }
    pub fn has_audio(&self) -> bool {
        self.audio != StreamMode::Off
    }
    /// Anything to produce at all?
    pub fn playable(&self) -> bool {
        self.has_video() || self.has_audio()
    }
}

pub fn plan_streams(
    info: &kahawai_core::media::MediaInfo,
    target: &Target,
    audio_track: usize,
    video_track: usize,
) -> RemuxPlan {
    // Clamp stale indexes (rescan shrank the track list) to the last track.
    let audio_track = audio_track.min(info.audio.len().saturating_sub(1));
    let video_track = video_track.min(info.video.len().saturating_sub(1));
    let names = muxable_names(SegmentFormat::Ts);
    let copyable = |kind: &str, codec: &str, accepted: &[&str]| {
        accepted.contains(&codec)
            && codec_to_caps_name(kind, codec).is_some_and(|n| names.contains(n))
    };
    let selected_v = info.video.get(video_track);
    let video = if selected_v.is_some_and(|v| copyable("video", &v.codec, target.video)) {
        StreamMode::Copy
    } else if h264_encoder().is_some()
        && selected_v.is_some_and(|v| codec_to_caps_name("video", &v.codec).is_some_and(can_decode))
    {
        StreamMode::Encode
    } else {
        StreamMode::Off
    };
    // The plan judges the SELECTED track, not "any track": switching
    // from an AAC track to a DTS one flips copy → encode.
    let selected = info.audio.get(audio_track);
    let audio = if selected.is_some_and(|a| copyable("audio", &a.codec, target.audio)) {
        StreamMode::Copy
    } else if aac_encoder().is_some()
        && selected.is_some_and(|a| codec_to_caps_name("audio", &a.codec).is_some_and(can_decode))
    {
        StreamMode::Encode
    } else {
        StreamMode::Off
    };
    RemuxPlan {
        video,
        audio,
        audio_track,
        video_track,
        ..Default::default()
    }
}

/// Human-readable per-kind verdict for the playback-info overlay
/// (§4.3b spirit: the player reports which path was taken and why —
/// nothing converts silently).
pub fn plan_summary(info: &kahawai_core::media::MediaInfo, plan: &RemuxPlan) -> (String, String) {
    let names = muxable_names(SegmentFormat::Ts);
    let kind_summary =
        |kind: &str, codecs: Vec<&str>, mode: StreamMode, target_codec: &str| match mode {
            StreamMode::Copy => codecs
                .iter()
                .find(|c| codec_to_caps_name(kind, c).is_some_and(|n| names.contains(n)))
                .map(|c| format!("{c} copy"))
                .unwrap_or_else(|| "copy".into()),
            StreamMode::Encode => {
                let src = codecs
                    .iter()
                    .find(|c| codec_to_caps_name(kind, c).is_some_and(can_decode))
                    .copied()
                    .unwrap_or(kind);
                format!("{src} → {target_codec} (transcoded)")
            }
            StreamMode::Off => {
                if codecs.is_empty() {
                    "none".into()
                } else {
                    format!("{} dropped (needs transcoder)", codecs[0])
                }
            }
        };
    (
        kind_summary(
            "video",
            info.video
                .get(plan.video_track)
                .map(|v| v.codec.as_str())
                .into_iter()
                .collect(),
            plan.video,
            plan.video_codec.as_str(),
        ),
        kind_summary(
            "audio",
            info.audio
                .get(plan.audio_track)
                .map(|a| a.codec.as_str())
                .into_iter()
                .collect(),
            plan.audio,
            plan.audio_codec.as_str(),
        ),
    )
}
