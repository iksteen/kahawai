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
    AudioTarget, RemuxPlan, SegmentFormat, StreamMode, VideoTarget, can_decode, codec_to_caps_name,
    plan_summary,
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
    /// HUB-32a: a sidecar `.ass` to burn (index into
    /// `external_subtitles`). Hub-internal for the same reason as
    /// `burn_sidecar` — the worker is handed the FILE, not an index,
    /// because it has no way to reach the media's neighbourhood.
    pub burn_ass_sidecar: Option<usize>,
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

/// HUB-32a: what the ASS fallback tier can and should do. The two only
/// mean anything together — a preference no box can honour is a
/// refusal, not a quiet flatten (owner decision), and a capable box
/// with the preference unset still flattens.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AssBurn {
    /// The box that would run the encode reports `assrender` (TC-1).
    /// `assrender` is absent on the mac mini and present on both Linux
    /// boxes, so this is a real filter, not a formality.
    pub capable: bool,
    /// The user's `ass_fallback` preference says burn rather than
    /// flatten. Per-user and global; there is no server default.
    pub preferred: bool,
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

/// Does this client need an ASS track burned in? (HUB-32a: it cannot
/// render ASS, the user prefers burning to flattening, and a box can.)
/// Like [`burn_wanted`] it is decided before the plan, because it
/// vetoes direct play and copy alike.
fn ass_burn_wanted(profile: &CapabilityProfile, info: &MediaInfo, ass_burn: AssBurn) -> bool {
    ass_burn.capable
        && ass_burn.preferred
        && !profile.ass_render
        && (info
            .subtitles
            .iter()
            .any(|s| matches!(s.format.as_str(), "ass" | "ssa"))
            // A user's own .ass counts: some releases carry no embedded
            // subtitles at all and the sidecar IS the subtitle track.
            || info
                .external_subtitles
                .iter()
                .any(|s| matches!(s.format.as_str(), "ass" | "ssa")))
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
    // HUB-32a: the ASS fallback tier's capability and preference. A
    // `burn_pick` naming an ass/ssa track burns it regardless of
    // `preferred` — an explicit pick is not a preference — but never
    // without `capable`.
    ass_burn: AssBurn,
    // HUB-15b: the verified encoder codec names of the box that would
    // run an encode ("h264"/"hevc"/"av1"/"aac"/"opus") — an executor
    // fact like `tonemap`, fetched from the speculatively placed
    // transcoder or the local probes. Encode targets are picked from
    // this set ∩ the client profile, in ubiquity order.
    targets: &[String],
) -> SourcePlan {
    let audio_track = audio_track.min(info.audio.len().saturating_sub(1));
    let video_track = video_track.min(info.video.len().saturating_sub(1));
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
    // A pick is honoured only by the tier that can actually carry it:
    // image tracks need the display-set timeline (`burn_capable`), ASS
    // needs a box with `assrender` (`ass_burn.capable`). The two live
    // in different plan fields and different pipeline branches.
    let picked = |p: &BurnPick| -> Option<&str> {
        match p {
            BurnPick::Embedded(i) => info.subtitles.get(*i).map(|s| s.format.as_str()),
            BurnPick::Sidecar(i) => info.external_subtitles.get(*i).map(|s| s.format.as_str()),
        }
    };
    // Only IMAGE picks force a burn. An ASS pick does not: the ASS
    // tier is chosen by the user's standing `ass_fallback` preference
    // and nothing else (owner decision, 2026-08-03) — picking a track
    // says WHICH subtitles, never HOW they are delivered.
    let forced_burn = burn_pick.filter(|p| match picked(p) {
        // A sidecar image burn only ever meant VobSub: a bare .sub has
        // no index to walk.
        Some("pgs" | "dvdsub") => burn_capable && matches!(p, BurnPick::Embedded(_)),
        Some("vobsub") => burn_capable,
        _ => false,
    });
    let forced_embedded = match forced_burn {
        Some(BurnPick::Embedded(i)) => Some(i),
        _ => None,
    };
    // ...but it does say which ASS track the tier applies to. Without
    // this the burn would always take the first ASS stream, and a
    // release with five languages would burn whichever came first.
    let picked_ass = burn_pick.filter(|p| matches!(picked(p), Some("ass" | "ssa")));
    let direct = single_part
        && container_ok
        && (v.is_none() || v_client_ok)
        && (a.is_none() || a_client_ok)
        && (v.is_some() || a.is_some())
        // Serving the file as-is cannot burn anything into it.
        && !burn_wanted(profile, info, burn_capable, ocr_text)
        && !ass_burn_wanted(profile, info, ass_burn)
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

    // HUB-32a: the same last-resort shape one tier up. A client that
    // cannot render ASS itself gets the flattened VTT by default and
    // the burned picture when the user's preference says so — never
    // silently, and never without a box that can do it. The pick only
    // chooses which track; a sidecar pick burns from the FILE, so it
    // carries no embedded index.
    let ass_wanted = ass_burn_wanted(profile, info, ass_burn);
    let is_ass = |f: &str| matches!(f, "ass" | "ssa");
    let first_embedded_ass = || info.subtitles.iter().position(|s| is_ass(&s.format));
    let first_sidecar_ass = || {
        info.external_subtitles
            .iter()
            .position(|s| is_ass(&s.format))
    };
    // Embedded index (burns from the demuxer pad) and sidecar index
    // (burns from the file) are mutually exclusive; embedded wins when
    // nothing was picked, because it is the one that carries fonts.
    let (burn_ass_track, burn_ass_file) = if !ass_wanted {
        (None, None)
    } else {
        match picked_ass {
            Some(BurnPick::Embedded(i)) => (Some(i), None),
            Some(BurnPick::Sidecar(i)) => (None, Some(i)),
            None => match first_embedded_ass() {
                Some(i) => (Some(i), None),
                None => (None, first_sidecar_ass()),
            },
        }
    };

    // Remux/transcode arms, per CANDIDATE CONTAINER (HUB-15b).
    // An encode is only admissible when the client accepts its TARGET
    // (the capability-mask honesty rule, HUB-14) AND the executor
    // verifiably encodes it AND the container carries it. TS keeps
    // exactly h264/aac (hevc-in-TS has patchy client support); fMP4
    // offers the full ubiquity ladders. The two candidates are judged
    // by Cost and a tie goes to TS — so every session that worked
    // before is byte-identical, and fMP4 appears only where it is
    // strictly cheaper (av1/vp9 copies) or the only playable path
    // (no-h264 clients on hevc/av1-encoding fleets).
    let burn_active = burn_subtitle.is_some()
        || forced_burn.is_some()
        || burn_ass_track.is_some()
        || burn_ass_file.is_some();
    let candidate =
        |format: SegmentFormat| -> (StreamMode, VideoTarget, StreamMode, AudioTarget, (u8, Cost)) {
            let (v_ladder, a_ladder): (&[VideoTarget], &[AudioTarget]) = match format {
                SegmentFormat::Ts => (&[VideoTarget::H264], &[AudioTarget::Aac]),
                SegmentFormat::Fmp4 => (
                    &[VideoTarget::H264, VideoTarget::Hevc, VideoTarget::Av1],
                    &[AudioTarget::Aac, AudioTarget::Opus],
                ),
            };
            let names = crate::remux::muxable_names(format);
            let muxable = |kind: &str, codec: &str| {
                // isofmp4mux's `audio/mpeg` template is mpegversion 4 only,
                // and codec_to_caps_name collides mp3 onto the same name.
                if format == SegmentFormat::Fmp4 && codec == "mp3" {
                    return false;
                }
                codec_to_caps_name(kind, codec).is_some_and(|n| names.contains(n))
            };
            let vt = v_ladder
                .iter()
                .copied()
                .find(|t| {
                    profile.video.iter().any(|c| c.codec == t.as_str())
                        && targets.iter().any(|e| e == t.as_str())
                })
                .unwrap_or_default();
            let client_takes_video_target = v_ladder.iter().any(|t| {
                profile.video.iter().any(|c| c.codec == t.as_str())
                    && targets.iter().any(|e| e == t.as_str())
            });
            let at = a_ladder
                .iter()
                .copied()
                .find(|t| {
                    profile.audio.iter().any(|c| c == t.as_str())
                        && targets.iter().any(|e| e == t.as_str())
                })
                .unwrap_or_default();
            let client_takes_audio_target = a_ladder.iter().any(|t| {
                profile.audio.iter().any(|c| c == t.as_str())
                    && targets.iter().any(|e| e == t.as_str())
            });
            let video = if v
                .is_some_and(|s| v_client_ok && muxable("video", &s.codec) && !burn_active)
            {
                StreamMode::Copy
            } else if client_takes_video_target
                && v.is_some_and(|s| codec_to_caps_name("video", &s.codec).is_some_and(can_decode))
            {
                StreamMode::Encode
            } else {
                StreamMode::Off
            };
            let audio = if a.is_some_and(|s| a_client_ok && muxable("audio", &s.codec)) {
                StreamMode::Copy
            } else if client_takes_audio_target
                && a.is_some_and(|s| codec_to_caps_name("audio", &s.codec).is_some_and(can_decode))
            {
                StreamMode::Encode
            } else {
                StreamMode::Off
            };
            let cost = if direct {
                Cost::Direct
            } else if (video == StreamMode::Off && audio == StreamMode::Off)
                || (v.is_some() && video == StreamMode::Off)
            {
                Cost::Unplayable
            } else if video == StreamMode::Encode {
                Cost::VideoEncode
            } else if audio == StreamMode::Encode {
                Cost::AudioEncode
            } else {
                Cost::Copy
            };
            // Cost cannot see a dropped audio stream (video-off is already
            // Unplayable): an aac-less client's TS candidate copies video
            // and silently drops audio, "cheaper" than fMP4 delivering
            // opus. Delivering beats saving an encode.
            let dropped = (a.is_some() && audio == StreamMode::Off) as u8;
            (video, vt, audio, at, (dropped, cost))
        };
    let ts = candidate(SegmentFormat::Ts);
    let f4 = candidate(SegmentFormat::Fmp4);
    // Fewest dropped streams, then cheapest, tie → TS: the proven path
    // wins unless fMP4 delivers more or strictly cheaper.
    let (segment_format, (video, video_target, audio, audio_target, _key)) = if f4.4 < ts.4 {
        (SegmentFormat::Fmp4, f4)
    } else {
        (SegmentFormat::Ts, ts)
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
        // Same honesty rule as burn_subtitle: only claim the burn when
        // the encode that carries it exists.
        burn_ass: burn_ass_track.filter(|_| video == StreamMode::Encode),
        max_channels: (audio == StreamMode::Encode && profile.max_audio_channels > 0).then(|| {
            a.map(|s| s.channels)
                .filter(|c| *c > 0)
                .map_or(profile.max_audio_channels, |c| {
                    c.min(profile.max_audio_channels)
                })
        }),
        tone_map,
        video_codec: video_target,
        audio_codec: audio_target,
        segment_format,
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
    // Off because no target survived (client ∩ fleet is empty for the
    // richest container): name the actual blocker and what the fleet
    // offers, not a generic "off" (the state the mask creates).
    let offered = |kind: &str| -> String {
        let ladder: &[&str] = if kind == "video" {
            &["h264", "hevc", "av1"]
        } else {
            &["aac", "opus"]
        };
        let have: Vec<&str> = ladder
            .iter()
            .copied()
            .filter(|t| targets.iter().any(|e| e == t))
            .collect();
        if have.is_empty() {
            format!("no {kind} encoder on the fleet")
        } else {
            format!("fleet encodes {}", have.join(", "))
        }
    };
    if video == StreamMode::Off && v.is_some() && !v_client_ok {
        video_verdict = format!(
            "{} → none (client accepts neither the source nor any offered target — {})",
            v.map(|s| s.codec.as_str()).unwrap_or("video"),
            offered("video")
        );
    }
    if audio == StreamMode::Off && a.is_some() && !a_client_ok {
        audio_verdict = format!(
            "{} → none (client accepts neither the source nor any offered target — {})",
            a.map(|s| s.codec.as_str()).unwrap_or("audio"),
            offered("audio")
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
    if !direct && plan.segment_format == SegmentFormat::Fmp4 {
        // The requirement wants the container named with the target.
        if v.is_some() {
            video_verdict.push_str(" · fmp4 segments");
        } else {
            audio_verdict.push_str(" · fmp4 segments");
        }
    }

    let subtitles = info
        .subtitles
        .iter()
        .enumerate()
        .map(|(index, s)| {
            let (tier, note) = match s.format.as_str() {
                "ass" | "ssa" if plan.burn_ass == Some(index) => {
                    (SubtitleTier::Burn, "burned in (forces the video encode)")
                }
                "ass" | "ssa" if profile.ass_render => (SubtitleTier::Text, ""),
                "ass" | "ssa" if burn_ass_track.is_some() => {
                    (SubtitleTier::Unavailable, "only one track can be burned in")
                }
                "ass" | "ssa" if ass_burn.preferred && !ass_burn.capable => (
                    SubtitleTier::Convert,
                    "flattened to VTT — no box in the fleet can burn ASS",
                ),
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
        // A user's own `.ass` burns only when the tier calls for it,
        // same as an embedded one, and only once the encode that would
        // carry it exists.
        burn_ass_sidecar: burn_ass_file.filter(|_| video == StreamMode::Encode),
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
    /// Full-fleet targets: what the dev box actually verifies.
    /// Every pre-HUB-32a case reads the same with the ASS tier off, so
    /// they keep their argument lists and this shadows the real one.
    /// The ASS cases below call `super::negotiate` and pass it.
    #[allow(clippy::too_many_arguments)]
    fn negotiate(
        profile: &CapabilityProfile,
        info: &MediaInfo,
        audio_track: usize,
        video_track: usize,
        single_part: bool,
        est_kbps: Option<u32>,
        tonemap: bool,
        burn_capable: bool,
        ocr_text: &[bool],
        burn_pick: Option<BurnPick>,
        targets: &[String],
    ) -> SourcePlan {
        super::negotiate(
            profile,
            info,
            audio_track,
            video_track,
            single_part,
            est_kbps,
            tonemap,
            burn_capable,
            ocr_text,
            burn_pick,
            AssBurn::default(),
            targets,
        )
    }

    fn fleet() -> Vec<String> {
        ["h264", "hevc", "av1", "aac", "opus"]
            .iter()
            .map(|s| s.to_string())
            .collect()
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

    /// HUB-15b: the encode target follows client ∩ fleet in ubiquity
    /// order, and the container follows the cheapest candidate with a
    /// tie to TS. The un-collapsing this requirement is about: a
    /// no-h264 client on an hevc-encoding fleet gets an hevc encode in
    /// fMP4 instead of a refusal.
    #[test]
    fn encode_targets_follow_client_and_fleet() {
        crate::init().unwrap();
        // An hevc-only client (the {"video":["h264"]} mask off state),
        // hevc source that needs an encode (tone-map forces it).
        let mut p = chrome();
        p.video.retain(|c| c.codec == "hevc");
        let mut info = media("matroska", Some(vs("hevc")), Some(au("aac", 2)));
        info.video[0].hdr = Some("hdr10".into());
        let sp = negotiate(&p, &info, 0, 0, true, None, true, true, &[], None, &fleet());
        assert_eq!(sp.cost, Cost::VideoEncode, "{}", sp.video_verdict);
        assert_eq!(sp.plan.video_codec, VideoTarget::Hevc);
        assert_eq!(sp.plan.segment_format, SegmentFormat::Fmp4);
        assert!(
            sp.video_verdict.contains("hevc (transcoded)")
                && sp.video_verdict.contains("fmp4 segments"),
            "verdict must state codec and container: {}",
            sp.video_verdict
        );

        // Same client, fleet without hevc: honest refusal naming the offer.
        let h264_only: Vec<String> = ["h264", "aac"].iter().map(|s| s.to_string()).collect();
        let sp = negotiate(
            &p,
            &info,
            0,
            0,
            true,
            None,
            true,
            true,
            &[],
            None,
            &h264_only,
        );
        assert_eq!(sp.cost, Cost::Unplayable);
        assert!(
            sp.video_verdict.contains("fleet encodes h264"),
            "refusal names the fleet: {}",
            sp.video_verdict
        );

        // h264-accepting client: ubiquity order keeps h264/TS even
        // though the fleet could do hevc — proven path on ties.
        let sp = negotiate(
            &chrome(),
            &info,
            0,
            0,
            true,
            None,
            true,
            true,
            &[],
            None,
            &fleet(),
        );
        assert_eq!(sp.plan.video_codec, VideoTarget::H264);
        assert_eq!(sp.plan.segment_format, SegmentFormat::Ts);

        // Opus: an aac-less client's audio target.
        let mut p = chrome();
        p.audio.retain(|c| c == "opus");
        let info = media("matroska", Some(vs("h264")), Some(au("dts", 6)));
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
            None,
            &fleet(),
        );
        assert_eq!(sp.cost, Cost::AudioEncode, "{}", sp.audio_verdict);
        assert_eq!(sp.plan.audio_codec, AudioTarget::Opus);
        assert_eq!(sp.plan.segment_format, SegmentFormat::Fmp4);
    }

    /// AV1/VP9 sources with capable clients flip from today's forced
    /// h264 encode to an fMP4 COPY — strictly cheaper, which is what
    /// lets fMP4 win the candidate comparison without a policy knob.
    #[test]
    fn av1_copy_rides_fmp4() {
        crate::init().unwrap();
        let mut p = chrome();
        p.video.push(VideoCap {
            codec: "av1".into(),
            ..Default::default()
        });
        let info = media("matroska", Some(vs("av1")), Some(au("aac", 2)));
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
            None,
            &fleet(),
        );
        assert_eq!(sp.cost, Cost::Copy, "{}", sp.video_verdict);
        assert_eq!(sp.plan.video, StreamMode::Copy);
        assert_eq!(sp.plan.segment_format, SegmentFormat::Fmp4);

        // Without av1 in the profile the old behavior stands: encode
        // to h264 in TS.
        let sp = negotiate(
            &chrome(),
            &info,
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
            &fleet(),
        );
        assert_eq!(sp.cost, Cost::VideoEncode);
        assert_eq!(sp.plan.video_codec, VideoTarget::H264);
        assert_eq!(sp.plan.segment_format, SegmentFormat::Ts);
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
            &fleet(),
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
            &fleet(),
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
            &fleet(),
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
            &fleet(),
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
            &fleet(),
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
            &fleet(),
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
            &fleet(),
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
            &fleet(),
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
            &fleet(),
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
            &fleet(),
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
            &fleet(),
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
            None,
            &fleet(),
        );
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
        let sp = negotiate(
            &able,
            &info,
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
            &fleet(),
        );
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
            &fleet(),
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
            &fleet(),
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
            &fleet(),
        );
        assert_eq!(sp.plan.max_channels, None);
    }

    /// HUB-32a: the ASS fallback ladder — client-native, then the
    /// user's choice between flattening and burning, and never a silent
    /// flatten when the choice was burn.
    #[test]
    fn ass_burns_only_when_wanted_and_possible() {
        let mut p = chrome();
        let mut info = media("matroska", Some(vs("h264")), Some(au("aac", 2)));
        info.subtitles = vec![SubtitleStream {
            format: "ass".into(),
            language: Some("en".into()),
        }];
        let go = |p: &CapabilityProfile, ass: AssBurn, pick: Option<BurnPick>| {
            super::negotiate(
                p,
                &info,
                0,
                0,
                true,
                None,
                false,
                true,
                &[],
                pick,
                ass,
                &fleet(),
            )
        };
        let capable = AssBurn {
            capable: true,
            preferred: false,
        };
        let wants_burn = AssBurn {
            capable: true,
            preferred: true,
        };

        // A client that renders ASS itself always wins: nothing to do,
        // no encode, whatever the preference says.
        p.ass_render = true;
        let sp = go(&p, wants_burn, None);
        assert_eq!(sp.plan.burn_ass, None);
        assert_eq!(sp.subtitles[0].tier, SubtitleTier::Text);
        assert_eq!(sp.cost, Cost::Copy, "{}", sp.video_verdict);

        // ...and a pick does NOT override it. Picking a track says
        // which subtitles, never how they are delivered (owner
        // decision, 2026-08-03): the burn tier is reached by the
        // preference alone, so a client that renders ASS keeps doing
        // so and nothing is forced.
        let sp = go(&p, capable, Some(BurnPick::Embedded(0)));
        assert_eq!(sp.plan.burn_ass, None);
        assert_eq!(sp.subtitles[0].tier, SubtitleTier::Text);
        assert_eq!(sp.cost, Cost::Copy, "a pick must not force an encode");

        // No client-side ASS and no preference: flatten, no video work.
        p.ass_render = false;
        let sp = go(&p, capable, None);
        assert_eq!(sp.plan.burn_ass, None);
        assert_eq!(sp.subtitles[0].tier, SubtitleTier::Convert);
        assert_eq!(sp.cost, Cost::Copy);

        // Preference set and a box that can: burn, and that forces the
        // encode carrying it — a copy cannot have anything burned in.
        let sp = go(&p, wants_burn, None);
        assert_eq!(sp.plan.burn_ass, Some(0));
        assert_eq!(sp.cost, Cost::VideoEncode, "{}", sp.video_verdict);
        assert_eq!(sp.subtitles[0].tier, SubtitleTier::Burn);
        let mut mp4 = info.clone();
        mp4.container = Some("mp4".into());
        let with_mp4 = super::negotiate(
            &p,
            &mp4,
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
            wants_burn,
            &fleet(),
        );
        assert!(!with_mp4.direct, "cannot burn into a file served as-is");

        // Preference set and NO box that can. Flattening happens, but
        // the verdict says why — the hub refuses this case outright
        // before it gets here (the 422), and a silent flatten is the
        // one thing the policy forbids.
        let sp = go(
            &p,
            AssBurn {
                capable: false,
                preferred: true,
            },
            None,
        );
        assert_eq!(sp.plan.burn_ass, None);
        assert_eq!(sp.cost, Cost::Copy);
        assert!(
            sp.subtitles[0].note.contains("no box"),
            "silent flatten: {:?}",
            sp.subtitles[0]
        );

        // A pick that names an ASS track is never mistaken for an image
        // burn: different plan field, different pipeline branch.
        let sp = go(&p, wants_burn, Some(BurnPick::Embedded(0)));
        assert_eq!(sp.plan.burn_subtitle, None);
        assert_eq!(sp.plan.burn_ass, Some(0));

        // With the tier active, the pick chooses WHICH track burns —
        // otherwise a release with five languages would always burn
        // whichever stream happened to come first.
        let mut multi = info.clone();
        multi.subtitles.push(SubtitleStream {
            format: "ass".into(),
            language: Some("de".into()),
        });
        let two = |pick| {
            super::negotiate(
                &p,
                &multi,
                0,
                0,
                true,
                None,
                false,
                true,
                &[],
                pick,
                wants_burn,
                &fleet(),
            )
        };
        assert_eq!(two(None).plan.burn_ass, Some(0), "no pick: the first");
        assert_eq!(two(Some(BurnPick::Embedded(1))).plan.burn_ass, Some(1));
    }

    /// A user's own `.ass` beside the media burns from the FILE, so the
    /// plan carries no index — the hub hands the bytes over, exactly as
    /// it does for a VobSub sidecar's display sets.
    #[test]
    fn a_sidecar_ass_pick_burns_from_the_file() {
        let mut p = chrome();
        p.ass_render = false;
        let mut info = media("matroska", Some(vs("h264")), Some(au("aac", 2)));
        info.external_subtitles = vec![kahawai_core::media::SidecarSubtitle {
            format: "ass".into(),
            language: Some("en".into()),
            path_rel: "film.en.ass".into(),
            track: None,
        }];
        let sp = super::negotiate(
            &p,
            &info,
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            Some(BurnPick::Sidecar(0)),
            // The preference is what selects the tier; the pick only
            // names the file.
            AssBurn {
                capable: true,
                preferred: true,
            },
            &fleet(),
        );
        assert_eq!(sp.burn_ass_sidecar, Some(0));
        assert_eq!(sp.burn_sidecar, None, "not an image burn");
        assert_eq!(sp.plan.burn_ass, None, "no embedded index to burn");
        assert_eq!(sp.cost, Cost::VideoEncode, "{}", sp.video_verdict);
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
            None,
            &fleet(),
        );
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
        let sp = negotiate(&p, &mp4, 0, 0, true, None, false, true, &[], None, &fleet());
        assert!(!sp.direct, "cannot burn into a file served as-is");

        // A compositing client keeps its cheap path and its overlay.
        p.graphics_overlay = true;
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
            None,
            &fleet(),
        );
        assert_eq!(sp.cost, Cost::Copy);
        assert_eq!(sp.plan.burn_subtitle, None);
        assert_eq!(sp.subtitles[0].tier, SubtitleTier::Graphics);

        // HUB-32c: with an OCR text track cached, the non-compositing
        // client is served text — the copy survives, nothing is burned.
        p.graphics_overlay = false;
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
            None,
            &fleet(),
        );
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
        let sp = negotiate(
            &p,
            &mp4,
            0,
            0,
            true,
            None,
            false,
            true,
            &[true],
            None,
            &fleet(),
        );
        assert!(sp.direct, "OCR text restores direct play");

        // The compositing client is unaffected: overlay beats text.
        p.graphics_overlay = true;
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
            None,
            &fleet(),
        );
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
            &fleet(),
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
            &fleet(),
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
            &fleet(),
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
            &fleet(),
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
            None,
            &fleet(),
        );
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
            None,
            &fleet(),
        );
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

        let sp = negotiate(
            &no_hevc,
            &info,
            0,
            0,
            true,
            None,
            true,
            true,
            &[],
            None,
            &fleet(),
        );
        assert_eq!(sp.cost, Cost::VideoEncode);
        assert!(sp.plan.tone_map);
        assert!(
            sp.video_verdict.contains("tone-mapped"),
            "verdict: {}",
            sp.video_verdict
        );

        let sp = negotiate(
            &no_hevc,
            &info,
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
            &fleet(),
        );
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
        let sp = negotiate(
            &no_hevc,
            &info,
            0,
            0,
            true,
            None,
            true,
            true,
            &[],
            None,
            &fleet(),
        );
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

        let sp = negotiate(&p, &info, 0, 0, true, None, true, true, &[], None, &fleet());
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
        let sp = negotiate(&p, &info, 0, 0, true, None, true, true, &[], None, &fleet());
        assert_eq!(sp.cost, Cost::Copy, "hdr-capable client keeps the copy");
        assert!(!sp.plan.tone_map);

        // HLG never vetoes a copy — SDR-compatible by design.
        p.hdr = false;
        let hlg = VideoStream {
            hdr: Some("hlg".into()),
            ..vs("hevc")
        };
        let info = media("matroska", Some(hlg), Some(au("aac", 2)));
        let sp = negotiate(&p, &info, 0, 0, true, None, true, true, &[], None, &fleet());
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
            &fleet(),
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
            &fleet(),
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
            &fleet(),
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
        let sp = negotiate(
            &q,
            &info,
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
            &fleet(),
        );
        assert_eq!(sp.plan.video, StreamMode::Off, "{}", sp.video_verdict);
        assert_eq!(sp.cost, Cost::Unplayable);
        assert!(
            sp.video_verdict.contains("client accepts neither"),
            "verdict must name the blocker: {}",
            sp.video_verdict
        );

        // Audio side: dts source, BOTH encode targets masked out —
        // audio goes Off while the video copy stands. (aac alone
        // masked now honestly delivers opus, HUB-15b — covered in
        // encode_targets_follow_client_and_fleet.)
        let mut r = chrome();
        r.audio.retain(|a| a != "aac" && a != "opus");
        let info = media("matroska", Some(vs("h264")), Some(au("dts", 6)));
        let sp = negotiate(
            &r,
            &info,
            0,
            0,
            true,
            None,
            false,
            true,
            &[],
            None,
            &fleet(),
        );
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
            let new = negotiate(
                &p,
                &info,
                0,
                0,
                true,
                None,
                false,
                true,
                &[],
                None,
                &fleet(),
            );
            assert_eq!(new.plan.video, old.video, "video parity for {info:?}");
            assert_eq!(new.plan.audio, old.audio, "audio parity for {info:?}");
        }
    }
}
