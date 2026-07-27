//! `items.sort_title` is denormalised, and the last denormalised table
//! this project had (`merged_metadata`) spent its life quietly wrong.
//!
//! The difference is where the maintenance lives: triggers, so the value
//! is recomputed by the database on every write to anything it depends
//! on. This test exists to prove that claim rather than assert it, and it
//! deliberately mutates through RAW SQL as well as through the provider
//! API — a hand-written UPDATE is exactly how the old merge drifted.

use sqlx::SqlitePool;

/// What `sort_title` must equal at every moment, computed from scratch.
/// If this and the stored column ever disagree, browse is sorting by
/// something the user cannot see.
const TRUTH: &str = "SELECT COUNT(*) FROM items i WHERE i.sort_title IS NOT COALESCE(
        (SELECT pm.title FROM item_match im
           JOIN provider_metadata pm
             ON pm.item_id = im.item_id AND pm.provider = im.provider
          WHERE im.item_id = i.id AND NULLIF(pm.title, '') IS NOT NULL),
        i.title)";

async fn drifted(db: &SqlitePool) -> i64 {
    sqlx::query_scalar(TRUTH).fetch_one(db).await.unwrap()
}

async fn sort_title(db: &SqlitePool, id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT sort_title FROM items WHERE id = ?")
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn item(db: &SqlitePool, id: &str, title: &str) {
    sqlx::query("INSERT INTO items (id, kind, title, norm_title) VALUES (?, 'movie', ?, ?)")
        .bind(id)
        .bind(title)
        .bind(title.to_lowercase())
        .execute(db)
        .await
        .unwrap();
}

#[tokio::test]
async fn sort_title_never_drifts() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();

    // A new item sorts by its own title.
    item(&db, "i1", "12 Monkeys").await;
    assert_eq!(sort_title(&db, "i1").await.as_deref(), Some("12 Monkeys"));
    assert_eq!(drifted(&db).await, 0);

    // An answer that wins the assignment renames it for sorting.
    kahawai_hub::providers::store_answer(
        &db,
        "i1",
        "tmdb",
        "63",
        "auto",
        kahawai_hub::providers::Fields {
            title: Some("Twelve Monkeys".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(sort_title(&db, "i1").await.as_deref(), Some("Twelve Monkeys"));
    assert_eq!(drifted(&db).await, 0);

    // The assigned record's title changing must follow, INCLUDING when
    // it is changed by hand. This is the merged-metadata failure mode.
    sqlx::query("UPDATE provider_metadata SET title = 'Douze Singes' WHERE item_id = 'i1'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        sort_title(&db, "i1").await.as_deref(),
        Some("Douze Singes"),
        "a raw UPDATE must not be able to leave the sort key stale"
    );
    assert_eq!(drifted(&db).await, 0);

    // A second provider that is NOT assigned must not move the sort.
    kahawai_hub::providers::store_answer(
        &db,
        "i1",
        "tvdb",
        "999",
        "auto",
        kahawai_hub::providers::Fields {
            title: Some("SHOULD NOT SORT HERE".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(sort_title(&db, "i1").await.as_deref(), Some("Douze Singes"));
    assert_eq!(drifted(&db).await, 0);

    // Moving the assignment moves the sort key with it.
    sqlx::query("UPDATE item_match SET provider = 'tvdb', provider_id = '999' WHERE item_id = 'i1'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(sort_title(&db, "i1").await.as_deref(), Some("SHOULD NOT SORT HERE"));
    assert_eq!(drifted(&db).await, 0);

    // Deleting the assigned answer falls back to the item's own title.
    sqlx::query("DELETE FROM provider_metadata WHERE item_id = 'i1' AND provider = 'tvdb'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(drifted(&db).await, 0);

    // Unmatching entirely.
    sqlx::query("DELETE FROM item_match WHERE item_id = 'i1'").execute(&db).await.unwrap();
    assert_eq!(sort_title(&db, "i1").await.as_deref(), Some("12 Monkeys"));
    assert_eq!(drifted(&db).await, 0);

    // A rescan renaming the file has to follow too.
    sqlx::query("UPDATE items SET title = '12 Monkeys (1995)' WHERE id = 'i1'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(sort_title(&db, "i1").await.as_deref(), Some("12 Monkeys (1995)"));
    assert_eq!(drifted(&db).await, 0);

    // An answer with an empty title must not blank the sort key — an
    // item that sorts under "" disappears to the top of every list.
    kahawai_hub::providers::store_answer(
        &db,
        "i1",
        "tmdb",
        "63",
        "auto",
        kahawai_hub::providers::Fields::default(),
    )
    .await
    .unwrap();
    assert_eq!(sort_title(&db, "i1").await.as_deref(), Some("12 Monkeys (1995)"));
    assert_eq!(drifted(&db).await, 0);
}

/// The whole catalogue, after a realistic churn of matches and re-matches
/// — the shape an enrichment run produces.
#[tokio::test]
async fn a_full_enrichment_leaves_nothing_stale() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    for n in 0..200 {
        item(&db, &format!("i{n}"), &format!("File {n}")).await;
    }
    for n in 0..200 {
        for (provider, pid) in [("tmdb", n), ("tvdb", n + 1000)] {
            kahawai_hub::providers::store_answer(
                &db,
                &format!("i{n}"),
                provider,
                &pid.to_string(),
                "auto",
                kahawai_hub::providers::Fields {
                    title: Some(format!("{provider} title {n}")),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
    }
    assert_eq!(drifted(&db).await, 0, "after matching");

    // Re-match half of them elsewhere, then reject a quarter outright.
    for n in (0..200).step_by(2) {
        kahawai_hub::providers::assign_manual(
            &db,
            &format!("i{n}"),
            "tvdb",
            &(n + 1000).to_string(),
            kahawai_hub::providers::Fields {
                title: Some(format!("manual {n}")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    assert_eq!(drifted(&db).await, 0, "after manual re-matching");

    for n in (0..200).step_by(4) {
        kahawai_hub::providers::reject_matches(&db, &format!("i{n}")).await.unwrap();
    }
    assert_eq!(drifted(&db).await, 0, "after rejections");
}

/// `item_libraries` is the second denormalised table, and gets the same
/// treatment: a truth query run after every kind of write that can move
/// membership. Deriving it per query is what made browse slow; deriving
/// it wrongly is what made `merged_metadata` untrustworthy, so this
/// checks the stored answer against a fresh derivation each time.
const LIB_TRUTH: &str = "SELECT (
    SELECT COUNT(*) FROM item_libraries il
     WHERE NOT EXISTS (
       SELECT 1 FROM item_sources ls
         JOIN items ci ON ci.id = ls.item_id
         JOIN library_collections lc
           ON lc.module_id = ls.module_id AND lc.collection_id = ls.collection_id
        WHERE lc.library_id = il.library_id
          AND COALESCE(ci.parent_id, ci.id) = il.item_id)
  ) + (
    SELECT COUNT(*) FROM (
      SELECT DISTINCT lc.library_id AS lid, COALESCE(ci.parent_id, ci.id) AS iid
        FROM item_sources ls
        JOIN items ci ON ci.id = ls.item_id
        JOIN library_collections lc
          ON lc.module_id = ls.module_id AND lc.collection_id = ls.collection_id)
     WHERE NOT EXISTS (SELECT 1 FROM item_libraries il
                        WHERE il.library_id = lid AND il.item_id = iid))";

async fn lib_drift(db: &SqlitePool) -> i64 {
    sqlx::query_scalar(LIB_TRUTH).fetch_one(db).await.unwrap()
}

#[tokio::test]
async fn library_membership_never_drifts() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let q = |sql: &'static str| {
        let db = db.clone();
        async move {
            sqlx::query(sql).execute(&db).await.unwrap_or_else(|e| panic!("{sql}\n  -> {e}"))
        }
    };

    q("INSERT INTO libraries (id, name, media_type) VALUES ('lib1','A','movies')").await;
    q("INSERT INTO libraries (id, name, media_type) VALUES ('lib2','B','movies')").await;
    q("INSERT INTO satellites (module_id, module_type, name, cert_fingerprint, enrolled_at, disabled)
       VALUES ('m1','mediahost','h','',unixepoch(),0)").await;
    for c in ["c1", "c2"] {
        sqlx::query(
            "INSERT INTO collections (module_id, collection_id, media_type, roots_json, sync_version)
             VALUES ('m1', ?, 'movies', '[\"/m\"]', 1)",
        )
        .bind(c)
        .execute(&db)
        .await
        .unwrap();
    }
    assert_eq!(lib_drift(&db).await, 0, "empty");

    // A show whose EPISODE carries the source: membership belongs to the
    // show, which is the case a naive delta gets wrong.
    item(&db, "show1", "A Show").await;
    q("INSERT INTO items (id, kind, title, norm_title, parent_id)
       VALUES ('ep1','episode','E1','e1','show1')").await;
    q("INSERT INTO files (module_id, collection_id, path_rel, size, mtime_unix,
                          head_xxh3, tail_xxh3, oshash, streams_json, subs_extracted)
       VALUES ('m1','c1','ep1.mkv',1,1,0,0,0,'{}',0)").await;
    q("INSERT INTO item_sources (item_id, module_id, collection_id, path_rel)
       VALUES ('ep1','m1','c1','ep1.mkv')").await;
    q("INSERT INTO library_collections (library_id, module_id, collection_id)
       VALUES ('lib1','m1','c1')").await;
    assert_eq!(lib_drift(&db).await, 0, "after composing a library");
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM item_libraries WHERE library_id='lib1'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(n, 1, "the SHOW is in the library, via its episode's source");

    // The same collection in a second library.
    q("INSERT INTO library_collections (library_id, module_id, collection_id)
       VALUES ('lib2','m1','c1')").await;
    assert_eq!(lib_drift(&db).await, 0, "two libraries over one collection");

    // A second source in another collection of lib1 — removing one must
    // not evict the item while the other still holds it.
    q("INSERT INTO library_collections (library_id, module_id, collection_id)
       VALUES ('lib1','m1','c2')").await;
    q("INSERT INTO files (module_id, collection_id, path_rel, size, mtime_unix,
                          head_xxh3, tail_xxh3, oshash, streams_json, subs_extracted)
       VALUES ('m1','c2','ep1-copy.mkv',1,1,0,0,0,'{}',0)").await;
    q("INSERT INTO item_sources (item_id, module_id, collection_id, path_rel)
       VALUES ('ep1','m1','c2','ep1-copy.mkv')").await;
    assert_eq!(lib_drift(&db).await, 0, "two sources");
    q("DELETE FROM item_sources WHERE collection_id='c1'").await;
    assert_eq!(lib_drift(&db).await, 0, "one source removed");
    let still: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM item_libraries WHERE library_id='lib1' AND item_id='show1'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(still, 1, "the other collection still puts it in lib1");

    // Removing a collection from a library.
    q("DELETE FROM library_collections WHERE library_id='lib1' AND collection_id='c2'").await;
    assert_eq!(lib_drift(&db).await, 0, "collection removed from a library");

    // Re-parenting an episode moves membership to the new show.
    q("INSERT INTO library_collections (library_id, module_id, collection_id)
       VALUES ('lib1','m1','c2')").await;
    item(&db, "show2", "Another Show").await;
    q("UPDATE items SET parent_id='show2' WHERE id='ep1'").await;
    assert_eq!(lib_drift(&db).await, 0, "after re-parenting");

    // And removing the item — sources first, as the app does, since
    // item_sources has no cascade — takes its membership with it.
    q("DELETE FROM item_sources WHERE item_id='ep1'").await;
    q("DELETE FROM items WHERE id='ep1'").await;
    assert_eq!(lib_drift(&db).await, 0, "after deleting the episode");
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM item_libraries")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(left, 0, "nothing should still claim membership");
}

/// The gap an AFTER UPDATE trigger keyed on NEW leaves: a row that MOVES
/// to a different item. Nothing in the hub does this — `item_id` is the
/// primary key of `item_match` and part of `provider_metadata`'s — but
/// "nothing does it today" is the reasoning that let `merged_metadata`
/// drift, so it is checked rather than assumed.
#[tokio::test]
async fn moving_a_row_between_items_leaves_nothing_stale() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "a", "Item A").await;
    item(&db, "b", "Item B").await;

    kahawai_hub::providers::store_answer(
        &db,
        "a",
        "tmdb",
        "1",
        "auto",
        kahawai_hub::providers::Fields { title: Some("From TMDB".into()), ..Default::default() },
    )
    .await
    .unwrap();
    assert_eq!(sort_title(&db, "a").await.as_deref(), Some("From TMDB"));

    // Re-point the ANSWER at the other item.
    sqlx::query("UPDATE provider_metadata SET item_id = 'b' WHERE item_id = 'a'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(drifted(&db).await, 0, "answer moved between items");

    // And the ASSIGNMENT.
    sqlx::query("UPDATE item_match SET item_id = 'b' WHERE item_id = 'a'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(drifted(&db).await, 0, "assignment moved between items");

    // The same shape on the membership side: a source moving collection,
    // and a collection moving library.
    sqlx::query(
        "INSERT INTO satellites (module_id, module_type, name, cert_fingerprint, enrolled_at, disabled)
         VALUES ('m1','mediahost','h','',unixepoch(),0)",
    )
    .execute(&db)
    .await
    .unwrap();
    for c in ["c1", "c2"] {
        sqlx::query(
            "INSERT INTO collections (module_id, collection_id, media_type, roots_json, sync_version)
             VALUES ('m1', ?, 'movies', '[\"/m\"]', 1)",
        )
        .bind(c)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO files (module_id, collection_id, path_rel, size, mtime_unix,
                                head_xxh3, tail_xxh3, oshash, streams_json, subs_extracted)
             VALUES ('m1', ?, 'f.mkv', 1, 1, 0, 0, 0, '{}', 0)",
        )
        .bind(c)
        .execute(&db)
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO libraries (id, name, media_type) VALUES ('L1','L','movies')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO libraries (id, name, media_type) VALUES ('L2','M','movies')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO library_collections (library_id, module_id, collection_id)
                 VALUES ('L1','m1','c1')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO item_sources (item_id, module_id, collection_id, path_rel)
                 VALUES ('a','m1','c1','f.mkv')")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(lib_drift(&db).await, 0, "seeded");

    sqlx::query("UPDATE item_sources SET collection_id='c2' WHERE item_id='a'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(lib_drift(&db).await, 0, "source moved collection");

    sqlx::query("UPDATE library_collections SET library_id='L2' WHERE library_id='L1'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(lib_drift(&db).await, 0, "collection moved library");
}

/// Reordering providers re-decides which record an item IS, and the new
/// winner usually carries a different title. That path writes item_match
/// rather than any title column, so it is worth proving the sort key
/// follows a decision made somewhere else entirely.
#[tokio::test]
async fn reordering_providers_moves_the_sort_key() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1", "some.file.2019").await;

    // Two equally strong answers with different titles. Default order
    // for movies is tmdb before tvdb, so tmdb wins first.
    for (provider, pid, title) in
        [("tmdb", "1", "The TMDB Title"), ("tvdb", "2", "A TVDB Title")]
    {
        kahawai_hub::providers::store_answer(
            &db,
            "i1",
            provider,
            pid,
            "auto",
            kahawai_hub::providers::Fields {
                title: Some(title.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    assert_eq!(sort_title(&db, "i1").await.as_deref(), Some("The TMDB Title"));
    assert_eq!(drifted(&db).await, 0);

    // Put tvdb first. Nothing writes a title here — only provider_ranks
    // and, through the re-pick, item_match.
    kahawai_hub::providers::set_chain(&db, "movies", &["tvdb".into(), "tmdb".into()])
        .await
        .unwrap();
    assert_eq!(
        sort_title(&db, "i1").await.as_deref(),
        Some("A TVDB Title"),
        "the sort key must follow the reorder, not the write that caused it"
    );
    assert_eq!(drifted(&db).await, 0);

    // And back.
    kahawai_hub::providers::set_chain(&db, "movies", &["tmdb".into(), "tvdb".into()])
        .await
        .unwrap();
    assert_eq!(sort_title(&db, "i1").await.as_deref(), Some("The TMDB Title"));
    assert_eq!(drifted(&db).await, 0);
}

/// The one divergence, pinned so it stays known: `sort_title` follows the
/// ASSIGNED record, while the resolved view side-fills a title from
/// another provider when the assigned record has none. Such an item is
/// displayed under the side-filled title and sorted under its filename.
///
/// It needs a match that carries no title at all, which providers do not
/// normally return — but if this ever stops being rare, the fix is to
/// resolve sort_title the way the view does, and this test is where that
/// would be noticed.
#[tokio::test]
async fn a_titleless_match_sorts_by_filename_while_showing_a_borrowed_title() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1", "zzz.filename").await;

    // The assigned record identifies the item but names nothing.
    kahawai_hub::providers::store_answer(
        &db,
        "i1",
        "tmdb",
        "1",
        "auto",
        kahawai_hub::providers::Fields::default(),
    )
    .await
    .unwrap();
    // A lower-ranked provider does have a title, which the view borrows.
    kahawai_hub::providers::store_answer(
        &db,
        "i1",
        "tvdb",
        "2",
        "auto",
        kahawai_hub::providers::Fields {
            title: Some("Borrowed Title".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let shown: Option<String> =
        sqlx::query_scalar("SELECT title FROM resolved_metadata WHERE item_id = 'i1'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(shown.as_deref(), Some("Borrowed Title"), "the view side-fills");
    assert_eq!(
        sort_title(&db, "i1").await.as_deref(),
        Some("zzz.filename"),
        "the sort key falls back to the filename, and this is the known divergence"
    );
    // Still not DRIFT: the stored value is what its definition says.
    assert_eq!(drifted(&db).await, 0);
}
