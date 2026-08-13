//! MH-9 hub side: worklists only for anime collections, copy-forward by
//! content identity (the files table is the at-most-once journal), stale
//! results dropped, hashes cleared when content changes.

use kahawai_hub::registry::{FileUpsertRecord, Registry};

const TEST_ROOT: &str = "/kahawai-test-root";

fn rec(path: &str, size: u64, mtime: i64) -> FileUpsertRecord {
    FileUpsertRecord {
        root_token: kahawai_core::media::root_token(std::path::Path::new(TEST_ROOT)),
        path_rel: path.into(),
        size,
        mtime_unix: mtime,
        head_xxh3: size + 1,
        tail_xxh3: size + 2,
        oshash: size + 3,
        streams_json: r#"{"container":"matroska"}"#.into(),
    }
}

async fn setup(dir: &std::path::Path) -> Registry {
    let db = kahawai_hub::db::open(dir).await.unwrap();
    let reg = Registry::new(db, Default::default());
    reg.announce_collection("01H", "anime", "anime", &[TEST_ROOT.into()])
        .await
        .unwrap();
    reg.announce_collection("01H", "movies", "movies", &[TEST_ROOT.into()])
        .await
        .unwrap();
    reg.upsert_files(
        "01H",
        "anime",
        vec![rec("[G] Show - 01 [ABCD1234].mkv", 100, 1)],
    )
    .await
    .unwrap();
    reg.upsert_files("01H", "movies", vec![rec("Heat (1995).mkv", 200, 1)])
        .await
        .unwrap();
    reg
}

#[tokio::test]
async fn worklist_is_anime_only_and_shrinks_as_hashes_land() {
    let dir = tempfile::tempdir().unwrap();
    let reg = setup(dir.path()).await;

    assert!(reg.ed2k_worklist("01H", "movies").await.unwrap().is_empty());
    let work = reg.ed2k_worklist("01H", "anime").await.unwrap();
    assert_eq!(work.len(), 1);
    assert_eq!(
        work[0].root_token,
        kahawai_core::media::root_token(std::path::Path::new(TEST_ROOT))
    );
    assert_eq!(work[0].path_rel, "[G] Show - 01 [ABCD1234].mkv");

    // Stale result (size moved on): dropped, still on the list.
    assert!(
        !reg.record_ed2k(
            "01H",
            "anime",
            &work[0].root_token,
            &work[0].path_rel,
            "aa".repeat(16).as_str(),
            999,
        )
        .await
        .unwrap()
    );
    assert_eq!(reg.ed2k_worklist("01H", "anime").await.unwrap().len(), 1);

    // Matching result: stored, list empty.
    assert!(
        reg.record_ed2k(
            "01H",
            "anime",
            &work[0].root_token,
            &work[0].path_rel,
            "ab".repeat(16).as_str(),
            100,
        )
        .await
        .unwrap()
    );
    assert!(reg.ed2k_worklist("01H", "anime").await.unwrap().is_empty());
}

#[tokio::test]
async fn copy_forward_and_content_change_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let reg = setup(dir.path()).await;
    let hash = "cd".repeat(16);
    reg.record_ed2k(
        "01H",
        "anime",
        &kahawai_core::media::root_token(std::path::Path::new(TEST_ROOT)),
        "[G] Show - 01 [ABCD1234].mkv",
        &hash,
        100,
    )
    .await
    .unwrap();

    // A rename/copy with identical content identity inherits the hash —
    // no second full read ever happens for known content.
    reg.upsert_files("01H", "anime", vec![rec("renamed/Show - 01.mkv", 100, 1)])
        .await
        .unwrap();
    assert!(reg.ed2k_worklist("01H", "anime").await.unwrap().is_empty());

    // Unchanged re-upsert (forced re-inspection) keeps the hash…
    reg.upsert_files(
        "01H",
        "anime",
        vec![rec("[G] Show - 01 [ABCD1234].mkv", 100, 1)],
    )
    .await
    .unwrap();
    assert!(reg.ed2k_worklist("01H", "anime").await.unwrap().is_empty());

    // …but a content change (new mtime/size) clears it for rehashing.
    reg.upsert_files(
        "01H",
        "anime",
        vec![rec("[G] Show - 01 [ABCD1234].mkv", 101, 2)],
    )
    .await
    .unwrap();
    assert_eq!(
        reg.ed2k_worklist("01H", "anime").await.unwrap()[0].path_rel,
        "[G] Show - 01 [ABCD1234].mkv"
    );
}

#[tokio::test]
async fn anime_collection_resolves_movies_but_not_extras() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let reg = Registry::new(db.clone(), Default::default());
    reg.announce_collection("01H", "anime", "anime", &[TEST_ROOT.into()])
        .await
        .unwrap();
    reg.upsert_files(
        "01H",
        "anime",
        vec![
            rec("Howls Moving Castle (2004).mkv", 700, 1),
            rec(
                "[Coalgirls]_Ao_no_Exorcist_NCED2_(1280x720)_[9634C2F9].mkv",
                50,
                1,
            ),
        ],
    )
    .await
    .unwrap();

    let movie: Option<(String, Option<i64>)> =
        sqlx::query_as("SELECT title, year FROM items WHERE kind = 'movie'")
            .fetch_optional(&db)
            .await
            .unwrap();
    let (title, year) = movie.expect("anime movie resolved as a movie item");
    assert_eq!(title, "Howls Moving Castle");
    assert_eq!(year, Some(2004));

    // The creditless-ending extra binds by NAME into season 0's ED band
    // (HUB-30 designations) — it used to stay bare on the promise that
    // "ed2k will identify it later", which never came for a file no item
    // held. The hash still refines the slot when AniDB knows the file.
    let nc: Option<(Option<i64>, i64, String)> = sqlx::query_as(
        "SELECT i.season,i.episode,p.title
           FROM files f JOIN file_bindings fb ON fb.file_id=f.id
           JOIN items i ON i.id=fb.item_id JOIN items p ON p.id=i.parent_id
          WHERE f.path_rel LIKE '%NCED2%'",
    )
    .fetch_optional(&db)
    .await
    .unwrap();
    let (season, episode, show) = nc.expect("NC extra bound under its show");
    assert_eq!(
        (season, episode),
        (Some(0), 122),
        "NCED2 lands in the ED credits band"
    );
    assert_eq!(show, "Ao no Exorcist");
}
