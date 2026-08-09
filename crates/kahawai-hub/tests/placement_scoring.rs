//! HUB-36 phase 5: the placement policy, stated as cases.
//!
//! Every one of these is a decision that used to be made on codec fit
//! and session count alone, where the honest answer needs throughput.

use kahawai_hub::pace;
use kahawai_hub::registry::{PlacementNeed, Registry, SUSTAINS};
use kahawai_proto::v1::{CapabilityReport, EncoderCap};

fn caps(hardware: bool, s1080: f32, tonemap: f32) -> CapabilityReport {
    CapabilityReport {
        encoders: vec![EncoderCap {
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
        video_codec: "h264".into(),
        audio_codec: String::new(),
        work_class: Some(class.into()),
        source_kbps: None,
    }
}

async fn registry() -> (tempfile::TempDir, std::sync::Arc<Registry>) {
    registry_with_local_executor(true).await
}

async fn registry_with_local_executor(
    enabled: bool,
) -> (tempfile::TempDir, std::sync::Arc<Registry>) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let allowed = kahawai_transport::mtls::AllowedCerts::default();
    (
        dir,
        std::sync::Arc::new(Registry::new(db, allowed).with_local_executor(enabled)),
    )
}

/// Connect a transcoder well enough to be a placement candidate.
fn connect(reg: &Registry, id: &str, c: CapabilityReport) {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    std::mem::forget(rx); // keep the link "up" for the test's lifetime
    reg.connected(id, "transcoder", id, "fp", "test");
    reg.register_tc_link(id, tx);
    reg.set_transcoder_caps(id, &c);
}

#[tokio::test]
async fn a_sustaining_box_beats_a_faster_looking_one_that_is_not() {
    let (_d, reg) = registry().await;
    let class = "1080|hevc|h264";
    // "fast" advertises a quicker encoder, but has been MEASURED
    // crawling on this work — a slow decode its benchmark cannot see.
    connect(&reg, "fast", caps(true, 9.0, 0.0));
    connect(&reg, "steady", caps(false, 2.0, 0.0));
    reg.set_pace("fast", class, 0.6);
    reg.set_pace("steady", class, 3.0);

    let p = reg.place(&need(class));
    assert_eq!(p.target.as_deref(), Some("steady"));
    assert_eq!(p.predicted, Some(3.0));
}

#[tokio::test]
async fn unmeasured_ranks_as_capable_not_last() {
    let (_d, reg) = registry().await;
    let class = "1080|hevc|h264";
    connect(&reg, "known-slow", caps(true, 9.0, 0.0));
    connect(&reg, "fresh", caps(true, 9.0, 0.0));
    // A box that has never run this work has no number at all. It must
    // still beat one measured below the bar, or a fleet that starts out
    // unmeasured can never earn a measurement.
    reg.set_pace("known-slow", class, 0.5);

    let p = reg.place(&need(class));
    assert_eq!(p.target.as_deref(), Some("fresh"));
    // It has never RUN this work, but it has been benchmarked, so the
    // components stand in for the missing observation.
    assert_eq!(p.predicted, Some(9.0));
}

#[tokio::test]
async fn a_box_with_nothing_measured_at_all_still_gets_work() {
    let (_d, reg) = registry().await;
    let class = "1080|hevc|h264";
    // A legacy satellite: reports its encoders, but no speeds (0 on the
    // wire = unmeasured) and has run nothing. There is no honest number
    // to give, and refusing it would strand the only box in the fleet.
    let mut c = caps(true, 0.0, 0.0);
    c.encoders[0].speed_1080 = None;
    c.encoders[0].speed_2160 = None;
    connect(&reg, "legacy", c);

    let p = reg.place(&need(class));
    assert_eq!(p.target.as_deref(), Some("legacy"));
    assert_eq!(
        p.predicted, None,
        "must not invent a speed it never measured"
    );
}

#[tokio::test]
async fn observed_pace_overrides_the_advertised_benchmark() {
    let (_d, reg) = registry().await;
    let class = "1080|hevc|h264";
    connect(&reg, "box", caps(true, 9.0, 0.0));
    // The benchmark says 9x; the box has actually done 2.5x on this
    // work. Observed wins outright and is NOT blended with the parts,
    // which would count the same cost twice.
    reg.set_pace("box", class, 2.5);
    assert_eq!(reg.place(&need(class)).predicted, Some(2.5));
}

#[tokio::test]
async fn the_slowest_component_governs_when_nothing_was_observed() {
    let (_d, reg) = registry().await;
    let class = "2160|hevc|h264|tm";
    // Quick encoder, slow tone-map: the chain is its narrowest link,
    // which on the J5005 was exactly the tone-map.
    connect(&reg, "box", caps(true, 12.0, 1.5));
    let mut n = need(class);
    n.needs_tonemap = true;
    let p = reg.place(&n);
    // 2160 speeds are a third of the 1080 figures in this fixture:
    // encoder 4.0, tone-map 0.5 — the tone-map governs.
    assert_eq!(p.target.as_deref(), Some("box"));
    let got = p.predicted.unwrap();
    assert!(
        (got - 0.5).abs() < 1e-5,
        "expected the tone-map term, got {got}"
    );
}

#[tokio::test]
async fn work_repatriates_only_when_no_fleet_box_sustains_and_the_hub_does() {
    let (_d, reg) = registry().await;
    let class = "1080|hevc|h264";
    connect(&reg, "crawler", caps(false, 0.4, 0.0));
    reg.set_pace("crawler", class, 0.4);
    // The hub has run this work at 5x.
    reg.set_pace(pace::LOCAL, class, 5.0);

    let p = reg.place(&need(class));
    assert_eq!(p.target, None, "should have come home");
    assert_eq!(p.predicted, Some(5.0));

    // ...but a sustaining satellite keeps it, even though the hub is
    // faster: hub cores serve clients (§4.5).
    connect(&reg, "capable", caps(true, 6.0, 0.0));
    reg.set_pace("capable", class, 2.0);
    let p = reg.place(&need(class));
    assert_eq!(p.target.as_deref(), Some("capable"));
}

#[tokio::test]
async fn disabling_the_local_executor_requires_and_keeps_work_on_the_fleet() {
    let (_d, reg) = registry_with_local_executor(false).await;
    let class = "1080|hevc|h264";

    let unavailable = reg.place(&need(class));
    assert!(!unavailable.available);
    assert_eq!(unavailable.target, None);

    connect(&reg, "external", caps(true, 0.4, 0.0));
    reg.set_pace("external", class, 0.4);
    // Even evidence that the hub would be faster must not override the
    // structural choice to keep encoding off this machine.
    reg.set_pace(pace::LOCAL, class, 5.0);
    let placed = reg.place(&need(class));
    assert!(placed.available);
    assert_eq!(placed.target.as_deref(), Some("external"));
}

#[tokio::test]
async fn a_thin_link_caps_a_fast_encoder() {
    let (_d, reg) = registry().await;
    let class = "1080|hevc|h264";
    connect(&reg, "box", caps(true, 8.0, 0.0));
    // 1 MB/s against a 20 Mbit source: the bytes cannot arrive fast
    // enough for the encoder to matter.
    reg.set_link_rate("box", 1_000_000);
    let mut n = need(class);
    n.source_kbps = Some(20_000);
    let got = reg.place(&n).predicted.unwrap();
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
/// `assrender` is genuinely absent on the mac mini, so this is a real
/// constraint rather than a formality.
#[tokio::test]
async fn an_ass_burn_only_lands_on_a_box_that_can_burn() {
    let (_d, reg) = registry().await;
    let class = "1080|hevc|h264";
    let mut burner = caps(false, 2.0, 0.0);
    burner.ass_burn = true;
    // The faster box cannot burn ASS. Without the filter it wins on
    // every other axis and the subtitles vanish.
    connect(&reg, "fast-no-ass", caps(true, 9.0, 0.0));
    connect(&reg, "slow-with-ass", burner);

    let mut n = need(class);
    assert_eq!(
        reg.place(&n).target.as_deref(),
        Some("fast-no-ass"),
        "without the need, speed wins"
    );

    n.needs_ass_burn = true;
    assert_eq!(reg.place(&n).target.as_deref(), Some("slow-with-ass"));
    assert!(reg.any_transcoder_ass_burn());

    // With no capable box the placement fails outright rather than
    // degrading — which is what makes the session's 422 reachable
    // instead of a silent no-subtitle encode on the local worker.
    let (_d2, empty) = registry().await;
    connect(&empty, "fast-no-ass", caps(true, 9.0, 0.0));
    assert_eq!(empty.place(&n).target, None);
    assert!(!empty.any_transcoder_ass_burn());
}
