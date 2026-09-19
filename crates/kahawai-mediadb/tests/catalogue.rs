mod common;
use common::*;
use kahawai_mediadb::*;
use kahawai_proto::v1 as p;
use prost::Message;

#[tokio::test]
async fn snapshots_replay_reopen_and_reconcile_only_the_final_generation() {
    let (dir, s) = store().await;
    let col = collection(
        &s,
        "c",
        MediaType::Movies,
        &["One.2000.mkv", "Two.2000.mkv"],
    )
    .await;
    let one = s
        .collection_items(&col)
        .await
        .unwrap()
        .into_iter()
        .find(|i| i.detected.title == "One")
        .unwrap();
    let metadata = s
        .put_provider_record(&record(
            "tmdb",
            "1",
            "Manual",
            Some(2002),
            MediaType::Movies,
        ))
        .await
        .unwrap();
    s.assign_metadata(&one.id, Some(&metadata)).await.unwrap();
    let old = s.files(&col).await.unwrap();
    assert_eq!(old[0].head_hash, Some(u64::MAX));
    let mut o = offer("c", MediaType::Movies, 5);
    o.oldest_replayable_version = 3;
    assert!(s.offer_collection("host", &o).await.unwrap().1.snapshot);
    s.apply_catalogue(
        "host",
        &delta("c", true, false, 0, vec![file(4, "Three.2000.mkv")]),
    )
    .await
    .unwrap();
    assert_eq!(s.files(&col).await.unwrap().len(), 3);
    assert_eq!(s.catalogue_cursor(&col).await.unwrap().version, 0);
    s.close().await;
    let s = Store::open(&dir.path().join("mediadb.db")).await.unwrap();
    assert!(s.offer_collection("host", &o).await.unwrap().1.snapshot);
    // Earlier incomplete snapshot's Three is not part of this completed one.
    let final_chunk = delta("c", true, true, 5, vec![file(1, "One.2000.mkv")]);
    s.apply_catalogue("host", &final_chunk).await.unwrap();
    s.apply_catalogue("host", &final_chunk).await.unwrap();
    assert_eq!(s.files(&col).await.unwrap().len(), 1);
    let after = s.collection_items(&col).await.unwrap();
    assert_eq!(after[0].id, one.id);
    let identified = after[0].library_item_id.clone();
    assert!(!s.library_item_record(&identified).await.unwrap().archived);
    assert_eq!(after[0].selected_record.as_deref(), Some(metadata.as_str()));
    assert_eq!(s.catalogue_cursor(&col).await.unwrap().version, 5);
    let update = delta("c", false, true, 7, vec![file(7, "Four.2000.mkv")]);
    s.apply_catalogue("host", &update).await.unwrap();
    s.apply_catalogue("host", &update).await.unwrap();
    assert_eq!(s.files(&col).await.unwrap().len(), 2);
    let remove = p::CatalogRecord {
        version: 8,
        kind: "file".into(),
        key: b"root\0One.2000.mkv".to_vec(),
        deleted: true,
        ..Default::default()
    };
    s.apply_catalogue("host", &delta("c", false, true, 8, vec![remove]))
        .await
        .unwrap();
    assert_eq!(s.collection_items(&col).await.unwrap().len(), 1);
    assert!(s.library_item_record(&identified).await.unwrap().archived);
}
#[tokio::test]
async fn chunks_are_atomic_and_cursor_cannot_pass_missing_snapshot_data() {
    let (_dir, s) = store().await;
    let col = collection(&s, "c", MediaType::Movies, &["One.2000.mkv"]).await;
    let mut unsupported = file(3, "Unknown.2000.mkv");
    unsupported.kind = "future_kind".into();
    assert!(
        s.apply_catalogue(
            "host",
            &delta(
                "c",
                false,
                true,
                3,
                vec![file(2, "Two.2000.mkv"), unsupported]
            )
        )
        .await
        .is_err()
    );
    assert_eq!(s.files(&col).await.unwrap().len(), 1);
    assert_eq!(s.catalogue_cursor(&col).await.unwrap().version, 1);
    let mut o = offer("c", MediaType::Movies, 10);
    o.oldest_replayable_version = 2;
    s.offer_collection("host", &o).await.unwrap();
    s.apply_catalogue(
        "host",
        &delta("c", true, false, 0, vec![file(9, "Nine.2000.mkv")]),
    )
    .await
    .unwrap();
    assert!(
        s.apply_catalogue("host", &delta("c", true, true, 8, vec![]))
            .await
            .is_err()
    );
    assert_eq!(s.files(&col).await.unwrap().len(), 2);
    assert!(s.catalogue_cursor(&col).await.unwrap().snapshot);
    s.apply_catalogue("host", &delta("c", true, true, 10, vec![]))
        .await
        .unwrap();
    assert_eq!(s.files(&col).await.unwrap().len(), 1);
    let mut epoch = offer("c", MediaType::Movies, 1);
    epoch.epoch = "new".into();
    s.offer_collection("host", &epoch).await.unwrap();
    let mut data = delta("c", true, true, 1, vec![file(1, "New.2001.mkv")]);
    data.epoch = "new".into();
    s.apply_catalogue("host", &data).await.unwrap();
    assert_eq!(s.files(&col).await.unwrap()[0].path, "New.2001.mkv");
}
fn fact(version: u64, kind: &str, payload: Vec<u8>) -> p::CatalogRecord {
    p::CatalogRecord {
        version,
        kind: kind.into(),
        key: b"root\0Film.2000.mkv".to_vec(),
        payload,
        deleted: false,
    }
}
#[tokio::test]
async fn source_facts_roundtrip_and_revisions_and_tombstones_remove_old_observations() {
    let (_dir, s) = store().await;
    let col = collection(&s, "c", MediaType::Movies, &["Film.2000.mkv"]).await;
    let source = Some(p::SourcePath::new("root", "Film.2000.mkv"));
    let values = vec![
        fact(
            2,
            "file_error",
            p::FileError {
                collection_id: "c".into(),
                source: source.clone(),
                error: "diagnostic".into(),
            }
            .encode_to_vec(),
        ),
        fact(
            3,
            "file_hashes",
            p::FileHashes {
                collection_id: "c".into(),
                hashes: vec![p::FileHash {
                    source: source.clone(),
                    size: 123,
                    ed2k_hex: "abcd".into(),
                    ..Default::default()
                }],
            }
            .encode_to_vec(),
        ),
        fact(
            4,
            "file_loudness",
            p::FileLoudness {
                collection_id: "c".into(),
                source: source.clone(),
                size: 123,
                mtime_unix: 456,
                analyzer: 4,
                ..Default::default()
            }
            .encode_to_vec(),
        ),
        fact(
            5,
            "file_attachments",
            p::FileAttachments {
                collection_id: "c".into(),
                source: source.clone(),
                size: 123,
                attachments_json: "[]".into(),
                chapters_json: Some("[]".into()),
            }
            .encode_to_vec(),
        ),
        fact(
            6,
            "file_keyframe",
            p::FileKeyframeInterval {
                collection_id: "c".into(),
                source: source.clone(),
                size: 123,
                max_keyframe_interval_ms: Some(2000),
            }
            .encode_to_vec(),
        ),
        fact(
            7,
            "file_geometry",
            p::FileVideoGeometry {
                collection_id: "c".into(),
                source: source.clone(),
                size: 123,
                geometry_json: "[]".into(),
                error: String::new(),
            }
            .encode_to_vec(),
        ),
        fact(
            8,
            "file_segments",
            p::SegmentDetectionResult {
                collection_id: "c".into(),
                episodes: vec![p::SegmentEpisodeResult {
                    source,
                    observed_size: 123,
                    observed_mtime_unix: 456,
                    ..Default::default()
                }],
                ..Default::default()
            }
            .encode_to_vec(),
        ),
    ];
    s.apply_catalogue("host", &delta("c", false, true, 8, values))
        .await
        .unwrap();
    let file = s.files(&col).await.unwrap().remove(0).id;
    assert_eq!(s.source_facts(&file).await.unwrap().len(), 7);
    let mut deleted = fact(9, "file_hashes", vec![]);
    deleted.deleted = true;
    s.apply_catalogue("host", &delta("c", false, true, 9, vec![deleted]))
        .await
        .unwrap();
    assert_eq!(s.source_facts(&file).await.unwrap().len(), 6);
    let mut replacement = file_media(10, "Film.2000.mkv", Default::default());
    let mut base = p::FileUpsert::decode(replacement.payload.as_slice()).unwrap();
    base.files[0].head_xxh3 = 42;
    replacement.payload = base.encode_to_vec();
    s.apply_catalogue("host", &delta("c", false, true, 10, vec![replacement]))
        .await
        .unwrap();
    assert!(s.source_facts(&file).await.unwrap().is_empty());
    let stale = fact(
        11,
        "file_keyframe",
        p::FileKeyframeInterval {
            collection_id: "c".into(),
            source: Some(p::SourcePath::new("root", "Film.2000.mkv")),
            size: 999,
            ..Default::default()
        }
        .encode_to_vec(),
    );
    assert!(
        s.apply_catalogue("host", &delta("c", false, true, 11, vec![stale]))
            .await
            .is_err()
    );
    assert_eq!(s.catalogue_cursor(&col).await.unwrap().version, 10);
}

#[tokio::test]
async fn a_diagnostic_without_a_probe_is_preserved_but_not_browsable() {
    let (_dir, s) = store().await;
    let (col, _) = s
        .offer_collection("host", &offer("c", MediaType::Movies, 1))
        .await
        .unwrap();
    let error = fact(
        1,
        "file_error",
        p::FileError {
            collection_id: "c".into(),
            source: Some(p::SourcePath::new("root", "Film.2000.mkv")),
            error: "unreadable".into(),
        }
        .encode_to_vec(),
    );
    s.apply_catalogue("host", &delta("c", true, true, 1, vec![error]))
        .await
        .unwrap();
    let files = s.files(&col).await.unwrap();
    assert_eq!(files.len(), 1);
    assert!(files[0].media.is_none());
    assert!(s.collection_items(&col).await.unwrap().is_empty());
    assert!(matches!(
        s.source_facts(&files[0].id).await.unwrap()[0],
        SourceFact::Error(_)
    ));
}

#[tokio::test]
async fn opening_a_different_database_is_read_only_and_never_bootstraps_it() {
    use sqlx::Connection;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("other.db");
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql("CREATE TABLE other(value TEXT); INSERT INTO other VALUES('keep')")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(Store::open(&path).await.is_err());
    assert!(Store::create(&path).await.is_err());
    assert_eq!(before, std::fs::read(path).unwrap());
}

#[tokio::test]
async fn snapshot_error_replaces_an_old_probe_but_keeps_the_live_diagnostic() {
    let (_dir, s) = store().await;
    let col = collection(&s, "c", MediaType::Movies, &["Film.2000.mkv"]).await;
    let original = s.files(&col).await.unwrap().remove(0).id;
    let mut o = offer("c", MediaType::Movies, 3);
    o.oldest_replayable_version = 2;
    s.offer_collection("host", &o).await.unwrap();
    let error = fact(
        3,
        "file_error",
        p::FileError {
            collection_id: "c".into(),
            source: Some(p::SourcePath::new("root", "Film.2000.mkv")),
            error: "probe now fails".into(),
        }
        .encode_to_vec(),
    );
    s.apply_catalogue("host", &delta("c", true, true, 3, vec![error]))
        .await
        .unwrap();
    let files = s.files(&col).await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].id, original);
    assert!(files[0].media.is_none());
    assert!(s.collection_items(&col).await.unwrap().is_empty());
    let facts = s.source_facts(&original).await.unwrap();
    assert_eq!(facts.len(), 1);
    match &facts[0] {
        SourceFact::Error(e) => assert_eq!(e.error, "probe now fails"),
        _ => panic!("diagnostic lost"),
    }
}

#[tokio::test]
async fn snapshot_replaces_fact_versions_from_a_cursor_ahead_of_the_host() {
    let (_dir, s) = store().await;
    let col = collection(&s, "c", MediaType::Movies, &["Film.2000.mkv"]).await;
    let source = Some(p::SourcePath::new("root", "Film.2000.mkv"));
    let old = fact(
        10,
        "file_hashes",
        p::FileHashes {
            collection_id: "c".into(),
            hashes: vec![p::FileHash {
                source: source.clone(),
                size: 123,
                ed2k_hex: "old".into(),
                ..Default::default()
            }],
        }
        .encode_to_vec(),
    );
    s.apply_catalogue("host", &delta("c", false, true, 10, vec![old]))
        .await
        .unwrap();
    assert!(
        s.offer_collection("host", &offer("c", MediaType::Movies, 2))
            .await
            .unwrap()
            .1
            .snapshot
    );
    let new = fact(
        2,
        "file_hashes",
        p::FileHashes {
            collection_id: "c".into(),
            hashes: vec![p::FileHash {
                source,
                size: 123,
                ed2k_hex: "new".into(),
                ..Default::default()
            }],
        }
        .encode_to_vec(),
    );
    s.apply_catalogue(
        "host",
        &delta("c", true, true, 2, vec![file(1, "Film.2000.mkv"), new]),
    )
    .await
    .unwrap();
    let file = s.files(&col).await.unwrap().remove(0).id;
    match &s.source_facts(&file).await.unwrap()[0] {
        SourceFact::Hashes(h) => assert_eq!(h.hashes[0].ed2k_hex, "new"),
        _ => panic!("wrong fact"),
    }
}
