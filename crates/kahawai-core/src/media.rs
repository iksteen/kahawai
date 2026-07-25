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
    // ponytail: HDR metadata (HDR10/HLG/DoVi, MH-3) not extracted yet — add
    // when negotiation grows tone-mapping decisions.
    pub hdr: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AudioStream {
    pub codec: String,
    pub channels: u32,
    pub sample_rate: u32,
    pub language: Option<String>,
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
    /// "srt", "ass", "vtt" — from the file extension.
    pub format: String,
    /// Language token from the filename ("Movie.en.srt" → "en"), verbatim.
    pub language: Option<String>,
}
