//! HUB-36 phase 5: the registry's half of placement — the snapshot it
//! takes under its locks reaches `kahawai_playback::placement` intact, and
//! the reservation happens in the same critical section.
//!
//! The policy itself is stated as cases in
//! `crates/kahawai-playback/tests/placement.rs`; what is pinned here is
//! the wiring: capability reports, live links, observed pace, link rate,
//! the local executor switch and the sender a required feature pairs with.

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
        required_protocol_feature: None,
        video_codec: "h264".into(),
        audio_codec: String::new(),
        work_class: Some(class.into()),
        source_kbps: None,
    }
}

async fn registry() -> (tempfile::TempDir, std::sync::Arc<Registry>) {
    registry_with_local_video_executor(true).await
}

async fn registry_with_local_video_executor(
    enabled: bool,
) -> (tempfile::TempDir, std::sync::Arc<Registry>) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let allowed = kahawai_transport::mtls::AllowedCerts::default();
    (
        dir,
        std::sync::Arc::new(
            Registry::new(
                db,
                allowed,
                kahawai_mediadb::Store::in_memory().await.unwrap(),
            )
            .with_local_video_executor(enabled),
        ),
    )
}

/// Connect a transcoder well enough to be a placement candidate.
fn connect(reg: &Registry, id: &str, c: CapabilityReport) {
    connect_minor(reg, id, kahawai_proto::PROTOCOL_MINOR, c);
}

fn connect_minor(reg: &Registry, id: &str, protocol_minor: u32, c: CapabilityReport) {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    std::mem::forget(rx); // keep the link "up" for the test's lifetime
    reg.connected(id, "transcoder", id, "fp", "test");
    reg.register_tc_link(id, protocol_minor, tx);
    reg.set_transcoder_caps(id, &c);
}

#[tokio::test]
async fn dispatch_pairs_the_required_protocol_with_the_same_live_sender() {
    let (_d, reg) = registry_with_local_video_executor(false).await;
    let (old_tx, mut old_rx) = tokio::sync::mpsc::channel(2);
    reg.register_tc_link("box", 0, old_tx);
    let required = Some(kahawai_proto::ProtocolFeature::ExactAudioLoudnessGains);
    reg.send_to_tc_requiring("box", kahawai_proto::v1::HubToTc::default(), required)
        .await
        .unwrap();
    assert!(old_rx.recv().await.unwrap().is_ok());

    let (new_tx, mut new_rx) = tokio::sync::mpsc::channel(2);
    reg.register_tc_link("box", 0, new_tx);
    reg.send_to_tc_requiring("box", kahawai_proto::v1::HubToTc::default(), required)
        .await
        .unwrap();
    assert!(new_rx.recv().await.unwrap().is_ok());
    assert!(old_rx.try_recv().is_err());
}

#[tokio::test]
async fn the_registry_snapshot_reaches_the_ranker() {
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
