//! HUB-14/HUB-15: the negotiation engine. A pure function from (client
//! capability profile, source stream facts, track selection) to a
//! per-elementary-stream plan — no I/O, no GStreamer state beyond the
//! encoder/muxer availability probes remux.rs already caches. The hub
//! runs it against EVERY candidate source and picks the cheapest
//! (HUB-16: direct > copy-remux > audio-encode > video-encode).
//!
//! Unknown-permissive: a source probed before the MH-3 extension has no
//! profile/level/bitrate/HDR facts. Missing facts never veto a copy —
//! the codec-name gate always applies, and re-encoding entire
//! un-rescanned libraries to defend against a hypothetical
//! profile-mismatch would be a worse failure than the occasional
//! visible playback error. HDR `None` reads as SDR.

use kahawai_core::media::{CapabilityProfile, MediaInfo};

use crate::remux::{
    RemuxPlan, StreamMode, aac_encoder, can_decode, codec_to_caps_name, h264_encoder, plan_summary,
    ts_muxable_names,
};

/// HUB-16 cost order; `Ord` IS the preference (smaller = cheaper).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cost {
    Direct,
    Copy,
    AudioEncode,
    VideoEncode,
    Unplayable,
}

#[derive(Debug, Clone)]
pub struct SourcePlan {
    /// Serve bytes as-is; `plan` is still filled for verdict text.
    pub direct: bool,
    pub plan: RemuxPlan,
    /// A VobSub sidecar burn the pick forced (index into
    /// `external_subtitles`). Hub-internal: the plan carries no
    /// embedded index to walk — the caller fetches the sidecar's
    /// display sets and hands them to the pipeline.
    pub burn_sidecar: Option<usize>,
    pub cost: Cost,
    pub video_verdict: String,
    pub audio_verdict: String,
    pub subtitles: Vec<SubtitleVerdict>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SubtitleVerdict {
    pub index: usize,
    /// The unified track id (hub fills it in; the negotiation itself
    /// only knows stream indexes).
    pub track_id: Option<i64>,
    pub format: String,
    pub language: Option<String>,
    pub tier: SubtitleTier,
    pub note: &'static str,
}

/// HUB-32a/b/c policy order as data: bitmap streaming → OCR text →
/// burn-in, `Unavailable` when none of them can happen.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleTier {
    Text,
    Convert,
    Graphics,
    /// HUB-32c: this image stream is served as an OCR-derived text
    /// track (listed separately, machine-derived) — no video encode.
    Ocr,
    /// HUB-32b last resort: composited into the picture by the encoder.
    Burn,
    Unavailable,
}

/// An explicit image track picked for burn-in (subtitle unification).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BurnPick {
    /// Index into `info.subtitles`.
    Embedded(usize),
    /// Index into `info.external_subtitles` (a VobSub sidecar): its
    /// display sets are handed to the pipeline by the caller, so the
    /// plan carries no embedded index.
    Sidecar(usize),
}

/// H.264 caps profile strings in capability order: a client that
/// decodes an entry decodes everything before it.
const H264_PROFILES: &[&str] = &[
    "constrained-baseline",
    "baseline",
    "main",
    "high",
    "high-10",
    "high-4:2:2",
    "high-4:4:4",
];

/// HEVC order (the short list that occurs in the wild).
const HEVC_PROFILES: &[&str] = &["main", "main-still-picture", "main-10"];

fn profile_rank(codec: &str, profile: &str) -> Option<usize> {
    let table = match codec {
        "h264" => H264_PROFILES,
        "hevc" => HEVC_PROFILES,
        _ => return None,
    };
    table.iter().position(|p| *p == profile)
}

/// "4.1" → 41; unparseable → None (permissive).
fn level_num(level: &str) -> Option<u32> {
    let mut parts = level.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next().map_or(Some(0), |m| m.parse().ok())?;
    Some(major * 10 + minor)
}

/// Does one declared capability admit this stream? Codec name is a
/// hard gate; profile/level compare only when BOTH sides state one
/// (unknown-permissive on either side).
fn cap_admits(
    codec: &str,
    cap: &kahawai_core::media::VideoCap,
    v: &kahawai_core::media::VideoStream,
) -> bool {
    if let (Some(have), Some(max)) = (v.profile.as_deref(), cap.max_profile.as_deref())
        && let (Some(h), Some(m)) = (profile_rank(codec, have), profile_rank(codec, max))
        && h > m
    {
        return false;
    }
    if let (Some(have), Some(max)) = (v.level.as_deref(), cap.max_level.as_deref())
        && let (Some(h), Some(m)) = (level_num(have), level_num(max))
        && h > m
    {
        return false;
    }
    true
}

/// Does the client's video capability admit this source stream for
/// COPY? A profile may carry SEVERAL caps per codec (a generic family
/// entry plus source-aware exact probes) — ANY admitting cap suffices,
/// which is what lets a precise "High 4.1 verified" coexist with the
/// family floor without weakening either.
fn video_fits(profile: &CapabilityProfile, v: &kahawai_core::media::VideoStream) -> bool {
    if !profile
        .video
        .iter()
        .filter(|c| c.codec == v.codec)
        .any(|c| cap_admits(&v.codec, c, v))
    {
        return false;
    }
    if let Some(max_h) = profile.max_height
        && v.height > max_h
    {
        return false;
    }
    if let (Some(max_fps), Some((n, d))) = (profile.max_fps, v.fps)
        && d > 0
        && n / d > max_fps
    {
        return false;
    }
    true
}

/// Does this client need an image subtitle burned in? (HUB-32b: it
/// cannot composite one, and the source carries one.) Decided before
/// the plan, because it vetoes direct play and copy alike.
fn burn_wanted(
    profile: &CapabilityProfile,
    info: &MediaInfo,
    burn_capable: bool,
    ocr_text: &[bool],
) -> bool {
    burn_capable
        && !profile.graphics_overlay
        && info.subtitles.iter().enumerate().any(|(i, s)| {
            matches!(s.format.as_str(), "pgs" | "vobsub" | "dvdsub")
                && !ocr_text.get(i).copied().unwrap_or(false)
        })
}

/// The whole decision, one source. `est_kbps` is the hub's aggregate
/// size×8/duration estimate — the only bitrate available for
/// pre-extension rows; used solely when a cap is set. `tonemap` is the
/// hub's answer to "can the box that would run an encode tone-map HDR"
/// (HUB-15a) — a server fact, not a client capability. `burn_capable`
/// is the same kind of fact for HUB-32b: whether this source's
/// display-set timeline can actually be read where the encode will
/// run (index walks are disk-speed locally and round-trip-bound over
/// a lease — measured at ~4 KB/s, which no session can wait for).
// The argument list IS the decision's input space (HUB-14/15): profile,
// source, track choices, and the per-box facts. Bundling them into a
// struct would just move the same eight names one hop away.
#[allow(clippy::too_many_arguments)]
pub fn negotiate(
    profile: &CapabilityProfile,
    info: &MediaInfo,
    audio_track: usize,
    video_track: usize,
    single_part: bool,
    est_kbps: Option<u32>,
    tonemap: bool,
    burn_capable: bool,
    // Per-subtitle-stream: a cached OCR text track exists (HUB-32c).
    // Indexes align with `info.subtitles`; short slices read as false.
    ocr_text: &[bool],
    // Explicit user pick of an image track to burn (subtitle
    // unification): overrides BOTH the overlay preference and the
    // OCR-spares-burn rule — the user asked for pixels, so the video
    // encode that carries them is forced. Still needs `burn_capable`;
    // picks that name a non-image track are ignored.
    burn_pick: Option<BurnPick>,
) -> SourcePlan {
    let audio_track = audio_track.min(info.audio.len().saturating_sub(1));
    let video_track = video_track.min(info.video.len().saturating_sub(1));
    let names = ts_muxable_names();
    let muxable = |kind: &str, codec: &str| {
        codec_to_caps_name(kind, codec).is_some_and(|n| names.contains(n))
    };
    let cap = profile.max_bandwidth_kbps;
    let over_cap = cap.is_some_and(|c| est_kbps.is_some_and(|e| e > c));

    let v = info.video.get(video_track);
    let a = info.audio.get(audio_track);

    // Copy admissibility per stream, before muxability (direct play
    // has no muxer; TS-muxability gates only the remux path).
    //
    // HUB-15a decision arm: an hdr10 copy to a client that cannot
    // display HDR is only admissible when we cannot do better. With a
    // tone-map-capable executor the encode wins — Firefox decodes HEVC
    // but renders PQ untouched (washed out); Chrome/Safari tone-map in
    // their compositor and declare hdr:true, keeping their copies.
    // Without a capable box, the copy stands (washed beats a washed
    // encode with generation loss). HLG never vetoes: it is
    // SDR-compatible by design.
    let hdr_veto = tonemap && !profile.hdr && v.is_some_and(|s| s.hdr.as_deref() == Some("hdr10"));
    let v_client_ok = v.is_some_and(|v| video_fits(profile, v)) && !over_cap && !hdr_veto;
    let a_client_ok = a.is_some_and(|a| {
        profile.audio.contains(&a.codec)
            && (profile.max_audio_channels == 0 || a.channels <= profile.max_audio_channels)
    });

    // Direct: the container itself plus every selected stream fits.
    let container_ok = info
        .container
        .as_deref()
        .is_some_and(|c| profile.containers.iter().any(|p| p == c));
    let forced_burn = burn_pick.filter(|p| {
        burn_capable
            && match p {
                BurnPick::Embedded(i) => info
                    .subtitles
                    .get(*i)
                    .is_some_and(|s| matches!(s.format.as_str(), "pgs" | "vobsub" | "dvdsub")),
                BurnPick::Sidecar(i) => info
                    .external_subtitles
                    .get(*i)
                    .is_some_and(|s| s.format == "vobsub"),
            }
    });
    let forced_embedded = match forced_burn {
        Some(BurnPick::Embedded(i)) => Some(i),
        _ => None,
    };
    let direct = single_part
        && container_ok
        && (v.is_none() || v_client_ok)
        && (a.is_none() || a_client_ok)
        && (v.is_some() || a.is_some())
        // Serving the file as-is cannot burn anything into it.
        && !burn_wanted(profile, info, burn_capable, ocr_text)
        && forced_burn.is_none();

    // HUB-32b last resort: an image subtitle a client cannot composite
    // is burned into the picture. The policy is fidelity-first — the
    // client gets its subtitles — so this FORCES the video encode that
    // carries them, turning a direct play or a copy into a transcode.
    // Only the first such track: burning two on top of each other is
    // never what anyone means.
    // ... unless an OCR text track already exists for it (HUB-32c): the
    // tier order is bitmap → OCR text → burn, so a track with OCR text
    // is served as text and forces nothing.
    let burn_subtitle = forced_embedded.or_else(|| {
        (!profile.graphics_overlay && burn_capable)
            .then(|| {
                info.subtitles.iter().enumerate().position(|(i, s)| {
                    matches!(s.format.as_str(), "pgs" | "vobsub" | "dvdsub")
                        && !ocr_text.get(i).copied().unwrap_or(false)
                })
            })
            .flatten()
    });

    // Remux/transcode verdict per stream.
    // An encode is only admissible when the client accepts its TARGET.
    // This used to be skipped ("everything plays h264/aac"), which made
    // the capability mask a liar: a profile without h264 still received
    // h264, so the branch a codec-less client would take was untestable
    // — and a real client without the codec would get an unwatchable
    // stream with a confident verdict (found via the mask, HUB-14).
    let client_takes_h264 = profile.video.iter().any(|c| c.codec == "h264");
    let client_takes_aac = profile.audio.iter().any(|c| c == "aac");
    let burn_active = burn_subtitle.is_some() || forced_burn.is_some();
    let video = if v.is_some_and(|s| v_client_ok && muxable("video", &s.codec) && !burn_active) {
        StreamMode::Copy
    } else if client_takes_h264
        && h264_encoder().is_some()
        && v.is_some_and(|s| codec_to_caps_name("video", &s.codec).is_some_and(can_decode))
    {
        StreamMode::Encode
    } else {
        StreamMode::Off
    };
    let audio = if a.is_some_and(|s| a_client_ok && muxable("audio", &s.codec)) {
        StreamMode::Copy
    } else if client_takes_aac
        && aac_encoder().is_some()
        && a.is_some_and(|s| codec_to_caps_name("audio", &s.codec).is_some_and(can_decode))
    {
        StreamMode::Encode
    } else {
        StreamMode::Off
    };

    // HUB-15a: tone-map only when the pixels get rewritten anyway (an
    // encode) and only PQ — HLG is SDR-compatible by design, and a
    // COPY of HDR to an SDR client stays a copy: browsers that decode
    // HEVC tone-map in their own compositor, better than we can.
    let tone_map = tonemap
        && video == StreamMode::Encode
        && v.is_some_and(|s| s.hdr.as_deref() == Some("hdr10"));

    let plan = RemuxPlan {
        video,
        audio,
        audio_track,
        video_track,
        video_kbps: (video == StreamMode::Encode).then(|| cap.map_or(6000, |c| 6000.min(c))),
        max_height: (video == StreamMode::Encode)
            .then_some(profile.max_height)
            .flatten(),
        // The client's ceiling resolved to the count the encoder should
        // actually produce: a stereo-capable client gets stereo, and a
        // source with fewer channels than the ceiling keeps its own
        // (never upmix). Unknown source count (pre-extension row) falls
        // back to the ceiling itself.
        // Only claim the burn when the encode that carries it exists.
        burn_subtitle: burn_subtitle.filter(|_| video == StreamMode::Encode),
        max_channels: (audio == StreamMode::Encode && profile.max_audio_channels > 0).then(|| {
            a.map(|s| s.channels)
                .filter(|c| *c > 0)
                .map_or(profile.max_audio_channels, |c| {
                    c.min(profile.max_audio_channels)
                })
        }),
        tone_map,
    };

    let cost = if direct {
        Cost::Direct
    } else if !plan.playable() || (v.is_some() && video == StreamMode::Off) {
        // A source WITH video whose video cannot be delivered is
        // unplayable, full stop — an audio-only stream of a film with a
        // confident verdict is worse than a refusal. (Audio-less
        // sources still play as video-only, and music has no video row
        // to lose.)
        Cost::Unplayable
    } else if video == StreamMode::Encode {
        Cost::VideoEncode
    } else if audio == StreamMode::Encode {
        Cost::AudioEncode
    } else {
        Cost::Copy
    };

    // Verdicts: the established plan_summary strings, plus negotiation
    // notes nothing else can know.
    let (mut video_verdict, mut audio_verdict) = plan_summary(info, &plan);
    if direct {
        video_verdict = match v {
            Some(s) => format!("{} direct", s.codec),
            None => "none".into(),
        };
    }
    // Off because the client refuses the encode TARGET: name the actual
    // blocker, not a generic "off" (this is the state the mask creates).
    if video == StreamMode::Off && !client_takes_h264 && v.is_some() && !v_client_ok {
        video_verdict = format!(
            "{} → none (client accepts neither the source nor h264)",
            v.map(|s| s.codec.as_str()).unwrap_or("video")
        );
    }
    if audio == StreamMode::Off && !client_takes_aac && a.is_some() && !a_client_ok {
        audio_verdict = format!(
            "{} → none (client accepts neither the source nor aac)",
            a.map(|s| s.codec.as_str()).unwrap_or("audio")
        );
    }
    if plan.tone_map {
        video_verdict.push_str(" · hdr10 → sdr (tone-mapped)");
    } else if let Some(s) = v
        && s.hdr.is_some()
        && !profile.hdr
        && video != StreamMode::Off
    {
        // Copies: the client's decoder tone-maps better than we can.
        // Encodes without a capable box: the honest truth (HUB-15a).
        video_verdict.push_str(" · HDR delivered as-is");
    }
    if over_cap && !direct && video == StreamMode::Encode {
        video_verdict.push_str(" · bandwidth cap");
    }

    let subtitles = info
        .subtitles
        .iter()
        .enumerate()
        .map(|(index, s)| {
            let (tier, note) = match s.format.as_str() {
                "ass" | "ssa" if profile.ass_render => (SubtitleTier::Text, ""),
                "ass" | "ssa" => (SubtitleTier::Convert, "flattened to VTT"),
                // A planned burn outranks the passive readings: an
                // explicit pick burns even for overlay-capable clients.
                "pgs" | "vobsub" | "dvdsub" if plan.burn_subtitle == Some(index) => {
                    (SubtitleTier::Burn, "burned in (forces the video encode)")
                }
                "pgs" | "vobsub" | "dvdsub" if profile.graphics_overlay => {
                    (SubtitleTier::Graphics, "")
                }
                "pgs" | "vobsub" | "dvdsub" if ocr_text.get(index).copied().unwrap_or(false) => {
                    (SubtitleTier::Ocr, "served as OCR text — no encode needed")
                }
                "pgs" | "vobsub" | "dvdsub" if burn_subtitle.is_some() => {
                    (SubtitleTier::Unavailable, "only one track can be burned in")
                }
                "pgs" | "vobsub" | "dvdsub" if !profile.graphics_overlay => (
                    SubtitleTier::Unavailable,
                    "burn-in needs a locally-read source (HUB-32b)",
                ),
                "pgs" | "vobsub" | "dvdsub" => (
                    SubtitleTier::Unavailable,
                    "no OCR text yet and no burn path",
                ),
                _ => (SubtitleTier::Text, ""),
            };
            SubtitleVerdict {
                index,
                track_id: None,
                format: s.format.clone(),
                language: s.language.clone(),
                tier,
                note,
            }
        })
        .collect();

    SourcePlan {
        direct,
        plan,
        burn_sidecar: match forced_burn {
            Some(BurnPick::Sidecar(i)) if video == StreamMode::Encode => Some(i),
            _ => None,
        },
        cost,
        video_verdict,
        audio_verdict,
        subtitles,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kahawai_core::media::{AudioStream, SubtitleStream, VideoCap, VideoStream};

    fn media(container: &str, v: Option<VideoStream>, a: Option<AudioStream>) -> MediaInfo {
        MediaInfo {
            container: Some(container.into()),
            duration_ms: Some(60_000),
            video: v.into_iter().collect(),
            audio: a.into_iter().collect(),
            ..Default::default()
        }
    }
    fn vs(codec: &str) -> VideoStream {
        VideoStream {
            codec: codec.into(),
            width: 1920,
            height: 1080,
            ..Default::default()
        }
    }
    fn au(codec: &str, channels: u32) -> AudioStream {
        AudioStream {
            codec: codec.into(),
            channels,
            sample_rate: 48000,
            ..Default::default()
        }
    }
    fn chrome() -> CapabilityProfile {
        CapabilityProfile {
            containers: vec!["mp4".into(), "webm".into()],
            video: vec![
                VideoCap {
                    codec: "h264".into(),
                    ..Default::default()
                },
                VideoCap {
                    codec: "hevc".into(),
                    ..Default::default()
                },
            ],
            audio: vec!["aac".into(), "mp3".into(), "opus".into()],
            ..Default::default()
        }
    }

    #[test]
    fn decision_table() {
        let p = chrome();
        // mp4/h264/aac: direct.
        let sp = negotiate(
            &p,
            &media("mp4", Some(vs("h264")), Some(au("aac", 2))),
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
        );
        assert_eq!(sp.cost, Cost::Direct);
        assert!(sp.direct);
        // Same streams in MKV: copy-remux (container unsupported).
        let sp = negotiate(
            &p,
            &media("matroska", Some(vs("h264")), Some(au("aac", 6))),
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
        );
        assert_eq!(sp.cost, Cost::Copy);
        assert_eq!(sp.plan.video, StreamMode::Copy);
        // DTS audio: audio encode, channels unlimited so no downmix.
        let sp = negotiate(
            &p,
            &media("matroska", Some(vs("h264")), Some(au("dts", 6))),
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
        );
        assert_eq!(sp.cost, Cost::AudioEncode);
        assert_eq!(sp.plan.max_channels, None);
        // HEVC with hevc in profile: copy; without: video encode.
        let sp = negotiate(
            &p,
            &media("matroska", Some(vs("hevc")), Some(au("aac", 2))),
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
        );
        assert_eq!(sp.plan.video, StreamMode::Copy);
        let mut no_hevc = chrome();
        no_hevc.video.retain(|c| c.codec != "hevc");
        let sp = negotiate(
            &no_hevc,
            &media("matroska", Some(vs("hevc")), Some(au("aac", 2))),
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
        );
        assert_eq!(sp.cost, Cost::VideoEncode);
        assert_eq!(sp.plan.video_kbps, Some(6000));
        // Multi-part never direct, even when everything fits.
        let sp = negotiate(
            &p,
            &media("mp4", Some(vs("h264")), Some(au("aac", 2))),
            0,
            0,
            false,
            None,
            false,
            true,
            &[],
            None,
        );
        assert_ne!(sp.cost, Cost::Direct);
    }

    #[test]
    fn ceilings_and_caps() {
        // 2160p against a 1080 ceiling: encode with scale.
        let mut p = chrome();
        p.max_height = Some(1080);
        let big = VideoStream {
            height: 2160,
            width: 3840,
            ..vs("h264")
        };
        let sp = negotiate(
            &p,
            &media("matroska", Some(big), Some(au("aac", 2))),
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
        );
        assert_eq!(sp.cost, Cost::VideoEncode);
        assert_eq!(sp.plan.max_height, Some(1080));
        // Profile ceiling: high-10 source vs high client → encode; unknown source profile → copy.
        let mut p = chrome();
        p.video = vec![VideoCap {
            codec: "h264".into(),
            max_profile: Some("high".into()),
            max_level: None,
        }];
        let ten_bit = VideoStream {
            profile: Some("high-10".into()),
            ..vs("h264")
        };
        let sp = negotiate(
            &p,
            &media("matroska", Some(ten_bit), Some(au("aac", 2))),
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
        );
        assert_eq!(sp.cost, Cost::VideoEncode);
        let unknown = vs("h264"); // no profile field → permissive
        let sp = negotiate(
            &p,
            &media("matroska", Some(unknown), Some(au("aac", 2))),
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
        );
        assert_eq!(sp.cost, Cost::Copy, "unknown profile must not veto a copy");
        // Bandwidth cap: 14 Mbit estimate vs 8 Mbit cap → no direct, encode clamped.
        let mut p = chrome();
        p.max_bandwidth_kbps = Some(800);
        let sp = negotiate(
            &p,
            &media("mp4", Some(vs("h264")), Some(au("aac", 2))),
            0,
            0,
            true,
            Some(14000),
            false,
            true,
            &[],
            None,
        );
        assert_ne!(sp.cost, Cost::Direct);
        assert_eq!(sp.plan.video_kbps, Some(800));
        // Channel limit: 5.1 aac vs max 2 → encode with downmix.
        let mut p = chrome();
        p.max_audio_channels = 2;
        let sp = negotiate(
            &p,
            &media("matroska", Some(vs("h264")), Some(au("aac", 6))),
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
        );
        assert_eq!(sp.cost, Cost::AudioEncode);
        assert_eq!(sp.plan.max_channels, Some(2));
    }

    #[test]
    fn hdr_and_subtitles_speak_in_verdicts() {
        let p = chrome(); // hdr: false
        let hdr = VideoStream {
            hdr: Some("hdr10".into()),
            ..vs("h264")
        };
        let mut info = media("matroska", Some(hdr), Some(au("aac", 2)));
        info.subtitles = vec![
            SubtitleStream {
                format: "srt".into(),
                language: None,
            },
            SubtitleStream {
                format: "ass".into(),
                language: None,
            },
            SubtitleStream {
                format: "pgs".into(),
                language: None,
            },
        ];
        // This fixture's client can composite, so the PGS track does
        // NOT drag the plan into an encode (burn-in has its own test).
        let mut p = p;
        p.graphics_overlay = true;
        // No capable box: the copy stands, the verdict says as-is.
        let sp = negotiate(&p, &info, 0, 0, true, None, false, true, &[], None);
        assert_eq!(
            sp.cost,
            Cost::Copy,
            "without tone-map capability the copy stands"
        );
        assert!(
            sp.video_verdict.contains("as-is"),
            "verdict: {}",
            sp.video_verdict
        );
        assert!(!sp.plan.tone_map, "copies never tone-map");
        assert_eq!(sp.subtitles[0].tier, SubtitleTier::Text);
        assert_eq!(
            sp.subtitles[1].tier,
            SubtitleTier::Convert,
            "no ass_render → flatten"
        );
        assert_eq!(sp.subtitles[2].tier, SubtitleTier::Graphics);
        let mut able = chrome();
        able.ass_render = true;
        able.graphics_overlay = true;
        let sp = negotiate(&able, &info, 0, 0, true, None, false, true, &[], None);
        assert_eq!(sp.subtitles[1].tier, SubtitleTier::Text);
        assert_eq!(sp.subtitles[2].tier, SubtitleTier::Graphics);
    }

    /// The channel ceiling resolves against the SOURCE: a stereo
    /// client gets 2 off a 5.1 track, and a mono track stays mono
    /// rather than being upmixed to fill the ceiling.
    #[test]
    fn channel_ceiling_resolves_against_the_source() {
        let mut p = chrome();
        p.max_audio_channels = 2;
        let sp = negotiate(
            &p,
            &media("matroska", Some(vs("h264")), Some(au("dts", 6))),
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
        );
        assert_eq!(sp.plan.audio, StreamMode::Encode);
        assert_eq!(sp.plan.max_channels, Some(2), "5.1 → the client's ceiling");

        let sp = negotiate(
            &p,
            &media("matroska", Some(vs("h264")), Some(au("dts", 1))),
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
        );
        assert_eq!(
            sp.plan.max_channels,
            Some(1),
            "mono source is not upmixed to the ceiling"
        );

        // Unlimited (the web client's own declaration) imposes nothing.
        p.max_audio_channels = 0;
        let sp = negotiate(
            &p,
            &media("matroska", Some(vs("h264")), Some(au("dts", 6))),
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
        );
        assert_eq!(sp.plan.max_channels, None);
    }

    /// HUB-32b: a client that cannot composite gets image subtitles
    /// burned in, and that forces the encode which carries them — a
    /// copy or a direct play cannot have anything burned into it.
    #[test]
    fn image_subs_burn_in_and_force_the_encode() {
        let mut p = chrome();
        p.graphics_overlay = false;
        // A source that would otherwise be a clean copy.
        let mut info = media("matroska", Some(vs("h264")), Some(au("aac", 2)));
        info.subtitles = vec![SubtitleStream {
            format: "pgs".into(),
            language: None,
        }];

        let sp = negotiate(&p, &info, 0, 0, true, None, false, true, &[], None);
        assert_eq!(
            sp.cost,
            Cost::VideoEncode,
            "burn-in forces the encode: {}",
            sp.video_verdict
        );
        assert_eq!(sp.plan.burn_subtitle, Some(0));
        assert_eq!(sp.subtitles[0].tier, SubtitleTier::Burn);

        // Direct play is off the table for the same reason.
        let mut mp4 = info.clone();
        mp4.container = Some("mp4".into());
        let sp = negotiate(&p, &mp4, 0, 0, true, None, false, true, &[], None);
        assert!(!sp.direct, "cannot burn into a file served as-is");

        // A compositing client keeps its cheap path and its overlay.
        p.graphics_overlay = true;
        let sp = negotiate(&p, &info, 0, 0, true, None, false, true, &[], None);
        assert_eq!(sp.cost, Cost::Copy);
        assert_eq!(sp.plan.burn_subtitle, None);
        assert_eq!(sp.subtitles[0].tier, SubtitleTier::Graphics);

        // HUB-32c: with an OCR text track cached, the non-compositing
        // client is served text — the copy survives, nothing is burned.
        p.graphics_overlay = false;
        let sp = negotiate(&p, &info, 0, 0, true, None, false, true, &[true], None);
        assert_eq!(
            sp.cost,
            Cost::Copy,
            "OCR text spares the encode: {}",
            sp.video_verdict
        );
        assert_eq!(sp.plan.burn_subtitle, None);
        assert_eq!(sp.subtitles[0].tier, SubtitleTier::Ocr);

        // And direct play comes back for the same reason.
        let mut mp4 = info.clone();
        mp4.container = Some("mp4".into());
        let sp = negotiate(&p, &mp4, 0, 0, true, None, false, true, &[true], None);
        assert!(sp.direct, "OCR text restores direct play");

        // The compositing client is unaffected: overlay beats text.
        p.graphics_overlay = true;
        let sp = negotiate(&p, &info, 0, 0, true, None, false, true, &[true], None);
        assert_eq!(sp.subtitles[0].tier, SubtitleTier::Graphics);
    }

    /// Subtitle unification: an explicit burn pick beats BOTH sparing
    /// rules — OCR text existing, and the client compositing overlays.
    /// The user asked for pixels; the encode that carries them happens.
    #[test]
    fn explicit_burn_pick_overrides_the_sparing_rules() {
        let mut p = chrome();
        p.graphics_overlay = true; // a compositing client — would normally spare
        let mut info = media("matroska", Some(vs("h264")), Some(au("aac", 2)));
        info.subtitles = vec![SubtitleStream {
            format: "pgs".into(),
            language: None,
        }];

        let sp = negotiate(
            &p,
            &info,
            0,
            0,
            true,
            None,
            false,
            true,
            &[true],
            Some(BurnPick::Embedded(0)),
        );
        assert_eq!(sp.cost, Cost::VideoEncode, "{}", sp.video_verdict);
        assert_eq!(sp.plan.burn_subtitle, Some(0));
        assert_eq!(sp.subtitles[0].tier, SubtitleTier::Burn);
        assert!(!sp.direct, "cannot burn into a file served as-is");

        // Without the capability fact the pick is ignored, not honored
        // dishonestly.
        let sp = negotiate(
            &p,
            &info,
            0,
            0,
            true,
            None,
            false,
            false,
            &[],
            Some(BurnPick::Embedded(0)),
        );
        assert_eq!(sp.plan.burn_subtitle, None);

        // A non-image index is ignored too.
        let sp = negotiate(
            &p,
            &info,
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            Some(BurnPick::Embedded(7)),
        );
        assert_eq!(sp.plan.burn_subtitle, None);
        assert_eq!(sp.cost, Cost::Copy);

        // A VobSub sidecar pick burns too: its sets are handed to the
        // pipeline by the caller, so the plan carries no embedded index
        // — `burn_sidecar` tells the hub what to fetch.
        let mut side = info.clone();
        side.subtitles.clear();
        side.external_subtitles = vec![kahawai_core::media::SidecarSubtitle {
            path_rel: "movie.idx".into(),
            format: "vobsub".into(),
            language: Some("en".into()),
            track: Some(0),
        }];
        let sp = negotiate(
            &p,
            &side,
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            Some(BurnPick::Sidecar(0)),
        );
        assert_eq!(sp.cost, Cost::VideoEncode, "{}", sp.video_verdict);
        assert_eq!(sp.plan.burn_subtitle, None);
        assert_eq!(sp.burn_sidecar, Some(0));
        assert!(!sp.direct);
    }

    /// A source whose timeline cannot be read where the encode runs
    /// must not claim a burn it will not perform: no burn, no forced
    /// encode, and a verdict that names the reason.
    #[test]
    fn burn_in_is_not_promised_when_it_cannot_be_done() {
        let mut p = chrome();
        p.graphics_overlay = false;
        let mut info = media("matroska", Some(vs("h264")), Some(au("aac", 2)));
        info.subtitles = vec![SubtitleStream {
            format: "pgs".into(),
            language: None,
        }];

        let sp = negotiate(&p, &info, 0, 0, true, None, false, false, &[], None);
        assert_eq!(sp.cost, Cost::Copy, "no capability, no gratuitous encode");
        assert_eq!(sp.plan.burn_subtitle, None);
        assert_eq!(sp.subtitles[0].tier, SubtitleTier::Unavailable);
        assert!(
            sp.subtitles[0].note.contains("locally-read"),
            "note: {}",
            sp.subtitles[0].note
        );
    }

    /// Text subtitles never trigger burn-in — they have their own
    /// tiers, and a video encode for them would be gratuitous.
    #[test]
    fn text_subs_do_not_force_burn_in() {
        let mut p = chrome();
        p.graphics_overlay = false;
        let mut info = media("matroska", Some(vs("h264")), Some(au("aac", 2)));
        info.subtitles = vec![SubtitleStream {
            format: "srt".into(),
            language: None,
        }];
        let sp = negotiate(&p, &info, 0, 0, true, None, false, true, &[], None);
        assert_eq!(sp.cost, Cost::Copy);
        assert_eq!(sp.plan.burn_subtitle, None);
    }

    /// HUB-15a decision arm: PQ + encode + capable box → tone-map;
    /// no capable box, or HLG, or a copy → not.
    #[test]
    fn tonemap_arm_gates_on_encode_pq_and_capability() {
        let mut no_hevc = chrome();
        no_hevc.video.retain(|c| c.codec != "hevc");
        let pq = VideoStream {
            hdr: Some("hdr10".into()),
            ..vs("hevc")
        };
        let info = media("matroska", Some(pq), Some(au("aac", 2)));

        let sp = negotiate(&no_hevc, &info, 0, 0, true, None, true, true, &[], None);
        assert_eq!(sp.cost, Cost::VideoEncode);
        assert!(sp.plan.tone_map);
        assert!(
            sp.video_verdict.contains("tone-mapped"),
            "verdict: {}",
            sp.video_verdict
        );

        let sp = negotiate(&no_hevc, &info, 0, 0, true, None, false, true, &[], None);
        assert!(!sp.plan.tone_map, "no capable box → encode as-is");
        assert!(
            sp.video_verdict.contains("as-is"),
            "verdict: {}",
            sp.video_verdict
        );

        let hlg = VideoStream {
            hdr: Some("hlg".into()),
            ..vs("hevc")
        };
        let info = media("matroska", Some(hlg), Some(au("aac", 2)));
        let sp = negotiate(&no_hevc, &info, 0, 0, true, None, true, true, &[], None);
        assert!(
            !sp.plan.tone_map,
            "HLG is SDR-compatible by design — no map"
        );
    }

    /// The Firefox case: the client DECODES hevc but cannot DISPLAY
    /// hdr10 (profile.hdr false). With a capable box the copy is
    /// vetoed and the encode tone-maps; a client declaring hdr:true
    /// (Chrome/Safari — they tone-map themselves) keeps the copy.
    #[test]
    fn hdr10_copy_vetoed_when_client_cannot_display_it() {
        let mut p = chrome(); // hdr: false, decodes hevc
        assert!(
            p.video.iter().any(|c| c.codec == "hevc"),
            "fixture must decode hevc"
        );
        let pq = VideoStream {
            hdr: Some("hdr10".into()),
            ..vs("hevc")
        };
        let info = media("matroska", Some(pq), Some(au("aac", 2)));

        let sp = negotiate(&p, &info, 0, 0, true, None, true, true, &[], None);
        assert_eq!(
            sp.cost,
            Cost::VideoEncode,
            "copyable codec still encodes: {}",
            sp.video_verdict
        );
        assert!(sp.plan.tone_map);
        assert!(
            sp.video_verdict.contains("tone-mapped"),
            "verdict: {}",
            sp.video_verdict
        );

        p.hdr = true;
        let sp = negotiate(&p, &info, 0, 0, true, None, true, true, &[], None);
        assert_eq!(sp.cost, Cost::Copy, "hdr-capable client keeps the copy");
        assert!(!sp.plan.tone_map);

        // HLG never vetoes a copy — SDR-compatible by design.
        p.hdr = false;
        let hlg = VideoStream {
            hdr: Some("hlg".into()),
            ..vs("hevc")
        };
        let info = media("matroska", Some(hlg), Some(au("aac", 2)));
        let sp = negotiate(&p, &info, 0, 0, true, None, true, true, &[], None);
        assert_eq!(sp.cost, Cost::Copy);
    }

    /// Multiple caps per codec compose: an exact source-aware probe and
    /// the generic family floor each admit what THEY verified.
    #[test]
    fn any_cap_per_codec_admits() {
        let mut p = chrome();
        // Precise-only declaration: High 4.1 verified, nothing else.
        p.video = vec![VideoCap {
            codec: "h264".into(),
            max_profile: Some("high".into()),
            max_level: Some("4.1".into()),
        }];
        let ok = VideoStream {
            profile: Some("high".into()),
            level: Some("4.1".into()),
            ..vs("h264")
        };
        let sp = negotiate(
            &p,
            &media("matroska", Some(ok), Some(au("aac", 2))),
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
        );
        assert_eq!(sp.plan.video, StreamMode::Copy);
        let over = VideoStream {
            profile: Some("high-10".into()),
            level: Some("4.1".into()),
            ..vs("h264")
        };
        let sp = negotiate(
            &p,
            &media("matroska", Some(over), Some(au("aac", 2))),
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
        );
        assert_eq!(
            sp.cost,
            Cost::VideoEncode,
            "the precise cap must reject high-10"
        );
        // Adding a second, higher cap for the same codec admits it.
        p.video.push(VideoCap {
            codec: "h264".into(),
            max_profile: Some("high-10".into()),
            max_level: Some("4.1".into()),
        });
        let over = VideoStream {
            profile: Some("high-10".into()),
            level: Some("4.1".into()),
            ..vs("h264")
        };
        let sp = negotiate(
            &p,
            &media("matroska", Some(over), Some(au("aac", 2))),
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
        );
        assert_eq!(
            sp.plan.video,
            StreamMode::Copy,
            "any admitting cap suffices"
        );
    }

    /// The mask's sharpest edge (HUB-14): a client that does not list
    /// the encode target gets a REFUSAL, not the target anyway. This
    /// was designed wrong originally — "everything plays h264/aac" —
    /// which made a masked h264 undetectable and a real codec-less
    /// client unwatchable-with-a-confident-verdict.
    #[test]
    fn no_encode_target_in_profile_means_no_encode() {
        let mut p = chrome();
        p.video.retain(|c| c.codec != "h264");
        // HEVC source the client also cannot copy (profile lists only
        // h264-family after the mask): encode is the only path, and the
        // client refuses its target.
        let mut hevc = chrome();
        hevc.video.retain(|_| false);
        let info = media("matroska", Some(vs("hevc")), Some(au("aac", 2)));
        let mut q = chrome();
        q.video.retain(|c| c.codec != "hevc" && c.codec != "h264");
        let sp = negotiate(&q, &info, 0, 0, true, None, false, true, &[], None);
        assert_eq!(sp.plan.video, StreamMode::Off, "{}", sp.video_verdict);
        assert_eq!(sp.cost, Cost::Unplayable);
        assert!(
            sp.video_verdict.contains("client accepts neither"),
            "verdict must name the blocker: {}",
            sp.video_verdict
        );

        // Audio side: dts source, aac masked out — audio goes Off while
        // the video copy stands.
        let mut r = chrome();
        r.audio.retain(|a| a != "aac");
        let info = media("matroska", Some(vs("h264")), Some(au("dts", 6)));
        let sp = negotiate(&r, &info, 0, 0, true, None, false, true, &[], None);
        assert_eq!(sp.plan.audio, StreamMode::Off, "{}", sp.audio_verdict);
        assert!(
            sp.audio_verdict.contains("client accepts neither"),
            "verdict must name the blocker: {}",
            sp.audio_verdict
        );
    }

    /// The fallback profile reproduces plan_streams(WEB_TARGET) on the
    /// remux path — profileless requests lose nothing.
    #[test]
    fn default_profile_matches_web_target() {
        let p = CapabilityProfile::default();
        for (v, a) in [
            (Some(vs("h264")), Some(au("aac", 6))),
            (Some(vs("hevc")), Some(au("aac", 2))),
            (Some(vs("h264")), Some(au("dts", 6))),
            (Some(vs("h264")), Some(au("flac", 2))),
        ] {
            let info = media("matroska", v, a);
            let old = crate::remux::plan_streams(&info, &crate::remux::WEB_TARGET, 0, 0);
            let new = negotiate(&p, &info, 0, 0, true, None, false, true, &[], None);
            assert_eq!(new.plan.video, old.video, "video parity for {info:?}");
            assert_eq!(new.plan.audio, old.audio, "audio parity for {info:?}");
        }
    }
}
