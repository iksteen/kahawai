//! Normalized technical stream model (MH-3) — what a mediahost reports per
//! file and what capability negotiation later consumes. Codec names are
//! lowercase normalized strings ("h264", "hevc", "aac", …).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

fn is_false(value: &bool) -> bool {
    !*value
}

fn deserialize_nonnegative_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let value = Option::<i64>::deserialize(deserializer)?;
    match value {
        Some(value) if value >= 0 => u32::try_from(value)
            .map(Some)
            .map_err(|_| D::Error::custom("integer exceeds u32")),
        // Legacy indexers wrote -1 for an unsupported measurement. It means
        // unknown, just like null; accepting it here lets unrelated targeted
        // updates preserve and normalize those deployed source rows.
        Some(_) | None => Ok(None),
    }
}

/// A file's own loudness statement (HUB-19).
///
/// Gains are dB to apply; peaks are linear sample values where 1.0 is
/// full scale, so a player can turn a track down without clipping it —
/// the peak is what says whether a positive gain is safe.
///
/// Album values exist so a record plays with its own dynamics intact:
/// applying per-track gain across an album flattens the quiet tracks
/// the artist meant to be quiet. A client that has both should prefer
/// album when playing an album and track when shuffling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ReplayGain {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_gain_db: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_peak: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_gain_db: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_peak: Option<f64>,
    /// The loudness the gains aim at, when the file says. ReplayGain 1.0
    /// files usually mean 89 dB SPL; absent is the common case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_level_db: Option<f64>,
}

impl ReplayGain {
    /// None when the file carried no usable number — which is how an
    /// untagged file stays absent from the payload instead of arriving
    /// as an object full of nulls.
    pub fn some(self) -> Option<Self> {
        let empty = self.track_gain_db.is_none()
            && self.track_peak.is_none()
            && self.album_gain_db.is_none()
            && self.album_peak.is_none();
        (!empty).then_some(self)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct MediaInfo {
    /// Container format, normalized ("matroska", "mp4", "webm", …).
    #[schema(required)]
    pub container: Option<String>,
    #[schema(required)]
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
    /// ReplayGain (HUB-19), as the file states it: a loudness
    /// measurement someone made once, carried to the client rather than
    /// re-measured or applied here. Absent for anything untagged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_gain: Option<ReplayGain>,
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
    /// Chapters as the container declares them, in start order:
    /// the file's own division of itself, which is a fact about the bytes
    /// and not an inference like a detected intro. Tri-state, as
    /// `attachments` is: `None` = never looked for (a row from before the
    /// field, which the backfill worklist picks up), `Some([])` = looked
    /// for and there are none, `Some([...])` = these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chapters: Option<Vec<Chapter>>,
    /// Whether the exact source was inspected for PAR/orientation. This is
    /// separate from scanning/reconciliation: old rows are filled by a
    /// targeted, idle mediahost worklist. New discovery gets it for free.
    #[serde(default, skip_serializing_if = "is_false")]
    pub video_geometry_probed: bool,
    /// A terminal targeted-probe failure. It keeps an unreadable source out of
    /// an infinite work loop and is replaced when that physical row changes or
    /// a later targeted probe succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_geometry_error: Option<String>,
}

/// One chapter of a file, on that FILE's timeline. An item assembled from
/// several parts shifts them onto its own; nothing else does.
///
/// `end_ms` is only what the container states. Matroska usually leaves it
/// out and means "until the next one starts", which a reader can work out
/// and a writer must not invent — a stated end can be earlier than the
/// next start, and that gap is a fact about the file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct Chapter {
    pub start_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ms: Option<u64>,
    /// The chapter's name, when it has one. Plenty of files number their
    /// chapters and say nothing else; a nameless chapter is still a seek
    /// point worth showing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct Attachment {
    pub file_name: String,
    pub mime_type: String,
    /// Byte offset of the raw payload within the media file.
    pub offset: u64,
    pub size: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct VideoStream {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    /// Frames per second as (numerator, denominator).
    #[schema(required)]
    pub fps: Option<(u32, u32)>,
    #[schema(required)]
    pub bit_depth: Option<u32>,
    pub interlaced: bool,
    /// "hdr10" | "hlg", from caps colorimetry (MH-3). None = SDR or a
    /// row probed before extraction existed — negotiation treats both
    /// as SDR (HUB-15a).
    #[schema(required)]
    pub hdr: Option<String>,
    /// Caps profile string ("high", "main-10", …). None = unknown
    /// (pre-extension row) — negotiation is unknown-permissive.
    #[serde(default)]
    #[schema(required)]
    pub profile: Option<String>,
    /// Caps level string ("4.1"). Same unknown semantics as `profile`.
    #[serde(default)]
    #[schema(required)]
    pub level: Option<String>,
    /// Stream bitrate where the container states one; 0 is never stored.
    #[serde(default)]
    #[schema(required)]
    pub bitrate_kbps: Option<u32>,
    /// Longest gap between keyframes, from the container index at scan
    /// (MH-3). `None` = not measured: an unsupported container, an
    /// absent index, or a row scanned before this existed. Unknown is
    /// NOT zero — a caller that needs a bound must assume it could be
    /// long.
    ///
    /// Bounds the longest HLS segment OUR SEGMENTERS produce, and so
    /// the honest `EXT-X-TARGETDURATION` (RFC 8216 §4.3.3.1, "the
    /// maximum Media Segment duration").
    ///
    /// The coupling is our tooling, not the format: `splitmuxsink` and
    /// `isofmp4mux` both close a fragment at the first keyframe past
    /// the fragment target, and a copy has no encoder to ask for one,
    /// so the source's spacing leaks into segment length. The spec
    /// itself only RECOMMENDS keyframe-aligned segments (§6.2.1
    /// "SHOULD attempt to divide… on packet and key frame
    /// boundaries"), and §3 explicitly allows a segment whose leading
    /// frames are "downloaded but possibly discarded". A segmenter
    /// that cut on a time grid would make this field irrelevant.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_nonnegative_u32"
    )]
    pub max_keyframe_interval_ms: Option<u32>,
    /// Pixel aspect ratio as an exact reduced fraction. `(1,1)` is a measured
    /// square-pixel source; `None` means this row predates geometry probing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pixel_aspect_ratio: Option<(u32, u32)>,
    /// GStreamer's normalized display transform (`rotate-0`, `rotate-90`, …,
    /// including the four `flip-rotate-*` forms). This is the transform to
    /// apply, not merely the container's raw rotation tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orientation: Option<String>,
    /// Display dimensions after pixel aspect and orientation. Coded dimensions
    /// remain in `width`/`height`; clients size the pre-play frame from these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_height: Option<u32>,
}

/// The source-owned subset returned by a targeted geometry probe.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct VideoGeometry {
    pub pixel_aspect_ratio: (u32, u32),
    pub orientation: String,
    pub display_width: u32,
    pub display_height: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_negative_keyframe_interval_is_unknown() {
        let stream: VideoStream = serde_json::from_str(
            r#"{"codec":"h264","width":1920,"height":1080,"fps":null,"bit_depth":null,"interlaced":false,"hdr":null,"profile":null,"level":null,"bitrate_kbps":null,"max_keyframe_interval_ms":-1}"#,
        )
        .unwrap();
        assert_eq!(stream.max_keyframe_interval_ms, None);
        assert!(
            !serde_json::to_string(&stream)
                .unwrap()
                .contains("max_keyframe_interval_ms")
        );
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct AudioStream {
    pub codec: String,
    pub channels: u32,
    pub sample_rate: u32,
    #[schema(required)]
    pub language: Option<String>,
    #[serde(default)]
    #[schema(required)]
    pub bitrate_kbps: Option<u32>,
    /// Channel mask as a hex string ("0x3f") when the caps carry one —
    /// enough to distinguish 5.1 from 6.0 later without committing to a
    /// pretty-printer now.
    #[serde(default)]
    #[schema(required)]
    pub layout: Option<String>,
}

/// What a client needs from `EXT-X-TARGETDURATION`, which is three
/// different needs and not a boolean.
///
/// Our segmenters close fragments at keyframes, so on a COPY the
/// declared value is bounded by the source's keyframe spacing —
/// measured here to run past 147 s. (That bound is a property of
/// `splitmuxsink`/`isofmp4mux`, not of HLS; see
/// `VideoStream::max_keyframe_interval_ms`.) RFC 8216 §4.3.3.1 makes
/// under-declaring a violation, and §6.3.3 tells a client not to start
/// within three target durations of the end, so an HONEST value on
/// such a file is conforming and still awkward. The client is the only
/// party that knows which it can live with, so it says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum TargetDuration {
    /// Don't care: declare the cheap constant and accept that segments
    /// may exceed it. What every browser has always got — hls.js does
    /// not enforce the bound — and knowingly non-conforming.
    Ignore,
    /// Must be accurate; any value is fine. The declaration follows the
    /// source, so a long-GOP file gets a long target and the client
    /// waits longer before it may start. Costs nothing at playback.
    Accurate,
    /// Must be accurate AND no larger than `max_secs`. The only mode
    /// that can force a video ENCODE — when the source's keyframes are
    /// too far apart to cut inside the ceiling, the encoder's own GOP is
    /// the only way to produce one, and the client asked for a
    /// guarantee rather than a measurement.
    Short { max_secs: u32 },
}

impl TargetDuration {
    /// The ceiling this mode imposes, if any.
    pub fn ceiling_secs(&self) -> Option<u32> {
        match self {
            Self::Short { max_secs } => Some((*max_secs).max(1)),
            _ => None,
        }
    }
}

/// HUB-14: what the requesting client can play. Sent with each play
/// request; per-request state, never persisted. `Default` is the
/// conservative fallback for requests without one — it reproduces the
/// pre-negotiation behavior exactly (mp4/webm direct, WEB_TARGET
/// codecs, no ceilings), so old clients and scripts lose nothing.
/// Every field defaults EXCEPT `target_duration`, which is required.
/// The struct-level `#[serde(default)]` used to make the whole profile
/// partial; a client must now state what it needs from the playlist,
/// because there is no answer that is right for all three kinds of
/// client and a silent default would pick one of them for it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct CapabilityProfile {
    /// Containers the client demuxes natively (normalized like
    /// `MediaInfo.container`: "mp4", "webm", "matroska", …).
    #[serde(default = "default_containers")]
    pub containers: Vec<String>,
    #[serde(default = "default_video")]
    pub video: Vec<VideoCap>,
    /// Audio codec names the client decodes ("aac", "mp3", "opus", …).
    #[serde(default = "default_audio")]
    pub audio: Vec<String>,
    /// 0 = unlimited. The web client sends 0: browsers downmix 5.1 AAC
    /// natively, and forcing stereo would re-encode every 5.1 track.
    #[serde(default)]
    pub max_audio_channels: u32,
    #[serde(default)]
    pub max_height: Option<u32>,
    #[serde(default)]
    pub max_fps: Option<u32>,
    /// Client will DISPLAY HDR bytes acceptably — either a real HDR
    /// pipeline, or compositor tone mapping (Chrome/Safari do this on
    /// SDR displays; Firefox does not and renders PQ washed-out).
    /// false + an hdr10 source vetoes copy/direct when the server can
    /// tone-map an encode instead (HUB-15a decision arm).
    #[serde(default)]
    pub hdr: bool,
    /// min()-ed with the user's stored bandwidth pref by the hub.
    #[serde(default)]
    pub max_bandwidth_kbps: Option<u32>,
    /// HUB-32a: client renders ASS/SSA faithfully (JASSUB).
    #[serde(default)]
    pub ass_render: bool,
    /// HUB-32b: client composites bitmap display sets (canvas overlay).
    #[serde(default)]
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
    #[serde(default = "default_true")]
    pub vtt_render: bool,
    /// REQUIRED. See [`TargetDuration`] — the one field with no default,
    /// because guessing it wrong is either a spec violation or an
    /// unplayable startup delay depending on which client asked.
    pub target_duration: TargetDuration,
}

fn default_containers() -> Vec<String> {
    vec!["mp4".into(), "webm".into()]
}
fn default_video() -> Vec<VideoCap> {
    vec![VideoCap {
        codec: "h264".into(),
        ..Default::default()
    }]
}
fn default_audio() -> Vec<String> {
    vec!["aac".into(), "mp3".into()]
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
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
            // The fallback for a request with NO profile at all, which
            // is the pre-negotiation behaviour this Default exists to
            // reproduce. A client that sends a profile must choose.
            target_duration: TargetDuration::Ignore,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct SubtitleStream {
    /// "srt", "ass", "pgs", "vobsub", "webvtt", …
    pub format: String,
    #[schema(required)]
    pub language: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct SidecarSubtitle {
    /// Path relative to the collection root (same keying as the media file).
    pub path_rel: String,
    /// "srt", "ass", "vtt" from the file extension — or "vobsub" for an
    /// `.idx`/`.sub` pair (image subtitles; `path_rel` is the .idx).
    pub format: String,
    /// Language token from the filename ("Movie.en.srt" → "en") — or,
    /// for vobsub, the track's own `id:` from inside the .idx.
    #[schema(required)]
    pub language: Option<String>,
    /// VobSub only: the track index within the .idx (one sidecar file
    /// can carry many languages; each becomes its own entry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<u32>,
}

/// One mediahost collection as configured (stable name, media type, roots).
///
/// The collection name is its per-mediahost identity. A root's identity is
/// derived from its normalized configured path rather than another operator-
/// maintained name; see [`root_token`]. Config loading normalizes and validates
/// the paths before any mediahost worker sees this type.
///
/// Lives in core because every binary parses the full config file — including
/// builds that carry no mediahost module at all.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionConfig {
    pub name: String,
    pub media_type: String,
    pub roots: Vec<std::path::PathBuf>,
}

/// Exact identity of one configured collection root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionRoot {
    pub token: String,
    pub path: std::path::PathBuf,
}

impl CollectionConfig {
    pub fn resolved_roots(&self) -> impl Iterator<Item = CollectionRoot> + '_ {
        self.roots.iter().map(|path| CollectionRoot {
            token: root_token(path),
            path: path.clone(),
        })
    }
}

/// Resolve a configured path against its config directory and normalize it
/// lexically, without consulting the filesystem.
pub fn normalize_root_path(
    path: &std::path::Path,
    config_dir: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    use std::path::Component;

    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_dir.join(path)
    };
    if !path.is_absolute() {
        return Err(format!(
            "root path is not absolute after config resolution: {}",
            path.display()
        ));
    }
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(format!(
                        "root path escapes its filesystem root: {}",
                        path.display()
                    ));
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    Ok(out)
}

/// Deterministic root identity from the normalized configured path.
///
/// Full SHA-256 is retained. Configured paths come from TOML strings and are
/// therefore UTF-8; config validation guarantees an absolute, lexically
/// normalized input before this is called.
pub fn root_token(path: &std::path::Path) -> String {
    use data_encoding::BASE64URL_NOPAD;
    use sha2::{Digest, Sha256};

    let path = path
        .to_str()
        .expect("configured collection roots are UTF-8 TOML strings");
    let mut hash = Sha256::new();
    hash.update(b"kahawai-root-path-v1");
    hash.update([0]);
    hash.update(path.as_bytes());
    format!("root-sha256-{}", BASE64URL_NOPAD.encode(&hash.finalize()))
}

#[cfg(test)]
mod root_identity_tests {
    use super::*;

    #[test]
    fn root_token_vector_is_stable_and_full_width() {
        let token = root_token(std::path::Path::new("/srv/media/movies"));
        assert_eq!(
            token,
            "root-sha256-3RBdn0tNKZrWf3uzPvhpTAjPkGYwOkF2L1ql5BR_8Dc"
        );
        assert_eq!(token.len(), "root-sha256-".len() + 43);
    }
}
