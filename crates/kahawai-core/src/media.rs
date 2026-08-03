//! Normalized technical stream model (MH-3) — what a mediahost reports per
//! file and what capability negotiation later consumes. Codec names are
//! lowercase normalized strings ("h264", "hevc", "aac", …).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MediaInfo {
    /// Container format, normalized ("matroska", "mp4", "webm", …).
    pub container: Option<String>,
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub video: Vec<VideoStream>,
    #[serde(default)]
    pub audio: Vec<AudioStream>,
    #[serde(default)]
    pub subtitles: Vec<SubtitleStream>,
    /// Sidecar subtitle files next to the media file (MH-4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_subtitles: Vec<SidecarSubtitle>,
    /// Local artwork in the media file's directory (MH-4):
    /// cover/folder/poster image, path relative to the collection root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork: Option<String>,
    /// A Kodi-style .nfo beside the media file or in its directory
    /// (HUB-9), path relative to the collection root. Recorded at scan so
    /// the hub can decline instantly for the files that have none — it
    /// costs a lease read to actually parse one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nfo: Option<String>,
    /// Container-level tags (title, artist, album, track number, …).
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    /// Embedded attachments (fonts, cover art) DECLARED at scan or by
    /// the MH-4 backfill: name, mime, and the payload's byte range —
    /// never the payload itself. Tri-state: `None` = never declared
    /// (fall back to demux), `Some([])` = checked and none exist,
    /// `Some([...])` = read these exact ranges (HUB-34 fonts rung).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Attachment>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    pub file_name: String,
    pub mime_type: String,
    /// Byte offset of the raw payload within the media file.
    pub offset: u64,
    pub size: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct VideoStream {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    /// Frames per second as (numerator, denominator).
    pub fps: Option<(u32, u32)>,
    pub bit_depth: Option<u32>,
    pub interlaced: bool,
    /// "hdr10" | "hlg", from caps colorimetry (MH-3). None = SDR or a
    /// row probed before extraction existed — negotiation treats both
    /// as SDR (HUB-15a).
    pub hdr: Option<String>,
    /// Caps profile string ("high", "main-10", …). None = unknown
    /// (pre-extension row) — negotiation is unknown-permissive.
    #[serde(default)]
    pub profile: Option<String>,
    /// Caps level string ("4.1"). Same unknown semantics as `profile`.
    #[serde(default)]
    pub level: Option<String>,
    /// Stream bitrate where the container states one; 0 is never stored.
    #[serde(default)]
    pub bitrate_kbps: Option<u32>,
    /// Longest gap between keyframes, from the container index at scan
    /// (MH-3). `None` = not measured: an unsupported container, an
    /// absent index, or a row scanned before this existed. Unknown is
    /// NOT zero — a caller that needs a bound must assume it could be
    /// long.
    ///
    /// A stream copy can only cut on a source keyframe, so this is what
    /// bounds the longest HLS segment and therefore the only honest
    /// `EXT-X-TARGETDURATION` (RFC 8216 §4.3.3.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_keyframe_interval_ms: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AudioStream {
    pub codec: String,
    pub channels: u32,
    pub sample_rate: u32,
    pub language: Option<String>,
    #[serde(default)]
    pub bitrate_kbps: Option<u32>,
    /// Channel mask as a hex string ("0x3f") when the caps carry one —
    /// enough to distinguish 5.1 from 6.0 later without committing to a
    /// pretty-printer now.
    #[serde(default)]
    pub layout: Option<String>,
}

/// HUB-14: what the requesting client can play. Sent with each play
/// request; per-request state, never persisted. `Default` is the
/// conservative fallback for requests without one — it reproduces the
/// pre-negotiation behavior exactly (mp4/webm direct, WEB_TARGET
/// codecs, no ceilings), so old clients and scripts lose nothing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CapabilityProfile {
    /// Containers the client demuxes natively (normalized like
    /// `MediaInfo.container`: "mp4", "webm", "matroska", …).
    pub containers: Vec<String>,
    pub video: Vec<VideoCap>,
    /// Audio codec names the client decodes ("aac", "mp3", "opus", …).
    pub audio: Vec<String>,
    /// 0 = unlimited. The web client sends 0: browsers downmix 5.1 AAC
    /// natively, and forcing stereo would re-encode every 5.1 track.
    pub max_audio_channels: u32,
    pub max_height: Option<u32>,
    pub max_fps: Option<u32>,
    /// Client will DISPLAY HDR bytes acceptably — either a real HDR
    /// pipeline, or compositor tone mapping (Chrome/Safari do this on
    /// SDR displays; Firefox does not and renders PQ washed-out).
    /// false + an hdr10 source vetoes copy/direct when the server can
    /// tone-map an encode instead (HUB-15a decision arm).
    pub hdr: bool,
    /// min()-ed with the user's stored bandwidth pref by the hub.
    pub max_bandwidth_kbps: Option<u32>,
    /// HUB-32a: client renders ASS/SSA faithfully (JASSUB).
    pub ass_render: bool,
    /// HUB-32b: client composites bitmap display sets (canvas overlay).
    pub graphics_overlay: bool,
    /// Client renders plain timed text. Every text rung — a converted
    /// SRT, a flattened ASS, an OCR-derived track — is delivered as
    /// WebVTT and nothing else, so this one bit covers all of them: no
    /// browser's `<track>` accepts SRT, and the cue-tap path feeds the
    /// same TextTrack renderer.
    ///
    /// Defaults TRUE, unlike its neighbours. `ass_render` and
    /// `graphics_overlay` default false because a client that says
    /// nothing probably cannot do them; text is the opposite — it is
    /// what the conservative fallback has always delivered, and
    /// defaulting false would silently move every quiet client to
    /// burn-in.
    pub vtt_render: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct VideoCap {
    /// Normalized codec name ("h264", "hevc", "vp9", "av1").
    pub codec: String,
    /// Highest caps profile the client decodes; None = no ceiling.
    #[serde(default)]
    pub max_profile: Option<String>,
    /// Highest caps level ("4.1"); None = no ceiling.
    #[serde(default)]
    pub max_level: Option<String>,
}

impl Default for CapabilityProfile {
    fn default() -> Self {
        Self {
            containers: vec!["mp4".into(), "webm".into()],
            video: vec![VideoCap {
                codec: "h264".into(),
                ..Default::default()
            }],
            audio: vec!["aac".into(), "mp3".into()],
            max_audio_channels: 0,
            max_height: None,
            max_fps: None,
            hdr: false,
            max_bandwidth_kbps: None,
            ass_render: false,
            graphics_overlay: false,
            vtt_render: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SubtitleStream {
    /// "srt", "ass", "pgs", "vobsub", "webvtt", …
    pub format: String,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SidecarSubtitle {
    /// Path relative to the collection root (same keying as the media file).
    pub path_rel: String,
    /// "srt", "ass", "vtt" from the file extension — or "vobsub" for an
    /// `.idx`/`.sub` pair (image subtitles; `path_rel` is the .idx).
    pub format: String,
    /// Language token from the filename ("Movie.en.srt" → "en") — or,
    /// for vobsub, the track's own `id:` from inside the .idx.
    pub language: Option<String>,
    /// VobSub only: the track index within the .idx (one sidecar file
    /// can carry many languages; each becomes its own entry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<u32>,
}

/// One mediahost collection as configured (name, media type, roots).
/// Lives in core because every binary parses the full config file —
/// including builds that carry no mediahost module at all.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionConfig {
    pub name: String,
    pub media_type: String,
    pub roots: Vec<std::path::PathBuf>,
}
