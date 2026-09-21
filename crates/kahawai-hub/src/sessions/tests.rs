#[test]
fn diagnostics_survive_pipeline_purge_and_hub_restart() {
    let data = tempfile::tempdir().unwrap();
    let scratch = data.path().join("sessions");
    let sessions = super::Sessions::new(scratch.clone());
    sessions.note_session("session", "stable-item");
    // Per-run directories: r1 failed and was bundled on the way out, r2 was
    // still running when the hub went down.
    let first = scratch.join("session").join("r1");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::write(first.join("worker.log"), "first attempt failed").unwrap();
    let (item, header) = sessions.log_header("session");
    crate::sessionlog::store(
        data.path(),
        &item,
        "session",
        &format!(
            "{header}{}",
            kahawai_playback::bundle::gather("hub-local worker", "session", &first)
        ),
    );
    std::fs::remove_dir_all(&first).unwrap();
    let second = scratch.join("session").join("r2");
    std::fs::create_dir_all(&second).unwrap();
    std::fs::write(second.join("worker.log"), "second attempt interrupted").unwrap();
    drop(sessions);
    let _restarted = super::Sessions::new(scratch.clone());
    assert!(!scratch.exists());
    let path = crate::sessionlog::newest_for_item(data.path(), "stable-item").unwrap();
    let body = std::fs::read_to_string(path).unwrap();
    assert!(body.contains("first attempt failed"));
    assert!(body.contains("second attempt interrupted"));
    assert!(body.contains("recovered after hub restart"));
}
use super::{
    LoudnessPreference, Negotiation, PartSource, Sessions, fold_facts, local_audio_encoder_names,
    local_tonemap_available, local_video_encoder_names, replanned_verdict, same_video_path,
    source_choice_key,
};

/// The per-user cap has to hold when starts ARRIVE TOGETHER, which
/// is the only time it matters. It used to count `active`, drop the
/// lock, and let the session land there some five hundred lines and
/// sixteen awaits later — so concurrent callers all read the same
/// stale count. Measured against the live hub before this fix: 20
/// concurrent starts admitted against a cap of 4.
///
/// Admission is tested directly rather than through `start`, which
/// would need a registry, a mediahost and a real file to reach the
/// same decision.
#[test]
fn the_per_user_cap_holds_when_starts_arrive_together() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = std::sync::Arc::new(Sessions::with_limits(
        dir.path().join("sessions"),
        4,
        std::time::Duration::from_secs(90),
    ));

    // Which ids won is a race; the test must not assume.
    let admitted: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(20));
    let mut threads = Vec::new();
    for i in 0..20 {
        let (sessions, admitted, barrier) = (sessions.clone(), admitted.clone(), barrier.clone());
        threads.push(std::thread::spawn(move || {
            let id = format!("s{i}");
            barrier.wait(); // all twenty push on the door at once
            if sessions.admit(&id, "u1").is_ok() {
                admitted.lock().unwrap().push(id);
            }
        }));
    }
    for t in threads {
        t.join().unwrap();
    }
    let admitted = admitted.lock().unwrap().clone();
    assert_eq!(admitted.len(), 4, "the cap admitted more than it allows");

    // Another user is unaffected: the cap is per user, not global.
    assert!(sessions.admit("other", "u2").is_ok());

    // Releasing frees exactly one slot, and no more.
    sessions.release(&admitted[0]);
    assert!(sessions.admit("again", "u1").is_ok());
    assert!(sessions.admit("once-more", "u1").is_err());
}

#[tokio::test]
async fn local_serving_capabilities_require_successful_benchmarks() {
    let available = kahawai_media::remux::encoder_capabilities();
    let Some((codec, element, _)) = available.first().copied() else {
        eprintln!("skip: no verified encoder on this test host");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let db = crate::db::open(dir.path()).await.unwrap();
    let registry = crate::registry::Registry::new(
        db,
        Default::default(),
        kahawai_mediadb::Store::in_memory().await.unwrap(),
    );
    assert!(
        local_video_encoder_names(&registry).is_empty(),
        "unmeasured local encoder was offered"
    );

    let cache = dir.path().join("benchmarks.json");
    let mut measured = kahawai_media::bench::BenchResults {
        gst: kahawai_media::bench::gst_version(),
        tonemap: Some(kahawai_media::bench::Speeds {
            s1080: Some(2.0),
            s2160: Some(0.5),
        }),
        ..Default::default()
    };
    measured.encoders.insert(
        element.into(),
        kahawai_media::bench::Speeds {
            s1080: Some(3.0),
            s2160: Some(0.8),
        },
    );
    kahawai_media::bench::store(&cache, &measured);
    registry.set_local_bench(measured);
    assert_eq!(local_video_encoder_names(&registry), [codec]);
    assert_eq!(
        local_tonemap_available(&registry),
        kahawai_media::remux::tonemap_available()
    );

    kahawai_media::bench::record_crash(
        &cache,
        &kahawai_media::bench::BenchmarkJob::Encoder(element.into()),
    );
    registry.set_local_bench(kahawai_media::bench::load(&cache).unwrap());
    assert!(
        !local_video_encoder_names(&registry)
            .iter()
            .any(|candidate| candidate == codec),
        "quarantined local encoder remained a serving capability"
    );

    kahawai_media::bench::record_crash(&cache, &kahawai_media::bench::BenchmarkJob::ToneMap);
    registry.set_local_bench(kahawai_media::bench::load(&cache).unwrap());
    assert!(!local_tonemap_available(&registry));
}

/// A video encode moves the whole pipeline onto the full executor, so its
/// target set must include that executor's independently discovered audio
/// encoders. The local video list is benchmark-derived and intentionally
/// contains no audio codecs; deriving both target sets from it made an
/// all-in-one tone-map silently drop E-AC-3 instead of encoding AAC.
#[tokio::test]
async fn the_full_local_executor_keeps_its_audio_targets() {
    let expected = local_audio_encoder_names();
    if !expected.iter().any(|codec| codec == "aac") {
        eprintln!("skip: no local AAC encoder on this test host");
        return;
    }
    let Some((_, h264_element, _)) = kahawai_media::remux::encoder_capabilities()
        .iter()
        .find(|(codec, _, _)| *codec == "h264")
        .copied()
    else {
        eprintln!("skip: no local H.264 encoder on this test host");
        return;
    };
    let db = crate::db::open_in_memory().await.unwrap();
    let registry = crate::registry::Registry::new(
        db,
        Default::default(),
        kahawai_mediadb::Store::in_memory().await.unwrap(),
    )
    .with_local_video_executor(true);
    let mut bench = kahawai_media::bench::BenchResults {
        gst: kahawai_media::bench::gst_version(),
        ..Default::default()
    };
    bench.encoders.insert(
        h264_element.into(),
        kahawai_media::bench::Speeds {
            s1080: Some(1.0),
            s2160: Some(1.0),
        },
    );
    registry.set_local_bench(bench);
    let sessions = Sessions::new(tempfile::tempdir().unwrap().path().join("sessions"));
    let negotiation = Negotiation {
        registry: &registry,
        sessions: &sessions,
        profile: Default::default(),
        loudness: LoudnessPreference::Off,
        ass: Default::default(),
        ocr_set: Default::default(),
        raster_sources: None,
        burn_row: None,
        audio_track: 0,
        source_audio_tracks: Default::default(),
        video_track: 0,
        force_audio_encode: false,
        force_measurement: None,
    };

    let facts = negotiation.probe(&Default::default(), false, None);
    assert_eq!(facts.full_audio_targets, expected);
    let mut profile = kahawai_core::media::CapabilityProfile::default();
    profile.containers.clear();
    profile.video = vec![kahawai_core::media::VideoCap {
        codec: "h264".into(),
        ..Default::default()
    }];
    profile.audio = vec!["aac".into()];
    profile.max_height = Some(1920);
    let info = kahawai_core::media::MediaInfo {
        container: Some("matroska".into()),
        video: vec![kahawai_core::media::VideoStream {
            codec: "h264".into(),
            width: 3840,
            height: 2076,
            ..Default::default()
        }],
        audio: vec![kahawai_core::media::AudioStream {
            codec: "mp3".into(),
            channels: 6,
            sample_rate: 48_000,
            ..Default::default()
        }],
        ..Default::default()
    };
    let plan = kahawai_media::negotiate::negotiate_for_executors(
        &profile,
        &info,
        0,
        0,
        true,
        None,
        facts.tonemap,
        false,
        &[],
        None,
        &Default::default(),
        &facts.video_targets,
        &facts.full_audio_targets,
        &facts.local_audio_targets,
        false,
    );
    assert_eq!(plan.plan.video, kahawai_media::remux::StreamMode::Encode);
    assert_eq!(
        plan.plan.audio,
        kahawai_media::remux::StreamMode::Encode,
        "{}",
        plan.audio_verdict
    );
}

/// Facts amend the verdict by kind, exactly once — a seek-restart
/// re-learns the same fold and must not stutter it — and unknown
/// kinds change nothing.
#[test]
fn facts_fold_into_the_verdict_idempotently() {
    let fact = |kind: &str, detail: &str| kahawai_media::facts::Fact {
        kind: kind.into(),
        detail: detail.into(),
    };
    let mut verdict = Some((
        "hevc → h264 (transcoded)".to_string(),
        "dts → aac (transcoded)".to_string(),
    ));
    let facts = vec![
        fact("audio", "7.1 → 5.1"),
        fact("video", "tone-map"),
        fact("weird", "x"),
    ];
    fold_facts(&mut verdict, &facts);
    fold_facts(&mut verdict, &facts); // the seek-restart
    let (video, audio) = verdict.unwrap();
    assert_eq!(audio, "dts → aac (transcoded) · 7.1 → 5.1");
    assert_eq!(video, "hevc → h264 (transcoded) · tone-map");

    // No verdict (direct play) — nothing to amend, no panic.
    let mut none = None;
    fold_facts(&mut none, &facts);
    assert_eq!(none, None);
}

#[test]
fn optional_audio_gain_never_changes_the_video_path() {
    let base = kahawai_media::remux::RemuxPlan {
        video: kahawai_media::remux::StreamMode::Encode,
        tone_map: true,
        ..Default::default()
    };
    let mut gain_only = base;
    gain_only.stereo_gain_db = Some(3.0);
    assert!(same_video_path(&base, &gain_only));

    let mut different_codec = base;
    different_codec.video_codec = kahawai_media::remux::VideoTarget::Hevc;
    different_codec.segment_format = kahawai_media::remux::SegmentFormat::Fmp4;
    assert!(!same_video_path(&base, &different_codec));

    let mut missing_tonemap = base;
    missing_tonemap.tone_map = false;
    assert!(!same_video_path(&base, &missing_tonemap));
}

#[test]
fn a_track_replan_refuses_empty_output_and_refreshes_its_verdict() {
    let empty = kahawai_media::remux::RemuxPlan::default();
    assert!(replanned_verdict(&empty, "new video verdict", "new audio verdict").is_err());

    let mut playable = empty;
    playable.audio = kahawai_media::remux::StreamMode::Copy;
    assert_eq!(
        replanned_verdict(&playable, "new video verdict", "new audio verdict").unwrap(),
        ("new video verdict".into(), "new audio verdict".into())
    );
}

#[tokio::test]
async fn forced_video_encode_uses_protocol_four_baseline_layout_gains() {
    use kahawai_core::media::{AudioStream, MediaInfo, VideoStream};
    use kahawai_proto::v1::{CapabilityReport, EncoderCap};

    let dir = tempfile::tempdir().unwrap();
    let db = crate::db::open(dir.path()).await.unwrap();
    let registry = crate::registry::Registry::new(
        db,
        Default::default(),
        kahawai_mediadb::Store::in_memory().await.unwrap(),
    )
    .with_local_video_executor(false);
    let connect = |id: &str, minor: u32, hardware: bool| {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        std::mem::forget(rx);
        registry.connected(id, "transcoder", id, "fp", "test");
        registry.register_tc_link(id, minor, tx.clone());
        registry.set_transcoder_caps(
            id,
            &CapabilityReport {
                encoders: vec![
                    EncoderCap {
                        codec: "h264".into(),
                        element: if hardware { "nvh264enc" } else { "x264enc" }.into(),
                        hardware,
                        speed_1080: Some(if hardware { 9.0 } else { 2.0 }),
                        speed_2160: Some(if hardware { 3.0 } else { 0.7 }),
                    },
                    EncoderCap {
                        codec: "aac".into(),
                        element: "avenc_aac".into(),
                        hardware: false,
                        speed_1080: None,
                        speed_2160: None,
                    },
                ],
                max_sessions: 2,
                decode_caps: vec!["video/x-h265".into(), "audio/mpeg".into()],
                ..Default::default()
            },
        );
        tx
    };
    let baseline_fast = connect("baseline-fast", 0, true);
    let _baseline_slow = connect("baseline-slow", 0, false);

    let sessions = Sessions::new(dir.path().join("sessions"));
    let negotiation =
        Negotiation::preferences(&sessions, &registry, "user", Some(Default::default()), 0, 0)
            .await
            .unwrap();
    let info = MediaInfo {
        container: Some("matroska".into()),
        duration_ms: Some(60_000),
        video: vec![VideoStream {
            codec: "hevc".into(),
            width: 1920,
            height: 1080,
            ..Default::default()
        }],
        audio: vec![AudioStream {
            codec: "aac".into(),
            channels: 8,
            sample_rate: 48_000,
            ..Default::default()
        }],
        ..Default::default()
    };
    let parts = [PartSource {
        head_xxh3: 0,
        tail_xxh3: 0,
        file_id: crate::sessions::FileId::Catalogue("fixture".into()),
        module_id: "mediahost".into(),
        collection_id: "movies".into(),
        root_token: "root".into(),
        path_rel: "movie.mkv".into(),
        size: 1_000_000,
        mtime_unix: 1,
        base_ms: 0,
        duration_ms: 60_000,
    }];

    let measurement = kahawai_media::loudness::AudioLoudnessMeasurement {
        source: kahawai_media::loudness::AudioLayout::new(8, 0xc3f),
        layouts: vec![(8, 0xc3f), (6, 0x3f), (2, 0x3)]
            .into_iter()
            .map(
                |(channels, channel_mask)| kahawai_media::loudness::AudioLayoutLoudness {
                    layout: kahawai_media::loudness::AudioLayout::new(channels, channel_mask),
                    loudness: kahawai_media::loudness::AudioLoudness {
                        integrated_lufs: -24.0,
                        true_peak_dbtp: -8.0,
                    },
                },
            )
            .collect(),
    };

    assert!(
        negotiation
            .probe(&info, false, None)
            .full_protocol
            .supports(kahawai_proto::ProtocolFeature::ExactAudioLoudnessGains),
        "protocol 4.0 did not expose inherited exact layout gains"
    );
    assert!(
        negotiation
            .probe(
                &info,
                false,
                Some(kahawai_proto::ProtocolFeature::ExactAudioLoudnessGains),
            )
            .full_protocol
            .supports(kahawai_proto::ProtocolFeature::ExactAudioLoudnessGains),
        "the exact probe lost a protocol-4 baseline feature"
    );
    let normal = negotiation.plan_with_probe(&parts, &info, false, None);
    assert_eq!(normal.plan.video, kahawai_media::remux::StreamMode::Encode);
    assert_ne!(normal.plan.audio, kahawai_media::remux::StreamMode::Encode);
    let forced = negotiation.plan_with_probe(&parts, &info, false, Some(&measurement));
    assert_eq!(forced.plan.video, kahawai_media::remux::StreamMode::Encode);
    assert_eq!(
        forced.plan.audio,
        kahawai_media::remux::StreamMode::Encode,
        "force normalization was suppressed at the protocol-4 baseline"
    );

    let mut direct_info = info.clone();
    direct_info.container = Some("mp4".into());
    direct_info.video[0].codec = "h264".into();
    let direct = negotiation.plan_with_probe(&parts, &direct_info, false, None);
    assert_eq!(direct.cost, kahawai_media::negotiate::Cost::Direct);
    assert!(
        source_choice_key(&direct, true) < source_choice_key(&normal, false),
        "measured video transcode outranked unmeasured direct play"
    );
    assert!(
        source_choice_key(&direct, false) < source_choice_key(&direct, true),
        "force capability did not break an ordinary-cost tie"
    );

    assert!(registry.unregister_tc_link_if_current("baseline-fast", &baseline_fast));
    let fallback = negotiation.plan_with_probe(&parts, &info, false, Some(&measurement));
    assert_eq!(
        fallback.plan.audio,
        kahawai_media::remux::StreamMode::Encode,
        "another protocol-4 baseline worker lost exact layout gains"
    );

    let mut stereo_info = info.clone();
    stereo_info.audio[0].channels = 2;
    let stereo = kahawai_media::loudness::AudioLoudnessMeasurement {
        source: kahawai_media::loudness::AudioLayout::new(2, 0x3),
        layouts: vec![kahawai_media::loudness::AudioLayoutLoudness {
            layout: kahawai_media::loudness::AudioLayout::new(2, 0x3),
            loudness: kahawai_media::loudness::AudioLoudness {
                integrated_lufs: -24.0,
                true_peak_dbtp: -8.0,
            },
        }],
    };
    let stereo_normal = negotiation.plan_with_probe(&parts, &stereo_info, false, None);
    let baseline = negotiation.plan_with_probe(&parts, &stereo_info, false, Some(&stereo));
    assert_eq!(
        baseline.plan.audio,
        kahawai_media::remux::StreamMode::Encode,
        "protocol 4.0 did not expose exact stereo gain support"
    );
    assert_ne!(baseline.plan.audio, stereo_normal.plan.audio);
}
