//! The built-in transcoder is an admin projection, never an enrolled peer.

use kahawai_hub::registry::Registry;

#[tokio::test]
async fn local_transcoder_is_visible_without_enrollment_and_cannot_be_removed() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let catalogue = kahawai_mediadb::Store::in_memory().await.unwrap();
    let registry = Registry::new(db.clone(), Default::default(), catalogue.clone());
    assert!(registry.satellites_overview().await.unwrap().is_empty());
    let registry = registry.with_local_video_executor(true);
    registry
        .ensure_local_satellite("local", "Local mediahost")
        .await
        .unwrap();
    registry.set_pace("local", "1080|av1|h264", 3.2);
    let rows = registry.satellites_overview().await.unwrap();
    assert_eq!(rows.len(), 2);
    let local = rows
        .iter()
        .find(|row| row.module_id == Registry::LOCAL_TRANSCODER)
        .unwrap();
    assert_eq!(local.module_type, "transcoder");
    assert_eq!(local.cert_fingerprint, Registry::IN_PROCESS);
    assert!(local.connected);
    assert!(!local.disabled);
    assert!(local.capabilities.is_none());
    assert_eq!(local.build.as_deref(), Some(kahawai_core::build_stamp()));
    assert_eq!(local.pace.len(), 1);
    assert_eq!(local.pace[0].multiple, 3.2);
    assert!(local.link_bytes_per_sec.is_none());
    assert!(registry.is_in_process(&local.module_id).await.unwrap());
    assert!(registry.delete_satellite(&local.module_id).await.is_err());
    let enrolled: Vec<String> = sqlx::query_scalar("SELECT module_id FROM satellites")
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(enrolled, ["local"]);
    // Returning to a plain hub does not leave a stale transcoder behind.
    let plain = Registry::new(db, Default::default(), catalogue);
    assert_eq!(plain.satellites_overview().await.unwrap().len(), 1);
}

#[tokio::test]
async fn disabling_local_video_preserves_lightweight_placement_and_survives_restart() {
    use kahawai_hub::registry::PlacementNeed;

    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let catalogue = kahawai_mediadb::Store::in_memory().await.unwrap();
    let registry = Registry::new(db.clone(), Default::default(), catalogue.clone())
        .with_local_video_executor(true);
    let video = PlacementNeed {
        encode_video: true,
        video_codec: "h264".into(),
        ..Default::default()
    };
    assert!(registry.place(&video).available);
    registry
        .set_disabled(Registry::LOCAL_TRANSCODER, true)
        .await
        .unwrap();
    assert!(registry.local_video_executor_present());
    assert!(!registry.local_video_executor_enabled());
    assert!(!registry.place(&video).available);
    for need in [
        PlacementNeed::default(),
        PlacementNeed {
            encode_audio: true,
            ..Default::default()
        },
    ] {
        let placement = registry.place(&need);
        assert!(placement.available);
        assert!(
            placement.target.is_none(),
            "remux and audio-only work must stay local"
        );
    }
    let local = registry.satellites_overview().await.unwrap().pop().unwrap();
    assert!(local.connected && local.disabled);
    assert!(registry.delete_satellite(&local.module_id).await.is_err());
    assert!(
        registry
            .satellites_overview()
            .await
            .unwrap()
            .pop()
            .unwrap()
            .disabled
    );

    // Re-open the on-disk database and use the real startup loader.
    let restored = Registry::new(
        kahawai_hub::db::open(dir.path()).await.unwrap(),
        Default::default(),
        catalogue.clone(),
    )
    .with_local_video_executor(true);
    restored.load_allowlist().await.unwrap();
    assert!(!restored.local_video_executor_enabled());
    restored
        .set_disabled(Registry::LOCAL_TRANSCODER, false)
        .await
        .unwrap();
    assert!(restored.local_video_executor_enabled());
    assert!(restored.place(&video).available);
    assert!(
        !restored
            .satellites_overview()
            .await
            .unwrap()
            .pop()
            .unwrap()
            .disabled
    );
    let restored_again = Registry::new(
        kahawai_hub::db::open(dir.path()).await.unwrap(),
        Default::default(),
        catalogue.clone(),
    )
    .with_local_video_executor(true);
    restored_again.load_allowlist().await.unwrap();
    assert!(restored_again.local_video_executor_enabled());

    // Startup config is structural; admin cannot enable an executor it omitted.
    let plain = Registry::new(db, Default::default(), catalogue);
    assert!(
        plain
            .set_disabled(Registry::LOCAL_TRANSCODER, false)
            .await
            .is_err()
    );
    assert!(!plain.local_video_executor_enabled());
}

#[tokio::test]
async fn local_measurements_are_projected_and_quarantined_encoders_are_excluded() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Registry::new(
        db,
        Default::default(),
        kahawai_mediadb::Store::in_memory().await.unwrap(),
    )
    .with_local_video_executor(true);
    let available = kahawai_media::remux::encoder_capabilities();
    let (_, element, _) = available
        .iter()
        .find(|(codec, _, _)| *codec == "h264")
        .expect("the test installation provides an H.264 encoder");
    let mut bench = kahawai_media::bench::BenchResults::default();
    bench.encoders.insert(
        (*element).into(),
        kahawai_media::bench::Speeds {
            s1080: Some(6.0),
            s2160: Some(2.0),
        },
    );
    registry.set_local_bench(bench.clone());
    let row = registry.satellites_overview().await.unwrap().pop().unwrap();
    let caps = row.capabilities.unwrap();
    assert_eq!(caps.encoders.len(), 1);
    assert_eq!(caps.encoders[0].element, *element);
    assert_eq!(caps.encoders[0].speed_1080, Some(6.0));
    assert_eq!(caps.encoders[0].speed_2160, Some(2.0));
    registry
        .set_disabled(Registry::LOCAL_TRANSCODER, true)
        .await
        .unwrap();
    let disabled = registry.satellites_overview().await.unwrap().pop().unwrap();
    assert!(disabled.disabled);
    assert_eq!(
        disabled.capabilities.unwrap().encoders[0].speed_1080,
        Some(6.0)
    );
    bench.crashes.insert((*element).into(), 1);
    registry.set_local_bench(bench);
    let row = registry.satellites_overview().await.unwrap().pop().unwrap();
    assert!(row.capabilities.unwrap().encoders.is_empty());
    assert!(
        registry.pick_transcoder(&Default::default()).is_none(),
        "a virtual row cannot acquire a satellite slot"
    );
}
