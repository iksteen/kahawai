mod common;
use common::*;
use kahawai_mediadb::*;
use kahawai_proto::v1 as p;
use prost::Message;

/// A file record with chosen bytes identity, so tests can order by mtime
/// and change size without touching the shared fixture.
fn file_at(version: u64, path: &str, size: u64, mtime: i64) -> p::CatalogRecord {
    file_with(
        version,
        path,
        size,
        mtime,
        kahawai_core::media::MediaInfo::default(),
    )
}

fn file_with(
    version: u64,
    path: &str,
    size: u64,
    mtime: i64,
    media: kahawai_core::media::MediaInfo,
) -> p::CatalogRecord {
    let payload = p::FileUpsert {
        collection_id: String::new(),
        files: vec![p::FileRecord {
            source: Some(p::SourcePath::new("root", path)),
            size,
            mtime_unix: mtime,
            head_xxh3: 1,
            tail_xxh3: 2,
            oshash: 3,
            streams_json: serde_json::to_string(&media).unwrap(),
        }],
    }
    .encode_to_vec();
    p::CatalogRecord {
        version,
        kind: "file".into(),
        key: format!("root\0{path}").into_bytes(),
        payload,
        deleted: false,
    }
}

async fn states(store: &Store) -> Vec<(String, i64)> {
    states_of(store, "text").await
}

async fn states_of(store: &Store, kind: &str) -> Vec<(String, i64)> {
    store
        .subtitle_jobs_status()
        .await
        .unwrap()
        .into_iter()
        .filter(|s| s.kind == kind)
        .map(|s| (s.state, s.count))
        .collect()
}

async fn films(store: &Store, records: Vec<p::CatalogRecord>) {
    store
        .offer_collection("host", &offer("films", MediaType::Movies, 1))
        .await
        .unwrap();
    store
        .apply_catalogue("host", &delta("films", true, true, 1, records))
        .await
        .unwrap();
}

#[tokio::test]
async fn every_probed_file_gets_a_row_and_a_claim_ranks_them() {
    let (_dir, store) = store().await;
    store
        .offer_collection("host", &offer("films", MediaType::Movies, 3))
        .await
        .unwrap();
    store
        .offer_collection("host", &offer("shows", MediaType::Series, 3))
        .await
        .unwrap();
    store
        .apply_catalogue(
            "host",
            &delta(
                "films",
                true,
                true,
                3,
                vec![
                    file_at(1, "Old Film (1990).mkv", 10, 100),
                    file_at(2, "New Film (2020).mkv", 10, 300),
                ],
            ),
        )
        .await
        .unwrap();
    store
        .apply_catalogue(
            "host",
            &delta(
                "shows",
                true,
                true,
                3,
                vec![
                    file_at(1, "Show/Season 1/Show S01E01.mkv", 10, 900),
                    file_at(2, "Show/Season 1/Show S01E02.mkv", 10, 900),
                ],
            ),
        )
        .await
        .unwrap();
    assert_eq!(states(&store).await, vec![("pending".to_string(), 4)]);
    assert_eq!(
        states_of(&store, "sets").await,
        vec![("pending".to_string(), 4)]
    );

    // The episode someone is watching outranks every movie; within the
    // rest, movies first, then newest mtime, then path.
    let shows = store.collections("host").await.unwrap();
    let shows = shows.iter().find(|c| c.remote_id == "shows").unwrap();
    let watching = store
        .files(&shows.id)
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.path.ends_with("E02.mkv"))
        .unwrap()
        .item_id
        .unwrap();
    let jobs = store
        .claim_subtitle_jobs("text", "host", &[watching], 1_000, 60, 10)
        .await
        .unwrap();
    let paths: Vec<&str> = jobs.iter().map(|j| j.file.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "Show/Season 1/Show S01E01.mkv",
            "Show/Season 1/Show S01E02.mkv",
            "New Film (2020).mkv",
            "Old Film (1990).mkv",
        ]
    );
    assert!(
        jobs.iter()
            .all(|j| j.file.host == "host" && j.attempts == 1)
    );
    assert_eq!(states(&store).await, vec![("running".to_string(), 4)]);
    // Claimed rows are leased: nothing more to claim until the lease lapses.
    assert!(
        store
            .claim_subtitle_jobs("text", "host", &[], 1_000, 60, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.subtitle_jobs_next_due("text").await.unwrap(),
        Some(1_060)
    );
    assert_eq!(
        store
            .claim_subtitle_jobs("text", "host", &[], 1_061, 60, 10)
            .await
            .unwrap()
            .len(),
        4
    );
    // Another host sees none of them: the bytes are not its to walk.
    assert!(
        store
            .claim_subtitle_jobs("text", "other", &[], 5_000, 60, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_byte_change_resets_a_settled_row_and_a_version_bump_does_not() {
    let (_dir, store) = store().await;
    films(&store, vec![file_at(1, "Film.mkv", 10, 100)]).await;
    assert!(
        store
            .finish_subtitle_source(
                "host",
                "films",
                &p::SourcePath::new("root", "Film.mkv"),
                "text"
            )
            .await
            .unwrap()
    );
    assert_eq!(states(&store).await, vec![("done".to_string(), 1)]);

    store
        .apply_catalogue(
            "host",
            &delta(
                "films",
                false,
                true,
                2,
                vec![file_at(2, "Film.mkv", 10, 100)],
            ),
        )
        .await
        .unwrap();
    assert_eq!(states(&store).await, vec![("done".to_string(), 1)]);

    store
        .apply_catalogue(
            "host",
            &delta(
                "films",
                false,
                true,
                3,
                vec![file_at(3, "Film.mkv", 11, 100)],
            ),
        )
        .await
        .unwrap();
    assert_eq!(states(&store).await, vec![("pending".to_string(), 1)]);
}

#[tokio::test]
async fn a_reconnect_or_rerun_releases_rows_and_a_reported_failure_backs_off() {
    let (_dir, store) = store().await;
    films(&store, vec![file_at(1, "Film.mkv", 10, 100)]).await;
    let claim = |now: i64| store.claim_subtitle_jobs("text", "host", &[], now, 60, 1);
    let job = claim(0).await.unwrap().remove(0);
    assert_eq!(job.attempts, 1);
    assert_eq!(states(&store).await, vec![("running".to_string(), 1)]);

    assert_eq!(store.release_subtitle_host("other").await.unwrap(), 0);
    assert_eq!(store.release_subtitle_host("host").await.unwrap(), 1);
    assert_eq!(states(&store).await, vec![("pending".to_string(), 1)]);
    let job = claim(0).await.unwrap().remove(0);
    assert_eq!(job.attempts, 1, "a reconnect is not a failure");

    let source = p::SourcePath::new("root", "Film.mkv");
    assert!(
        !store
            .fail_subtitle_source(
                "host",
                "films",
                &p::SourcePath::new("root", "No.mkv"),
                "text",
                5,
                "x"
            )
            .await
            .unwrap()
    );
    assert!(
        store
            .fail_subtitle_source("host", "films", &source, "text", 5, "demux failed")
            .await
            .unwrap()
    );
    assert_eq!(states(&store).await, vec![("retry".to_string(), 1)]);
    assert_eq!(store.subtitle_jobs_next_due("text").await.unwrap(), Some(5));
    assert!(claim(4).await.unwrap().is_empty());
    for round in 2..=BLOCK_AFTER_ATTEMPTS {
        let job = claim(10_000 * round).await.unwrap().remove(0);
        assert_eq!(job.attempts, round);
        store
            .fail_subtitle_source("host", "films", &source, "text", 0, "demux failed")
            .await
            .unwrap();
    }
    assert_eq!(states(&store).await, vec![("blocked".to_string(), 1)]);
    assert!(claim(i64::MAX / 4).await.unwrap().is_empty());
    assert_eq!(store.rerun_subtitle_jobs("text").await.unwrap(), 1);
    let job = claim(0).await.unwrap().remove(0);
    assert_eq!(job.attempts, 1);
    store
        .finish_subtitle_job(&job.file.file_id, "text")
        .await
        .unwrap();
    assert_eq!(states(&store).await, vec![("done".to_string(), 1)]);

    // Removing the file removes its work.
    store
        .apply_catalogue(
            "host",
            &delta(
                "films",
                false,
                true,
                2,
                vec![p::CatalogRecord {
                    version: 2,
                    kind: "file".into(),
                    key: b"root\0Film.mkv".to_vec(),
                    payload: vec![],
                    deleted: true,
                }],
            ),
        )
        .await
        .unwrap();
    assert!(states(&store).await.is_empty());
}

#[tokio::test]
async fn a_source_resolves_by_media_path_or_by_sidecar_path() {
    let (_dir, store) = store().await;
    let media = kahawai_core::media::MediaInfo {
        external_subtitles: vec![kahawai_core::media::SidecarSubtitle {
            path_rel: "Film.idx".into(),
            format: "vobsub".into(),
            language: None,
            track: Some(0),
        }],
        ..Default::default()
    };
    films(&store, vec![file_with(1, "Film.mkv", 10, 100, media)]).await;
    for path in ["Film.mkv", "Film.idx"] {
        let file = store
            .source_file("host", "films", &p::SourcePath::new("root", path))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{path} resolves"));
        assert_eq!(
            (file.path.as_str(), file.host.as_str()),
            ("Film.mkv", "host")
        );
        assert_eq!(file.media.external_subtitles.len(), 1);
    }
    assert!(
        store
            .source_file("host", "films", &p::SourcePath::new("root", "Film.sub"))
            .await
            .unwrap()
            .is_none()
    );
    // A landing that names the sidecar settles the media file's row.
    assert!(
        store
            .finish_subtitle_source(
                "host",
                "films",
                &p::SourcePath::new("root", "Film.idx"),
                "sets"
            )
            .await
            .unwrap()
    );
    assert_eq!(
        states_of(&store, "sets").await,
        vec![("done".to_string(), 1)]
    );
    assert_eq!(states(&store).await, vec![("pending".to_string(), 1)]);
}

#[tokio::test]
async fn dispatch_window_is_shared_across_collections_and_atomic_between_claims() {
    let (_dir, store) = store().await;
    for collection in ["films", "extras"] {
        store
            .offer_collection("host", &offer(collection, MediaType::Movies, 20))
            .await
            .unwrap();
        let records = (1..=20)
            .map(|n| file_at(n, &format!("Film {n}.mkv"), 10, n as i64))
            .collect();
        store
            .apply_catalogue("host", &delta(collection, true, true, 20, records))
            .await
            .unwrap();
    }
    let (first, second) = tokio::join!(
        store.claim_subtitle_jobs("text", "host", &[], 1000, 60, 16),
        store.claim_subtitle_jobs("text", "host", &[], 1000, 60, 16),
    );
    let jobs: Vec<_> = first.unwrap().into_iter().chain(second.unwrap()).collect();
    assert_eq!(jobs.len(), 16, "concurrent claims share a host-wide window");
    assert_eq!(
        store
            .subtitle_dispatch_state("text", "host", 1000, 8)
            .await
            .unwrap(),
        (16, Some(1060)),
        "a full window sleeps until expiry despite pending rows"
    );
    assert_eq!(
        store
            .subtitle_dispatch_state("text", "other", 1000, 8)
            .await
            .unwrap(),
        (0, None)
    );
    for job in jobs.iter().take(7) {
        store
            .finish_subtitle_job(&job.file.file_id, "text")
            .await
            .unwrap();
    }
    assert_eq!(
        store
            .subtitle_dispatch_state("text", "host", 1000, 8)
            .await
            .unwrap(),
        (9, Some(1060))
    );
    store
        .finish_subtitle_job(&jobs[7].file.file_id, "text")
        .await
        .unwrap();
    assert_eq!(
        store
            .subtitle_dispatch_state("text", "host", 1000, 8)
            .await
            .unwrap(),
        (8, Some(0))
    );
    assert_eq!(
        store
            .claim_subtitle_jobs("text", "host", &[], 1000, 60, 16)
            .await
            .unwrap()
            .len(),
        8
    );
    assert_eq!(
        store
            .claim_subtitle_jobs("sets", "host", &[], 1000, 60, 16)
            .await
            .unwrap()
            .len(),
        16,
        "each kind has its own window"
    );
    assert_eq!(
        store
            .subtitle_dispatch_state("text", "host", 1061, 8)
            .await
            .unwrap()
            .0,
        0
    );
    assert_eq!(
        store
            .claim_subtitle_jobs("text", "host", &[], 1061, 60, 16)
            .await
            .unwrap()
            .len(),
        16,
        "expired leases permit a bounded retry"
    );
    store.release_subtitle_host("host").await.unwrap();
    assert_eq!(
        store
            .claim_subtitle_jobs("text", "host", &[], 1062, 60, 16)
            .await
            .unwrap()
            .len(),
        16,
        "reconnect does not dump the released backlog"
    );
}
