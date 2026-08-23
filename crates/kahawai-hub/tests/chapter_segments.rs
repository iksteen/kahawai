//! A season that names its own boundaries is answered from the names.
//!
//! The claim is not just that the numbers are right: a fully named season
//! dispatches no mediahost job. This harness registers a connected protocol-old
//! host, so any inferred analysis remains pending.

use std::sync::Arc;

use sqlx::Row;

/// Two episodes of a show, each with the chapter names a WEBRip carries.
async fn season(chapters: &[&str]) -> (tempfile::TempDir, Arc<kahawai_hub::registry::Registry>) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO collections(module_id,collection_id,media_type)
           VALUES('m','c','series');
         INSERT INTO collection_roots(module_id,collection_id,root_token,normalized_path)
           VALUES('m','c','r','/series');
         INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id)
           VALUES('show','show','Show','show','show','m','c');
         INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id,
                           parent_id,season,episode)
           VALUES('e1','episode','One','one','one','m','c','show',1,1),
                 ('e2','episode','Two','two','two','m','c','show',1,2);",
    )
    .execute(&db)
    .await
    .unwrap();

    let registry = Arc::new(kahawai_hub::registry::Registry::new(db, Default::default()));
    // The resolver the detector consults is connectivity-aware; the module
    // is "up" even though no byte plane exists in this harness, so reads
    // fail at the lease, not at planning.
    registry.connected("m", "mediahost", "m", "fp", "test");
    for (item, path) in [("e1", "e1.mkv"), ("e2", "e2.mkv")] {
        let mut info = kahawai_core::media::MediaInfo {
            duration_ms: Some(600_000),
            ..Default::default()
        };
        info.chapters = Some(
            chapters
                .iter()
                .enumerate()
                .map(|(at, title)| kahawai_core::media::Chapter {
                    start_ms: at as u64 * 60_000,
                    end_ms: None,
                    title: Some((*title).into()),
                })
                .collect(),
        );
        add_source(&registry, item, path, 1, info).await;
    }
    (dir, registry)
}

/// One more rendition of an item: a file, a single-part source, its part.
/// The mtime matters: the scan row must carry the READ file's, and a fixture
/// where every file shares one hid a wrong stamp completely.
async fn add_source(
    registry: &Arc<kahawai_hub::registry::Registry>,
    item: &str,
    path: &str,
    mtime: i64,
    info: kahawai_core::media::MediaInfo,
) {
    let db = registry.db();
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,
                           head_xxh3,tail_xxh3,oshash,streams_json)
         SELECT 'm','c',id,?,10,?,0,0,0,? FROM collection_roots RETURNING id",
    )
    .bind(path)
    .bind(mtime)
    .bind(serde_json::to_string(&info).unwrap())
    .fetch_one(db)
    .await
    .unwrap();
    let source_id: i64 = sqlx::query_scalar(
        "INSERT INTO playable_sources(module_id,collection_id,item_id,root_id,
                                      family_key,expected_parts)
         VALUES('m','c',?,NULL,?,1) RETURNING id",
    )
    .bind(item)
    .bind(format!("file:{path}"))
    .fetch_one(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO playable_source_parts(playable_source_id,module_id,collection_id,
                                           ordinal,file_id)
         VALUES(?,'m','c',1,?)",
    )
    .bind(source_id)
    .bind(file_id)
    .execute(db)
    .await
    .unwrap();
}

use kahawai_hub::segments::Analysis;

/// One pass, with the detector handed back so a test can read the status
/// flags the admin page's toast is driven by.
async fn analyze_on(
    registry: &Arc<kahawai_hub::registry::Registry>,
    detector: &kahawai_hub::segments::Detector,
) -> anyhow::Result<Analysis> {
    let dir = tempfile::tempdir().unwrap().keep();
    let sessions = Arc::new(kahawai_hub::sessions::Sessions::new(dir));
    detector
        .analyze_season(registry, &sessions, "show", 1)
        .await
}

async fn analyze(registry: &Arc<kahawai_hub::registry::Registry>) -> anyhow::Result<Analysis> {
    analyze_on(registry, &kahawai_hub::segments::Detector::new()).await
}

#[tokio::test]
async fn a_named_season_is_answered_without_reading_a_byte() {
    let (_dir, registry) = season(&["Recap", "Intro", "Part A", "Credits"]).await;

    // No byte plane exists here: reaching the fingerprint pass cannot succeed.
    let detector = kahawai_hub::segments::Detector::new();
    assert_eq!(
        analyze_on(&registry, &detector).await.unwrap(),
        Analysis {
            scanned: 2,
            awaiting: 0,
            // Zero is the branch's whole point: the names answered, so no
            // byte was attempted.
            attempted: 0
        }
    );

    let rows = sqlx::query(
        "SELECT item_id, kind, start_ms, end_ms, source FROM media_segments
          WHERE item_id = 'e1' ORDER BY start_ms",
    )
    .fetch_all(registry.db())
    .await
    .unwrap();
    let found: Vec<(String, i64, i64, String)> = rows
        .iter()
        .map(|r| {
            (
                r.get("kind"),
                r.get("start_ms"),
                r.get("end_ms"),
                r.get("source"),
            )
        })
        .collect();
    assert_eq!(
        found,
        [
            ("recap".to_string(), 0, 60_000, "chapter".to_string()),
            ("intro".into(), 60_000, 120_000, "chapter".into()),
            // The last chapter runs to the end of the episode.
            ("credits".into(), 180_000, 600_000, "chapter".into()),
        ]
    );

    // And both episodes are marked scanned, so the sweep moves on.
    let scans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_segment_scans")
        .fetch_one(registry.db())
        .await
        .unwrap();
    assert_eq!(scans, 2);

    // The flags the admin toast reads describe this pass: clean.
    let status = detector.status_counters();
    assert!(!status.awaiting_host && !status.last_failed && !status.running);
    assert_eq!(status.analyzed, 2);
}

#[tokio::test]
async fn a_named_pass_wipes_replaced_bytes_and_keeps_settled_ones() {
    // The all-named branch's wholesale decision, pinned in BOTH directions:
    // a PENDING episode (bytes new to the detector) starts clean — its old
    // bytes' inferred recap describes nothing on disk — while a SETTLED
    // episode keeps inferred rows for the kinds the names skip. The names
    // here skip the recap, so a surviving recap is possible at all.
    let (_dir, registry) = season(&["Intro", "Part A", "Part B", "Credits"]).await;
    sqlx::raw_sql(
        "INSERT INTO media_segments(item_id,kind,start_ms,end_ms,source)
           VALUES('e1','recap',0,30000,'blackframe'),
                 ('e2','recap',0,28000,'blackframe');",
    )
    .execute(registry.db())
    .await
    .unwrap();
    // e2 is SETTLED: its scan row carries the current generation and the
    // file's own mtime. e1 has no row, so it is the pending one.
    sqlx::query(
        "INSERT INTO media_segment_scans(item_id,scanned_at,detector,mtime_unix)
         VALUES('e2',1,?,1)",
    )
    .bind(kahawai_hub::segments::DETECTOR)
    .execute(registry.db())
    .await
    .unwrap();

    analyze(&registry).await.unwrap();

    let recaps: Vec<String> = sqlx::query_scalar(
        "SELECT item_id FROM media_segments WHERE kind='recap' ORDER BY item_id",
    )
    .fetch_all(registry.db())
    .await
    .unwrap();
    assert_eq!(
        recaps,
        ["e2"],
        "the pending episode's stale recap is wiped, the settled one's survives"
    );
}

#[tokio::test]
async fn a_connected_old_mediahost_is_awaited_not_failed() {
    let (_dir, registry) = season(&["Scene 1", "Scene 2"]).await;
    let detector = kahawai_hub::segments::Detector::new();
    assert_eq!(
        analyze_on(&registry, &detector).await.unwrap(),
        Analysis {
            scanned: 0,
            awaiting: 2,
            attempted: 0
        }
    );
    let status = detector.status_counters();
    assert!(status.awaiting_host);
    assert!(!status.last_failed);

    let scans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_segment_scans")
        .fetch_one(registry.db())
        .await
        .unwrap();
    assert_eq!(scans, 0, "nothing was read, so nothing is marked scanned");
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_segments")
        .fetch_one(registry.db())
        .await
        .unwrap();
    assert_eq!(stored, 0);
}

#[tokio::test]
async fn an_absent_host_is_awaited_not_failed() {
    // The same season behind a host that is GONE is the hub's weather: the
    // pass reports how many episodes wait, nothing errors, and nothing is
    // recorded — the sweep steps over it and retries next cycle.
    let (_dir, registry) = season(&["Scene 1", "Scene 2"]).await;
    registry.disconnected("m");

    let detector = kahawai_hub::segments::Detector::new();
    assert_eq!(
        analyze_on(&registry, &detector).await.unwrap(),
        Analysis {
            scanned: 0,
            awaiting: 2,
            attempted: 0
        }
    );
    let status = detector.status_counters();
    assert!(status.awaiting_host, "weather, and the toast says whose");
    assert!(!status.last_failed);
    let scans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_segment_scans")
        .fetch_one(registry.db())
        .await
        .unwrap();
    assert_eq!(scans, 0);
}

#[tokio::test]
async fn naming_only_the_credits_still_needs_the_season_compared() {
    // One kind named leaves the opening unknown, so the season needs a
    // mediahost job. This connected host predates the additive job message;
    // the half-answer remains pending rather than falling back to a lease.
    let (_dir, registry) = season(&["Scene 1", "Scene 2", "End Credits"]).await;
    assert_eq!(analyze(&registry).await.unwrap().awaiting, 2);

    let scans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_segment_scans")
        .fetch_one(registry.db())
        .await
        .unwrap();
    assert_eq!(scans, 0);
}

#[tokio::test]
async fn a_numbered_chapter_list_invents_nothing() {
    // Numbered chapters name nothing; the old connected mediahost cannot run
    // the inferred pass, so nothing is invented and the season stays pending.
    let (_dir, registry) = season(&["Chapter 1", "Chapter 2", "Chapter 3"]).await;
    assert_eq!(analyze(&registry).await.unwrap().awaiting, 2);

    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_segments")
        .fetch_one(registry.db())
        .await
        .unwrap();
    assert_eq!(stored, 0);
}

#[tokio::test]
async fn the_chapters_read_are_the_ones_that_would_play() {
    // Two encodes of the same episode, marked differently — an old 720p rip
    // inserted first and a 1080p REPACK after it. Segments are stored per
    // item, so the detector has to read the source playback would pick, or a
    // viewer of the REPACK gets the rip's boundaries.
    let (_dir, registry) = season(&["Recap", "Intro", "Part A", "Credits"]).await;
    for item in ["e1", "e2"] {
        let mut better = kahawai_core::media::MediaInfo {
            duration_ms: Some(600_000),
            video: vec![kahawai_core::media::VideoStream {
                codec: "h264".into(),
                width: 1920,
                height: 1080,
                ..Default::default()
            }],
            ..Default::default()
        };
        // The same four chapters, a minute later each: a different cut.
        better.chapters = Some(
            ["Recap", "Intro", "Part A", "Credits"]
                .iter()
                .enumerate()
                .map(|(at, title)| kahawai_core::media::Chapter {
                    start_ms: 60_000 + at as u64 * 60_000,
                    end_ms: None,
                    title: Some((*title).into()),
                })
                .collect(),
        );
        add_source(&registry, item, &format!("{item}-repack.mkv"), 2, better).await;
    }
    // The rip's bytes are NEWER than the REPACK's (a re-download, say):
    // ranking still picks the 1080p, so a scan stamped with the newest
    // sibling's mtime instead of the read file's is now a different number.
    sqlx::query("UPDATE files SET mtime_unix = 9 WHERE path_rel NOT LIKE '%repack%'")
        .execute(registry.db())
        .await
        .unwrap();

    assert_eq!(
        analyze(&registry).await.unwrap(),
        Analysis {
            scanned: 2,
            awaiting: 0,
            // Zero is the branch's whole point: the names answered, so no
            // byte was attempted.
            attempted: 0
        }
    );
    let start: i64 = sqlx::query_scalar(
        "SELECT start_ms FROM media_segments WHERE item_id='e1' AND kind='intro'",
    )
    .fetch_one(registry.db())
    .await
    .unwrap();
    assert_eq!(start, 120_000, "the REPACK's opening, not the old rip's");

    // The scan row names the file that was read — the REPACK, not the rip.
    // A wrong stamp is invisible to the boundary assertions above, and it
    // is the difference between a settled season and one the sweep re-reads
    // for ever.
    let stamped: Option<i64> =
        sqlx::query_scalar("SELECT mtime_unix FROM media_segment_scans WHERE item_id='e1'")
            .fetch_one(registry.db())
            .await
            .unwrap();
    assert_eq!(stamped, Some(2), "the mtime of the rendition that was read");

    // And the seam the row exists to close: the season the analysis just
    // answered is not offered again. This is the loop-termination property
    // itself — the stamp satisfying the pending predicate — and no other
    // test crosses it.
    let waiting = kahawai_hub::segments::pending_seasons(registry.db())
        .await
        .unwrap();
    assert!(
        waiting.is_empty(),
        "an answered season leaves the pending list: {waiting:?}"
    );
}

#[tokio::test]
async fn a_multi_part_item_is_skipped_rather_than_misread() {
    // `open_source` opens ONE file; an episode whose only rendition is
    // CD1/CD2 must not reach the byte path with a summed running time that
    // maps its credits window past CD1's end. It is skipped — no scan row,
    // so a future detector that learns parts can still pick it up.
    let (_dir, registry) = season(&["Scene 1", "Scene 2"]).await;
    // Replace both episodes' sources with two-part ones.
    sqlx::raw_sql("DELETE FROM playable_source_parts; DELETE FROM playable_sources;")
        .execute(registry.db())
        .await
        .unwrap();
    for item in ["e1", "e2"] {
        let source_id: i64 = sqlx::query_scalar(
            "INSERT INTO playable_sources(module_id,collection_id,item_id,root_id,
                                          family_key,expected_parts)
             VALUES('m','c',?,NULL,?,2) RETURNING id",
        )
        .bind(item)
        .bind(format!("parts:{item}"))
        .fetch_one(registry.db())
        .await
        .unwrap();
        for ordinal in [1i64, 2] {
            // root_id included, or `playable_rows`' inner join on
            // collection_roots drops the parts and the episode dies as "no
            // sources" before ever reaching the multi-part guard this test
            // exists to pin — deleting the guard kept the test green.
            let file_id: i64 = sqlx::query_scalar(
                "INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,
                                   head_xxh3,tail_xxh3,oshash,streams_json)
                 SELECT 'm','c',id,?,10,1,0,0,0,'{\"duration_ms\":600000}'
                   FROM collection_roots RETURNING id",
            )
            .bind(format!("{item}-cd{ordinal}.mkv"))
            .fetch_one(registry.db())
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO playable_source_parts(playable_source_id,module_id,collection_id,
                                                   ordinal,file_id)
                 VALUES(?,'m','c',?,?)",
            )
            .bind(source_id)
            .bind(ordinal)
            .bind(file_id)
            .execute(registry.db())
            .await
            .unwrap();
        }
    }

    // awaiting: 0 pins the chosen limitation from the round-3 commit: a
    // multi-part-only episode is skipped WITHOUT counting as awaiting, so
    // it must never put its season into the outage-retry loop.
    assert_eq!(
        analyze(&registry).await.unwrap(),
        Analysis {
            scanned: 0,
            awaiting: 0,
            attempted: 0
        },
        "nothing analysable, and nothing waiting"
    );
    let scans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_segment_scans")
        .fetch_one(registry.db())
        .await
        .unwrap();
    assert_eq!(scans, 0, "and nothing pretends to have been");
}
