//! HUB-36 phase 5: the placement policy, stated as cases over a fleet
//! snapshot. Every one of these is a decision that used to be made on
//! codec fit and session count alone, where the honest answer needs
//! throughput.

use kahawai_playback::placement::{
    ALPHA, BoxCaps, BoxSnapshot, EncoderSpeed, FleetSnapshot, LOCAL, PlacementNeed, SUSTAINS,
    blend, decide, rank, work_class,
};
use kahawai_proto::{ProtocolFeature, ProtocolFeatures};

fn caps(hardware: bool, s1080: f32, tonemap: f32) -> BoxCaps {
    BoxCaps {
        encoders: vec![EncoderSpeed {
            codec: "h264".into(),
            element: if hardware { "nvh264enc" } else { "x264enc" }.into(),
            hardware,
            speed_1080: Some(s1080),
            speed_2160: Some(s1080 / 3.0),
        }],
        max_sessions: 2,
        decode_caps: vec!["video/x-h265".into()],
        tonemap: tonemap > 0.0,
        tonemap_speed_1080: (tonemap > 0.0).then_some(tonemap),
        tonemap_speed_2160: (tonemap > 0.0).then_some(tonemap / 3.0),
        ass_burn: false,
    }
}

fn need(class: &str) -> PlacementNeed {
    PlacementNeed {
        encode_video: true,
        encode_audio: false,
        video_caps: vec!["video/x-h265".into()],
        audio_caps: vec![],
        needs_tonemap: false,
        needs_ass_burn: false,
        required_protocol_feature: None,
        video_codec: "h264".into(),
        audio_codec: String::new(),
        work_class: Some(class.into()),
        source_kbps: None,
    }
}

fn fleet(local_video_executor: bool) -> FleetSnapshot {
    FleetSnapshot {
        local_video_executor_enabled: local_video_executor,
        ..FleetSnapshot::default()
    }
}

fn connect(fleet: &mut FleetSnapshot, id: &str, caps: BoxCaps) {
    connect_minor(fleet, id, kahawai_proto::PROTOCOL_MINOR, caps);
}

fn connect_minor(fleet: &mut FleetSnapshot, id: &str, minor: u32, caps: BoxCaps) {
    fleet.boxes.insert(
        id.into(),
        BoxSnapshot {
            caps,
            protocol: ProtocolFeatures::new(minor),
            load: 0,
            disabled: false,
            link_rate: None,
        },
    );
}

fn set_pace(fleet: &mut FleetSnapshot, id: &str, class: &str, multiple: f64) {
    fleet.pace.insert((id.into(), class.into()), multiple);
}

#[test]
fn protocol_four_baseline_does_not_filter_exact_layout_gains() {
    let mut f = fleet(false);
    let class = "1080|hevc|h264";
    connect_minor(&mut f, "baseline-fast", 0, caps(true, 9.0, 0.0));
    connect_minor(&mut f, "future-slow", 9, caps(false, 2.0, 0.0));
    set_pace(&mut f, "baseline-fast", class, 5.0);
    set_pace(&mut f, "future-slow", class, 2.0);

    let mut exact = need(class);
    exact.required_protocol_feature = Some(ProtocolFeature::ExactAudioLoudnessGains);
    assert_eq!(decide(&f, &exact).target.as_deref(), Some("baseline-fast"));
    assert_eq!(
        decide(&f, &need(class)).target.as_deref(),
        Some("baseline-fast")
    );
}

#[test]
fn a_required_feature_a_box_lacks_removes_it_from_the_running() {
    let mut f = fleet(false);
    let class = "1080|hevc|h264";
    connect_minor(&mut f, "old", 4, caps(true, 9.0, 0.0));
    connect_minor(&mut f, "new", 5, caps(false, 2.0, 0.0));
    let mut n = need(class);
    n.required_protocol_feature = Some(ProtocolFeature::ReadinessRunway);
    assert_eq!(decide(&f, &n).target.as_deref(), Some("new"));
}

#[test]
fn exact_loudness_gains_are_available_at_protocol_four_minor_zero() {
    let mut f = fleet(false);
    let class = "1080|hevc|h264";
    connect_minor(&mut f, "baseline", 0, caps(true, 9.0, 0.0));
    set_pace(&mut f, "baseline", class, 9.0);
    let mut gain = need(class);
    gain.required_protocol_feature = Some(ProtocolFeature::ExactAudioLoudnessGains);
    assert_eq!(decide(&f, &gain).target.as_deref(), Some("baseline"));
    assert_eq!(decide(&f, &need(class)).target.as_deref(), Some("baseline"));
}

#[test]
fn a_sustaining_box_beats_a_faster_looking_one_that_is_not() {
    let mut f = fleet(true);
    let class = "1080|hevc|h264";
    // "fast" advertises a quicker encoder, but has been MEASURED crawling
    // on this work — a slow decode its benchmark cannot see.
    connect(&mut f, "fast", caps(true, 9.0, 0.0));
    connect(&mut f, "steady", caps(false, 2.0, 0.0));
    set_pace(&mut f, "fast", class, 0.6);
    set_pace(&mut f, "steady", class, 3.0);
    let p = decide(&f, &need(class));
    assert_eq!(p.target.as_deref(), Some("steady"));
    assert_eq!(p.predicted, Some(3.0));
}

#[test]
fn unmeasured_ranks_as_capable_not_last() {
    let mut f = fleet(true);
    let class = "1080|hevc|h264";
    connect(&mut f, "known-slow", caps(true, 9.0, 0.0));
    connect(&mut f, "fresh", caps(true, 9.0, 0.0));
    // A box that has never run this work has no number at all. It must
    // still beat one measured below the bar, or a fleet that starts out
    // unmeasured can never earn a measurement.
    set_pace(&mut f, "known-slow", class, 0.5);
    let p = decide(&f, &need(class));
    assert_eq!(p.target.as_deref(), Some("fresh"));
    // It has never RUN this work, but it has been benchmarked, so the
    // components stand in for the missing observation.
    assert_eq!(p.predicted, Some(9.0));
}

#[test]
fn a_box_with_nothing_measured_at_all_still_gets_work() {
    let mut f = fleet(true);
    let class = "1080|hevc|h264";
    // A legacy satellite: reports its encoders, but no speeds and has run
    // nothing. There is no honest number to give, and refusing it would
    // strand the only box in the fleet.
    let mut c = caps(true, 0.0, 0.0);
    c.encoders[0].speed_1080 = None;
    c.encoders[0].speed_2160 = None;
    connect(&mut f, "legacy", c);
    let p = decide(&f, &need(class));
    assert_eq!(p.target.as_deref(), Some("legacy"));
    assert_eq!(
        p.predicted, None,
        "must not invent a speed it never measured"
    );
}

#[test]
fn observed_pace_overrides_the_advertised_benchmark() {
    let mut f = fleet(true);
    let class = "1080|hevc|h264";
    connect(&mut f, "box", caps(true, 9.0, 0.0));
    // The benchmark says 9x; the box has actually done 2.5x on this work.
    // Observed wins outright and is NOT blended with the parts, which
    // would count the same cost twice.
    set_pace(&mut f, "box", class, 2.5);
    assert_eq!(decide(&f, &need(class)).predicted, Some(2.5));
}

#[test]
fn the_slowest_component_governs_when_nothing_was_observed() {
    let mut f = fleet(true);
    let class = "2160|hevc|h264|tm";
    // Quick encoder, slow tone-map: the chain is its narrowest link,
    // which on the J5005 was exactly the tone-map.
    connect(&mut f, "box", caps(true, 12.0, 1.5));
    let mut n = need(class);
    n.needs_tonemap = true;
    let p = decide(&f, &n);
    // 2160 speeds are a third of the 1080 figures in this fixture:
    // encoder 4.0, tone-map 0.5 — the tone-map governs.
    assert_eq!(p.target.as_deref(), Some("box"));
    let got = p.predicted.unwrap();
    assert!(
        (got - 0.5).abs() < 1e-5,
        "expected the tone-map term, got {got}"
    );
}

#[test]
fn work_repatriates_only_when_no_fleet_box_sustains_and_the_hub_does() {
    let mut f = fleet(true);
    let class = "1080|hevc|h264";
    connect(&mut f, "crawler", caps(false, 0.4, 0.0));
    set_pace(&mut f, "crawler", class, 0.4);
    // The hub has run this work at 5x.
    set_pace(&mut f, LOCAL, class, 5.0);
    let p = decide(&f, &need(class));
    assert_eq!(p.target, None, "should have come home");
    assert_eq!(p.predicted, Some(5.0));

    // ...but a sustaining satellite keeps it, even though the hub is
    // faster: hub cores serve clients (§4.5).
    connect(&mut f, "capable", caps(true, 6.0, 0.0));
    set_pace(&mut f, "capable", class, 2.0);
    assert_eq!(decide(&f, &need(class)).target.as_deref(), Some("capable"));
}

#[test]
fn disabling_the_local_video_executor_requires_and_keeps_video_on_the_fleet() {
    let mut f = fleet(false);
    let class = "1080|hevc|h264";
    let unavailable = decide(&f, &need(class));
    assert!(!unavailable.available);
    assert_eq!(unavailable.target, None);

    connect(&mut f, "external", caps(true, 0.4, 0.0));
    set_pace(&mut f, "external", class, 0.4);
    // Even evidence that the hub would be faster must not override the
    // structural choice to keep encoding off this machine.
    set_pace(&mut f, LOCAL, class, 5.0);
    let placed = decide(&f, &need(class));
    assert!(placed.available);
    assert_eq!(placed.target.as_deref(), Some("external"));
}

#[test]
fn audio_only_encode_is_always_lightweight_local_hub_work() {
    let mut f = fleet(false);
    let audio = PlacementNeed {
        encode_video: false,
        encode_audio: true,
        audio_caps: vec!["audio/x-ac3".into()],
        audio_codec: "aac".into(),
        ..PlacementNeed::default()
    };
    let p = decide(&f, &audio);
    assert!(p.available);
    assert_eq!(p.target, None);
    assert_eq!(p.predicted, None);
    // Even a connected full transcoder does not consume lightweight work.
    connect(&mut f, "external", caps(true, 9.0, 0.0));
    let p = decide(&f, &audio);
    assert!(p.available);
    assert_eq!(p.target, None);
}

#[test]
fn a_thin_link_caps_a_fast_encoder() {
    let mut f = fleet(true);
    let class = "1080|hevc|h264";
    connect(&mut f, "box", caps(true, 8.0, 0.0));
    // 1 MB/s against a 20 Mbit source: the bytes cannot arrive fast
    // enough for the encoder to matter.
    f.boxes.get_mut("box").unwrap().link_rate = Some(1_000_000);
    let mut n = need(class);
    n.source_kbps = Some(20_000);
    let got = decide(&f, &n).predicted.unwrap();
    assert!(
        (got - 0.4).abs() < 1e-5,
        "link term should govern, got {got}"
    );
    assert!(got < SUSTAINS);
}

/// HUB-32a: an ASS burn is a HARD placement filter, not a preference.
/// Unlike tone-map — where a box that cannot do it still runs the job
/// and the verdict says so — dropping a burn would hand back a picture
/// with no subtitles in it and nothing downstream could tell.
#[test]
fn an_ass_burn_only_lands_on_a_box_that_can_burn() {
    let mut f = fleet(true);
    let class = "1080|hevc|h264";
    let mut burner = caps(false, 2.0, 0.0);
    burner.ass_burn = true;
    // The faster box cannot burn ASS. Without the filter it wins on every
    // other axis and the subtitles vanish.
    connect(&mut f, "fast-no-ass", caps(true, 9.0, 0.0));
    connect(&mut f, "slow-with-ass", burner);

    let mut n = need(class);
    assert_eq!(
        decide(&f, &n).target.as_deref(),
        Some("fast-no-ass"),
        "without the need, speed wins"
    );
    n.needs_ass_burn = true;
    assert_eq!(decide(&f, &n).target.as_deref(), Some("slow-with-ass"));

    // With no capable box the placement fails outright rather than
    // degrading — which is what makes the session's 422 reachable instead
    // of a silent no-subtitle encode on the local worker.
    let mut empty = fleet(true);
    connect(&mut empty, "fast-no-ass", caps(true, 9.0, 0.0));
    assert_eq!(decide(&empty, &n).target, None);
}

#[test]
fn a_drained_or_full_box_is_not_a_candidate() {
    let mut f = fleet(false);
    let class = "1080|hevc|h264";
    connect(&mut f, "drained", caps(true, 9.0, 0.0));
    connect(&mut f, "full", caps(true, 9.0, 0.0));
    connect(&mut f, "free", caps(false, 2.0, 0.0));
    f.boxes.get_mut("drained").unwrap().disabled = true;
    f.boxes.get_mut("full").unwrap().load = 2;
    let ranked = rank(&f, &need(class));
    assert_eq!(ranked.len(), 1);
    assert_eq!(ranked[0].id, "free");
}

#[test]
fn ranking_is_stable_between_equal_boxes() {
    let mut f = fleet(false);
    let class = "1080|hevc|h264";
    connect(&mut f, "b", caps(true, 9.0, 0.0));
    connect(&mut f, "a", caps(true, 9.0, 0.0));
    let ids: Vec<_> = rank(&f, &need(class)).into_iter().map(|c| c.id).collect();
    assert_eq!(
        ids,
        ["a", "b"],
        "ties fall back to the id, never to map order"
    );
}

#[test]
fn class_keys_carry_source_codec_and_tonemap() {
    assert_eq!(work_class(2160, "hevc", "h264", true), "2160|hevc|h264|tm");
    assert_eq!(work_class(1080, "h264", "h264", false), "1080|h264|h264");
    // Bucketed at >1080, and anything above lands in the expensive class:
    // over-estimating costs a stronger box, under-estimating costs a
    // viewer a stall.
    assert_eq!(work_class(1080, "av1", "h264", false), "1080|av1|h264");
    assert_eq!(work_class(1081, "av1", "h264", false), "2160|av1|h264");
    assert_eq!(work_class(1440, "av1", "h264", false), "2160|av1|h264");
}

#[test]
fn ewma_converges_in_about_three_samples_and_no_outlier_dominates() {
    // First sample is the estimate: nothing to blend against.
    assert_eq!(blend(None, 4.0), 4.0);
    // A single outlier moves it by at most ALPHA.
    let after = blend(Some(4.0), 0.5);
    assert!(
        (after - 4.0).abs() <= 4.0 * ALPHA + f64::EPSILON,
        "one sample moved the estimate {after}"
    );
    // A hardware change is believed within ~3 samples.
    let mut v = 4.0;
    for _ in 0..3 {
        v = blend(Some(v), 0.5);
    }
    assert!(v < 1.9, "still {v} after three slow runs");
}
