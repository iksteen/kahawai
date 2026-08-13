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
             ON pm.item_id = i.id AND pm.provider = im.provider
          WHERE im.item_id = COALESCE(i.parent_id, i.id)
            AND NULLIF(pm.title, '') IS NOT NULL),
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
    sqlx::query(
        "INSERT OR IGNORE INTO satellites(module_id,module_type,name,cert_fingerprint)
                 VALUES('fixture','mediahost','fixture','fp')",
    )
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT OR IGNORE INTO collections(module_id,collection_id,media_type)
                 VALUES('fixture','default','movies')",
    )
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,module_id,collection_id)
                 VALUES(?,'movie',?,?,'fixture','default')",
    )
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
    assert_eq!(
        sort_title(&db, "i1").await.as_deref(),
        Some("Twelve Monkeys")
    );
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
    sqlx::query(
        "UPDATE item_match SET provider = 'tvdb', provider_id = '999' WHERE item_id = 'i1'",
    )
    .execute(&db)
    .await
    .unwrap();
    assert_eq!(
        sort_title(&db, "i1").await.as_deref(),
        Some("SHOULD NOT SORT HERE")
    );
    assert_eq!(drifted(&db).await, 0);

    // Deleting the assigned answer falls back to the item's own title.
    sqlx::query("DELETE FROM provider_metadata WHERE item_id = 'i1' AND provider = 'tvdb'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(drifted(&db).await, 0);

    // Unmatching entirely.
    sqlx::query("DELETE FROM item_match WHERE item_id = 'i1'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(sort_title(&db, "i1").await.as_deref(), Some("12 Monkeys"));
    assert_eq!(drifted(&db).await, 0);

    // A rescan renaming the file has to follow too.
    sqlx::query("UPDATE items SET title = '12 Monkeys (1995)' WHERE id = 'i1'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        sort_title(&db, "i1").await.as_deref(),
        Some("12 Monkeys (1995)")
    );
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
    assert_eq!(
        sort_title(&db, "i1").await.as_deref(),
        Some("12 Monkeys (1995)")
    );
    assert_eq!(drifted(&db).await, 0);
}

/// An episode's sort_title is the title its SHOW's assigned provider
/// gave it (0041) — that is what makes episode titles searchable, so it
/// has to follow the show's assignment around, not the episode's own
/// filename.
#[tokio::test]
async fn an_episodes_sort_title_follows_the_shows_assignment() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "show1", "A Show").await;
    sqlx::query("UPDATE items SET kind='show' WHERE id='show1'")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id)
         SELECT 'ep1','episode','S01E01','s01e01','show1',1,1,module_id,collection_id
           FROM items WHERE id='show1'",
    )
    .execute(&db)
    .await
    .unwrap();
    // No assignment anywhere: filename title stands.
    assert_eq!(sort_title(&db, "ep1").await.as_deref(), Some("S01E01"));
    assert_eq!(drifted(&db).await, 0);

    // The show matches tvdb, and the projection writes the episode's own
    // per-provider rows — the tvdb one should now name the episode.
    for (provider, pid, ep_title) in [
        ("tvdb", "414734", "The Real Title"),
        ("tmdb", "63", "Другое название"),
    ] {
        kahawai_hub::providers::store_answer(
            &db,
            "show1",
            provider,
            pid,
            "auto",
            kahawai_hub::providers::Fields {
                title: Some("A Show".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence, updated_at)
             VALUES ('ep1', ?, ?, ?, 'auto', unixepoch())",
        )
        .bind(provider)
        .bind(format!("{pid}-e1"))
        .bind(ep_title)
        .execute(&db)
        .await
        .unwrap();
    }
    // Movies chain ranks tmdb first, so tmdb owns the show...
    assert_eq!(
        sort_title(&db, "ep1").await.as_deref(),
        Some("Другое название")
    );
    assert_eq!(drifted(&db).await, 0);

    // ...and pinning the show to tvdb flips every episode title with it.
    sqlx::query(
        "INSERT INTO manual_match (item_id, provider, provider_id, pinned_at)
         VALUES ('show1','tvdb','414734', unixepoch())",
    )
    .execute(&db)
    .await
    .unwrap();
    assert_eq!(
        sort_title(&db, "ep1").await.as_deref(),
        Some("The Real Title"),
        "re-assigning the show must re-derive its children"
    );
    assert_eq!(drifted(&db).await, 0);

    // The projected title being corrected follows too, raw SQL included.
    sqlx::query(
        "UPDATE provider_metadata SET title='Corrected' WHERE item_id='ep1' AND provider='tvdb'",
    )
    .execute(&db)
    .await
    .unwrap();
    assert_eq!(sort_title(&db, "ep1").await.as_deref(), Some("Corrected"));
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
        kahawai_hub::providers::reject_matches(&db, &format!("i{n}"))
            .await
            .unwrap();
    }
    assert_eq!(drifted(&db).await, 0, "after rejections");
}

/// Libraries compose collections directly. Reusing one collection in two
/// libraries exposes the same item id; changing collection composition changes
/// visibility without cloning or synchronising catalogue rows.
#[tokio::test]
async fn libraries_compose_collection_items_directly() {
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO libraries(id,name,media_type) VALUES
                 ('l1','One','movies'),('l2','Two','movies')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO satellites(module_id,module_type,name,cert_fingerprint)
                 VALUES('m','mediahost','m','fp')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO collections(module_id,collection_id,media_type) VALUES
                 ('m','c1','movies'),('m','c2','movies')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO library_collections(library_id,module_id,collection_id) VALUES
                 ('l1','m','c1'),('l2','m','c1')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,module_id,collection_id)
                 VALUES('i','movie','Film','film','m','c1')",
    )
    .execute(&db)
    .await
    .unwrap();

    let visible: Vec<(String, String)> = sqlx::query_as(
        "SELECT lc.library_id,i.id FROM items i JOIN library_collections lc
           ON (lc.module_id,lc.collection_id)=(i.module_id,i.collection_id)
          WHERE i.id='i' ORDER BY lc.library_id",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(
        visible,
        vec![("l1".into(), "i".into()), ("l2".into(), "i".into())]
    );

    sqlx::query("UPDATE items SET collection_id='c2' WHERE id='i'")
        .execute(&db)
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM items i JOIN library_collections lc
           ON (lc.module_id,lc.collection_id)=(i.module_id,i.collection_id)
          WHERE i.id='i'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        count, 0,
        "an uncomposed collection is not visible through either library"
    );
}

/// Child items carry the same collection as their parent; browse excludes
/// children explicitly rather than relying on a projected membership table.
#[tokio::test]
async fn library_browse_counts_only_top_level_collection_items() {
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    sqlx::query("INSERT INTO libraries(id,name,media_type) VALUES('l','Series','series')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO satellites(module_id,module_type,name,cert_fingerprint)
                 VALUES('m','mediahost','m','fp')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO collections(module_id,collection_id,media_type)
                 VALUES('m','c','series')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO library_collections(library_id,module_id,collection_id)
                 VALUES('l','m','c')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,module_id,collection_id) VALUES
                 ('show','show','Show','show','m','c'),
                 ('film','movie','Film','film','m','c')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id)
                 VALUES('ep','episode','Episode','episode','show',1,1,'m','c')")
        .execute(&db).await.unwrap();
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT i.id FROM items i JOIN library_collections lc
           ON (lc.module_id,lc.collection_id)=(i.module_id,i.collection_id)
          WHERE lc.library_id='l' AND i.parent_id IS NULL ORDER BY i.id",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(ids, vec!["film".to_string(), "show".to_string()]);
}

/// Moving durable provider rows between items must still leave sort-title
/// derivation exact; catalogue visibility no longer has a copied row to drift.
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
        kahawai_hub::providers::Fields {
            title: Some("From TMDB".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE provider_metadata SET item_id='b' WHERE item_id='a'")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE item_match SET item_id='b' WHERE item_id='a'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(drifted(&db).await, 0);
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
    for (provider, pid, title) in [
        ("tmdb", "1", "The TMDB Title"),
        ("tvdb", "2", "A TVDB Title"),
    ] {
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
    assert_eq!(
        sort_title(&db, "i1").await.as_deref(),
        Some("The TMDB Title")
    );
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
    assert_eq!(
        sort_title(&db, "i1").await.as_deref(),
        Some("The TMDB Title")
    );
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
    assert_eq!(
        shown.as_deref(),
        Some("Borrowed Title"),
        "the view side-fills"
    );
    assert_eq!(
        sort_title(&db, "i1").await.as_deref(),
        Some("zzz.filename"),
        "the sort key falls back to the filename, and this is the known divergence"
    );
    // Still not DRIFT: the stored value is what its definition says.
    assert_eq!(drifted(&db).await, 0);
}
