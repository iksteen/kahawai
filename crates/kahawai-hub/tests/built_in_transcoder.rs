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
    assert!(registry.set_disabled(&local.module_id, true).await.is_err());
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
    bench.crashes.insert((*element).into(), 1);
    registry.set_local_bench(bench);
    let row = registry.satellites_overview().await.unwrap().pop().unwrap();
    assert!(row.capabilities.unwrap().encoders.is_empty());
    assert!(
        registry.pick_transcoder(&Default::default()).is_none(),
        "a virtual row cannot acquire a satellite slot"
    );
}
