//! Where a session should run, and how fast that is expected to go
//! (HUB-36 phase 5, TC-2): the pure half of placement.
//!
//! Everything here is a function over a [`FleetSnapshot`] the hub's
//! registry takes under its own locks. What is deliberately NOT here is
//! the reservation: the registry increments a box's load in the same
//! critical section that read it, because ten concurrent starts on a
//! five-box fleet once all chose the same `max_sessions = 2` box when the
//! count was read early and taken late. A pure ranker cannot hold a lock,
//! so it ranks and the registry takes.
//!
//! # Work classes
//!
//! `{res}|{src}|{dst}[|tm]` — the identity of a KIND of work, composed by
//! [`work_class`] and by nothing else. It deliberately carries the SOURCE
//! codec, the one dimension a benchmark cannot see (software AV1 *decode*
//! is invisible to an encoder measurement), and the tone-map flag, which
//! on the J5005 was the whole cost. The resolution is bucketed at `> 1080`
//! because the cost step that matters is 4K versus not; 1440p lands in
//! the expensive bucket, since guessing HIGH costs a session a stronger
//! box where guessing low costs a viewer a stall.
//!
//! # Pace
//!
//! What each box has been measured to achieve on a class, as a realtime
//! multiple folded by [`blend`]. A sample is one run on one file — that
//! title's bitrate, that moment's contention, that box's thermal state.
//! Storing the last value would let one bad run condemn a box; a mean
//! would make a hardware change take dozens of sessions to show. At
//! `ALPHA` 0.3 a box converges within about three sessions of a change
//! and no single outlier moves the estimate more than 30%. The hub's
//! `pace` module owns the table that persists these; this crate only
//! computes with them.

use std::collections::HashMap;

use kahawai_media::bench::BenchResults;
use kahawai_proto::{ProtocolFeature, ProtocolFeatures};

/// EWMA weight for a new pace sample. See the module doc.
pub const ALPHA: f64 = 0.3;

/// The reserved module id for work the hub ran itself.
pub const LOCAL: &str = "local";

/// Below this, a box is not keeping ahead of a viewer with any margin.
/// Not 1.0: a box that exactly matches realtime stalls the moment
/// anything else happens on it.
pub const SUSTAINS: f32 = 1.2;

/// `{res}|{src}|{dst}[|tm]` — see the module doc. Framerate is
/// deliberately absent: it would split every class in two for a
/// distinction most libraries never exercise, and the key is a string, so
/// an fps bucket is additive the day a real library shows the skew.
pub fn work_class(height: u32, src_codec: &str, dst_codec: &str, tone_map: bool) -> String {
    let res = if height > 1080 { "2160" } else { "1080" };
    let tm = if tone_map { "|tm" } else { "" };
    format!("{res}|{src_codec}|{dst_codec}{tm}")
}

/// The EWMA step, separated so it can be reasoned about without a
/// database.
pub fn blend(prev: Option<f64>, sample: f64) -> f64 {
    match prev {
        Some(prev) => ALPHA * sample + (1.0 - ALPHA) * prev,
        None => sample,
    }
}

/// Does this prediction clear the bar? An unmeasured box counts as
/// sustaining — refusing work for lack of evidence would leave a fresh
/// fleet unused, and the first session it runs is what produces the
/// evidence.
pub fn sustains(predicted: Option<f32>) -> bool {
    predicted.is_none_or(|p| p >= SUSTAINS)
}

/// Does this ELEMENT produce this codec? The local benchmark is keyed
/// by element (a box that gains a hardware encoder must not inherit the
/// software one's number), so the codec has to be read back off the
/// name. Substrings rather than a table: every family spells the codec
/// into the element (`nvh264enc`, `x265enc`, `vtenc_h265_hw`,
/// `svtav1enc`), and an unknown element simply matches nothing and is
/// left out of the estimate.
pub fn element_encodes(element: &str, codec: &str) -> bool {
    let element = element.to_ascii_lowercase();
    match codec {
        "h264" => element.contains("264"),
        "hevc" => element.contains("265") || element.contains("hevc"),
        "av1" => element.contains("av1"),
        _ => false,
    }
}

/// What a session needs from a transcoder (derived from plan + source).
#[derive(Debug, Clone, Default)]
pub struct PlacementNeed {
    pub encode_video: bool,
    pub encode_audio: bool,
    /// Source caps names per kind (any one must be decodable).
    pub video_caps: Vec<String>,
    pub audio_caps: Vec<String>,
    /// HUB-15a: the plan tone-maps — prefer a box reporting the GL
    /// segment (preference, not filter).
    pub needs_tonemap: bool,
    /// HUB-32a: the plan burns ASS subtitles. A HARD filter, unlike
    /// tone-map, because there is no honest degradation: dropping the
    /// burn would silently hand back a video with no subtitles at all.
    /// `assrender` is genuinely absent on some boxes (macOS here), so
    /// this is a real constraint and not a formality.
    pub needs_ass_burn: bool,
    /// Additive wire feature this plan requires. A HARD filter: choosing a
    /// peer without it would silently drop behavior.
    pub required_protocol_feature: Option<ProtocolFeature>,
    /// HUB-15b: the encode TARGET codec ("h264"/"hevc"/"av1", empty =
    /// any video encoder qualifies). A HARD filter, unlike tone-map: a
    /// box without the target's encoder cannot degrade gracefully.
    pub video_codec: String,
    /// Same for audio ("aac"/"opus", empty = any).
    pub audio_codec: String,
    /// HUB-36: the kind of work this is ([`work_class`]), or None when
    /// there is no encode to predict. Placement looks up what each box
    /// has been MEASURED to achieve on exactly this.
    pub work_class: Option<String>,
    /// Source bitrate, for the link term of the prediction: a box that
    /// cannot pull the bytes fast enough cannot produce fast enough,
    /// however quick its encoder.
    pub source_kbps: Option<u32>,
}

/// Where a session should run, and how fast that is expected to go.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    /// `Some(module_id)` = dispatch to that satellite, `None` = run in
    /// the hub's own supervised worker.
    pub target: Option<String>,
    /// False when video work has neither a suitable satellite nor AIO's
    /// full local executor. `target = None` alone means local (including
    /// ordinary hub audio work), so absence needs separate representation.
    pub available: bool,
    /// Realtime multiple this placement is expected to sustain. None
    /// when nothing about this box and this work has been measured —
    /// which is NOT the same as slow, and is treated as capable.
    pub predicted: Option<f32>,
}

/// One encoder a box reports (TC-1), with its measured speeds. `None`
/// means unmeasured, never slow.
#[derive(Debug, Clone, Default)]
pub struct EncoderSpeed {
    pub codec: String,
    pub element: String,
    pub hardware: bool,
    pub speed_1080: Option<f32>,
    pub speed_2160: Option<f32>,
}

/// What a box reported it can do (TC-1).
#[derive(Debug, Clone, Default)]
pub struct BoxCaps {
    pub encoders: Vec<EncoderSpeed>,
    /// 0 = unlimited.
    pub max_sessions: u32,
    /// Empty inventory = an older satellite that did not report; assumed
    /// capable (OPS-7 tolerance).
    pub decode_caps: Vec<String>,
    pub tonemap: bool,
    pub ass_burn: bool,
    pub tonemap_speed_1080: Option<f32>,
    pub tonemap_speed_2160: Option<f32>,
}

/// One box as the ranker sees it.
#[derive(Debug, Clone)]
pub struct BoxSnapshot {
    pub caps: BoxCaps,
    /// Additive wire features this box's link understands.
    pub protocol: ProtocolFeatures,
    /// Sessions currently placed on it (reservations included).
    pub load: usize,
    /// Admin-drained: skipped entirely.
    pub disabled: bool,
    /// Bytes per second its source plane has sustained, if measured.
    pub link_rate: Option<u64>,
}

/// The fleet at one instant. Only LINKED boxes that have reported
/// capabilities belong here; the registry filters before snapshotting.
#[derive(Debug, Clone, Default)]
pub struct FleetSnapshot {
    pub boxes: HashMap<String, BoxSnapshot>,
    /// Observed pace by `(module_id, work_class)`, [`LOCAL`] included.
    pub pace: HashMap<(String, String), f64>,
    /// AIO's local video executor benchmark, if it has landed.
    pub local_bench: Option<BenchResults>,
    pub local_video_executor_enabled: bool,
}

impl FleetSnapshot {
    pub fn pace_of(&self, module_id: &str, class: &str) -> Option<f64> {
        self.pace
            .get(&(module_id.to_string(), class.to_string()))
            .copied()
    }
}

/// One box that could take the work, with the terms it is ranked on.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub id: String,
    pub sustains: bool,
    pub tonemap_fit: bool,
    pub hardware: bool,
    pub predicted: Option<f32>,
    pub load: usize,
}

/// Placement (§4.5): capability fit (encoders AND source decoders)
/// ≥ capacity ≥ hw-accel ≥ inverse load, ranked best first.
///
/// Sustaining first — a box that keeps ahead of the viewer beats a
/// faster-on-paper one that does not — then tone-map fit, then hardware,
/// then the prediction itself, then least loaded.
pub fn rank(fleet: &FleetSnapshot, need: &PlacementNeed) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = fleet
        .boxes
        .iter()
        .filter(|(_, b)| !b.disabled)
        .filter_map(|(id, b)| {
            let caps = &b.caps;
            let has = |codec: &str| caps.encoders.iter().any(|e| e.codec == codec);
            // HUB-15b: match the TARGET the plan asks for; an empty need
            // means "any encoder of that kind".
            let video_ok = || match need.video_codec.as_str() {
                "" => ["h264", "hevc", "av1"].iter().any(|c| has(c)),
                c => has(c),
            };
            let audio_ok = || match need.audio_codec.as_str() {
                "" => ["aac", "opus"].iter().any(|c| has(c)),
                c => has(c),
            };
            if (need.encode_video && !video_ok()) || (need.encode_audio && !audio_ok()) {
                return None;
            }
            // Decode fit: the box must decode at least one source stream
            // of each kind it will encode.
            let can = |wanted: &[String]| {
                caps.decode_caps.is_empty() || wanted.iter().any(|w| caps.decode_caps.contains(w))
            };
            if (need.encode_video && !can(&need.video_caps))
                || (need.encode_audio && !can(&need.audio_caps))
            {
                return None;
            }
            let max = caps.max_sessions as usize;
            if max > 0 && b.load >= max {
                return None; // at capacity (TC-6)
            }
            if need
                .required_protocol_feature
                .is_some_and(|feature| !b.protocol.supports(feature))
            {
                return None;
            }
            if need.needs_ass_burn && !caps.ass_burn {
                return None; // cannot burn ASS; not a candidate at all
            }
            // Rank hardware on the codec the session will actually run
            // (empty need: any hw video encoder counts).
            let hardware = caps.encoders.iter().any(|e| {
                e.hardware
                    && match need.video_codec.as_str() {
                        "" => true,
                        c => e.codec == c,
                    }
            });
            // HUB-15a: an HDR encode prefers a box that can tone-map — a
            // preference, not a filter: with no capable box the job still
            // runs (worker encodes as-is, verdict said so).
            let tonemap_fit = !need.needs_tonemap || caps.tonemap;
            // HUB-36: what this box is expected to sustain on exactly
            // this work. None = never measured, which ranks as neutral
            // rather than last: a fresh box has to run something before
            // it can be known, and refusing it for want of evidence is
            // how a fleet stays unused.
            let predicted = predict_fleet(fleet, id, need);
            Some(Candidate {
                id: id.clone(),
                sustains: sustains(predicted),
                tonemap_fit,
                hardware,
                predicted,
                load: b.load,
            })
        })
        .collect();
    candidates.sort_by(|a, b| {
        let rank = |p: Option<f32>| p.unwrap_or(SUSTAINS);
        b.sustains
            .cmp(&a.sustains)
            .then(b.tonemap_fit.cmp(&a.tonemap_fit))
            .then(b.hardware.cmp(&a.hardware))
            .then(
                rank(b.predicted)
                    .partial_cmp(&rank(a.predicted))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(a.load.cmp(&b.load))
            .then(a.id.cmp(&b.id))
    });
    candidates
}

/// The box that would take this work, if any. A QUERY: nothing is
/// reserved — see the module doc.
pub fn choose(fleet: &FleetSnapshot, need: &PlacementNeed) -> Option<String> {
    rank(fleet, need).into_iter().next().map(|c| c.id)
}

/// 2160-class work? Read off the class key rather than passed
/// separately, so the prediction and the thing being learned can never
/// disagree about which bucket they are in.
pub fn is_2160(need: &PlacementNeed) -> bool {
    need.work_class
        .as_deref()
        .is_some_and(|c| c.starts_with("2160|"))
}

fn slowest(terms: impl IntoIterator<Item = f32>) -> Option<f32> {
    terms.into_iter().fold(None, |acc: Option<f32>, v| {
        Some(acc.map_or(v, |a| a.min(v)))
    })
}

fn fastest(terms: impl IntoIterator<Item = f32>) -> Option<f32> {
    terms.into_iter().fold(None, |acc: Option<f32>, v| {
        Some(acc.map_or(v, |a| a.max(v)))
    })
}

/// What a satellite is expected to sustain on this work.
///
/// OBSERVED wins outright when present: a measured run already contains
/// the decode, the tone-map, the encode AND that box's link stalls, so
/// folding the component terms in on top would count the same cost
/// twice. Only when nothing has been observed do the parts stand in, and
/// then the SLOWEST of them governs — a chain is its narrowest link.
pub fn predict_fleet(fleet: &FleetSnapshot, id: &str, need: &PlacementNeed) -> Option<f32> {
    if let Some(class) = need.work_class.as_deref()
        && let Some(observed) = fleet.pace_of(id, class)
    {
        return Some(observed as f32);
    }
    let b = fleet.boxes.get(id)?;
    let caps = &b.caps;
    let big = is_2160(need);
    let pos = |v: f32| (v > 0.0).then_some(v); // 0 on the wire = unmeasured

    let mut terms: Vec<f32> = Vec::new();
    let best = fastest(
        caps.encoders
            .iter()
            .filter(|e| match need.video_codec.as_str() {
                "" => true,
                c => e.codec == c,
            })
            .filter_map(|e| if big { e.speed_2160 } else { e.speed_1080 })
            .filter_map(pos),
    );
    terms.extend(best);
    if need.needs_tonemap {
        let tm = if big {
            caps.tonemap_speed_2160
        } else {
            caps.tonemap_speed_1080
        }
        .and_then(pos);
        terms.extend(tm);
    }
    // The link term applies to DISPATCHED work only: the bytes have to
    // cross the wire before they can be encoded.
    if let (Some(kbps), Some(bps)) = (need.source_kbps, b.link_rate)
        && kbps > 0
    {
        terms.push((bps as f32 * 8.0 / 1000.0) / kbps as f32);
    }
    slowest(terms)
}

/// The same question for AIO's full local transcoder. No link term: the
/// bytes are already here, which is precisely why repatriating can beat
/// a faster satellite on a thin wire.
pub fn predict_local(fleet: &FleetSnapshot, need: &PlacementNeed) -> Option<f32> {
    if let Some(class) = need.work_class.as_deref()
        && let Some(observed) = fleet.pace_of(LOCAL, class)
    {
        return Some(observed as f32);
    }
    let bench = fleet.local_bench.as_ref()?;
    let big = is_2160(need);
    let pick = |s: &kahawai_media::bench::Speeds| if big { s.s2160 } else { s.s1080 };
    let mut terms: Vec<f32> = Vec::new();
    let best = fastest(
        bench
            .encoders
            .iter()
            .filter(|(element, _)| bench.encoder_ready(element))
            .filter(|(element, _)| match need.video_codec.as_str() {
                "" => true,
                c => element_encodes(element, c),
            })
            .filter_map(|(_, s)| pick(s)),
    );
    terms.extend(best);
    if need.needs_tonemap && bench.tonemap_ready() {
        terms.extend(bench.tonemap.as_ref().and_then(pick));
    }
    slowest(terms)
}

/// Where this session should run, and how fast that is expected to go.
///
/// Audio-only encode is lightweight hub work (AR-10/HUB-16), so it never
/// consumes a fleet slot. Video encode is full-transcoder work: external
/// fleet first, with AIO's enabled local video executor as the measured
/// fallback/repatriation candidate. The registry reserves the returned
/// target under the same lock it took the snapshot with.
pub fn decide(fleet: &FleetSnapshot, need: &PlacementNeed) -> Placement {
    if !need.encode_video {
        return Placement {
            target: None,
            available: true,
            predicted: None,
        };
    }
    let local_enabled = fleet.local_video_executor_enabled;
    let local = local_enabled.then(|| predict_local(fleet, need)).flatten();
    match choose(fleet, need) {
        None => Placement {
            target: None,
            available: local_enabled,
            predicted: local,
        },
        Some(id) => {
            let fleet_pred = predict_fleet(fleet, &id, need);
            if local_enabled && !sustains(fleet_pred) && sustains(local) && local.is_some() {
                tracing::info!(
                    box_id = %id,
                    class = need.work_class.as_deref().unwrap_or("-"),
                    fleet = fleet_pred.unwrap_or(0.0),
                    local = local.unwrap_or(0.0),
                    "no fleet box sustains this work; keeping it local"
                );
                return Placement {
                    target: None,
                    available: true,
                    predicted: local,
                };
            }
            Placement {
                target: Some(id),
                available: true,
                predicted: fleet_pred,
            }
        }
    }
}
