use kahawai_core::media::{MediaInfo, VideoGeometry, VideoStream};
use kahawai_hub::registry::{FileUpsertRecord, Registry, SourcePath};

async fn fixture() -> (sqlx::SqlitePool, Registry, i64, String) {
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    let registry = Registry::new(db.clone(), Default::default());
    registry
        .record_satellite("m", "mediahost", "m", "fp")
        .await
        .unwrap();
    registry
        .announce_collection("m", "movies", "movies", &["/media".into()])
        .await
        .unwrap();
    let root_token = kahawai_core::media::root_token(std::path::Path::new("/media"));
    let info = MediaInfo {
        container: Some("matroska".into()),
        video: vec![VideoStream {
            codec: "h264".into(),
            width: 720,
            height: 480,
            ..Default::default()
        }],
        ..Default::default()
    };
    registry
        .upsert_files(
            "m",
            "movies",
            vec![FileUpsertRecord {
                root_token: root_token.clone(),
                path_rel: "anamorphic.mkv".into(),
                size: 100,
                mtime_unix: 1,
                head_xxh3: 2,
                tail_xxh3: 3,
                oshash: 4,
                streams_json: serde_json::to_string(&info).unwrap(),
            }],
        )
        .await
        .unwrap();
    let source_id = registry
        .source_id("m", "movies", &root_token, "anamorphic.mkv")
        .await
        .unwrap()
        .unwrap();
    (db, registry, source_id, root_token)
}

#[tokio::test]
async fn geometry_is_targeted_source_state_not_reconciliation_state() {
    let (db, registry, source_id, root_token) = fixture().await;
    assert_eq!(
        registry
            .video_geometry_worklist("m", "movies")
            .await
            .unwrap(),
        [SourcePath {
            root_token: root_token.clone(),
            path_rel: "anamorphic.mkv".into(),
        }]
    );
    let generation: i64 = sqlx::query_scalar(
        "SELECT sync_version FROM collections WHERE module_id='m' AND collection_id='movies'",
    )
    .fetch_one(&db)
    .await
    .unwrap();

    let geometry = vec![VideoGeometry {
        pixel_aspect_ratio: (32, 27),
        orientation: "rotate-90".into(),
        display_width: 480,
        display_height: 853,
    }];
    assert!(
        registry
            .record_file_video_geometry(
                "m",
                "movies",
                &root_token,
                "anamorphic.mkv",
                100,
                &serde_json::to_string(&geometry).unwrap(),
                "",
            )
            .await
            .unwrap()
    );
    assert!(
        registry
            .video_geometry_worklist("m", "movies")
            .await
            .unwrap()
            .is_empty()
    );
    let stored: String = sqlx::query_scalar("SELECT streams_json FROM files WHERE id=?")
        .bind(source_id)
        .fetch_one(&db)
        .await
        .unwrap();
    let stored: MediaInfo = serde_json::from_str(&stored).unwrap();
    assert!(stored.video_geometry_probed);
    assert_eq!(stored.video_geometry_error, None);
    assert_eq!(stored.video[0].pixel_aspect_ratio, Some((32, 27)));
    assert_eq!(stored.video[0].orientation.as_deref(), Some("rotate-90"));
    assert_eq!(
        (
            stored.video[0].display_width,
            stored.video[0].display_height
        ),
        (Some(480), Some(853))
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT sync_version FROM collections WHERE module_id='m' AND collection_id='movies'"
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        generation,
        "targeted geometry changed scan generation"
    );

    // A result raced by physical replacement cannot land on the new bytes.
    assert!(
        !registry
            .record_file_video_geometry(
                "m",
                "movies",
                &root_token,
                "anamorphic.mkv",
                99,
                &serde_json::to_string(&geometry).unwrap(),
                "",
            )
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn a_probe_failure_is_terminal_for_only_that_source_revision() {
    let (_db, registry, _source_id, root_token) = fixture().await;
    assert!(
        registry
            .record_file_video_geometry(
                "m",
                "movies",
                &root_token,
                "anamorphic.mkv",
                100,
                "",
                "discoverer timed out",
            )
            .await
            .unwrap()
    );
    assert!(
        registry
            .video_geometry_worklist("m", "movies")
            .await
            .unwrap()
            .is_empty(),
        "failed source became an infinite work loop"
    );

    // Changed physical content arrives through the ordinary bounded upsert and
    // replaces the old MediaInfo, making only this source eligible again.
    let info = MediaInfo {
        container: Some("matroska".into()),
        video: vec![VideoStream {
            codec: "h264".into(),
            width: 720,
            height: 480,
            ..Default::default()
        }],
        ..Default::default()
    };
    registry
        .upsert_files(
            "m",
            "movies",
            vec![FileUpsertRecord {
                root_token: root_token.clone(),
                path_rel: "anamorphic.mkv".into(),
                size: 101,
                mtime_unix: 2,
                head_xxh3: 5,
                tail_xxh3: 6,
                oshash: 7,
                streams_json: serde_json::to_string(&info).unwrap(),
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        registry
            .video_geometry_worklist("m", "movies")
            .await
            .unwrap()
            .len(),
        1
    );
}
