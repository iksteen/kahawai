//! `GET /api/v1/up-next` — the next episode of each series you are in.
//!
//! Four rules decide the row and each has its own way of quietly not
//! holding: the episode offered is the one after the last you FINISHED
//! (not the first unwatched, which is a different answer whenever you
//! skipped one), a series you are part-way through an episode of belongs
//! to continue watching instead, a series goes quiet a month after you
//! last watched it, and a new episode brings one back however long ago
//! that was.
//!
//! Ages are seeded explicitly rather than played through the API:
//! `unixepoch()` has one-second resolution, so marks made in sequence
//! would tie and the ordering assertion would be testing the tiebreaker.
//! Episode ids are real ULIDs minted at a chosen moment, because an item
//! id IS its date added — that is what `sort=-added` orders by, and what
//! the "arrived lately" half of the rule reads.

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

const DAY: u64 = 24 * 60 * 60;

async fn harness() -> (axum::Router, Arc<kahawai_hub::auth::Auth>, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(kahawai_hub::registry::Registry::new(
        db.clone(),
        Default::default(),
    ));
    let auth = Arc::new(
        kahawai_hub::auth::Auth::new(db.clone(), dir.path())
            .await
            .unwrap(),
    );
    let sessions = Arc::new(kahawai_hub::sessions::Sessions::new(
        tempfile::tempdir().unwrap().keep(),
    ));
    let ca = Arc::new(
        kahawai_hub::pki::HubCa::load_or_create(tempfile::tempdir().unwrap().keep().as_path())
            .unwrap(),
    );
    let enrollments = Arc::new(kahawai_hub::enrollment_service::EnrollmentService::new(
        ca,
        registry.clone(),
        std::time::Duration::from_secs(900),
        90,
    ));
    let enricher = Arc::new(kahawai_hub::enrich::Enricher::new(dir.path().to_path_buf()));
    let api = kahawai_hub::api::router(
        registry,
        auth.clone(),
        sessions,
        enrollments,
        Arc::new(kahawai_hub::subtitles::Subtitles::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        Arc::new(kahawai_hub::artwork::Artwork::new(
            tempfile::tempdir().unwrap().keep(),
            enricher.clone(),
        )),
        enricher,
        Arc::new(kahawai_hub::segments::Detector::new()),
        kahawai_hub::api::NetOptions::default(),
    );
    std::mem::forget(dir);
    (api, auth, db)
}

async fn get(api: &axum::Router, token: &str, uri: &str) -> serde_json::Value {
    let resp = api
        .clone()
        .oneshot(
            Request::get(uri)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{uri} -> {}", resp.status());
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn titles(page: &serde_json::Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap().to_string())
        .collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// An item id that was minted `days_ago` days ago. The random half is a
/// counter rather than random, so two ids minted in the same millisecond
/// are still distinct and still order the same way on every run.
fn id_added(days_ago: u64) -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    ulid::Ulid::from_parts(now_ms() - days_ago * DAY * 1000, seq as u128).to_string()
}

async fn collection(db: &sqlx::SqlitePool, library: Option<&str>) -> String {
    sqlx::query(
        "INSERT OR IGNORE INTO satellites(module_id,module_type,name,cert_fingerprint)
         VALUES('fixture','mediahost','fixture','fp')",
    )
    .execute(db)
    .await
    .unwrap();
    let collection = library.unwrap_or("unattached").to_string();
    sqlx::query(
        "INSERT OR IGNORE INTO collections(module_id,collection_id,media_type)
         VALUES('fixture',?,'series')",
    )
    .bind(&collection)
    .execute(db)
    .await
    .unwrap();
    if let Some(lib) = library {
        sqlx::query(
            "INSERT OR IGNORE INTO library_collections(library_id,module_id,collection_id)
             VALUES(?,'fixture',?)",
        )
        .bind(lib)
        .bind(&collection)
        .execute(db)
        .await
        .unwrap();
    }
    collection
}

async fn seed_show(db: &sqlx::SqlitePool, id: &str, title: &str, library: Option<&str>) {
    let collection = collection(db, library).await;
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id)
         VALUES(?,'show',?,?,?,'fixture',?)",
    )
    .bind(id)
    .bind(title)
    .bind(title.to_lowercase())
    .bind(title.to_lowercase())
    .bind(&collection)
    .execute(db)
    .await
    .unwrap();
}

/// One episode of `show`, its id dated `added_days_ago`, optionally with
/// this account's watch state: `(position_ms, played, watched_days_ago)`.
/// Episodes use a twenty-minute runtime so unfinished rows exercise the
/// Continue Watching threshold as well as the boolean mark.
async fn seed_episode(
    db: &sqlx::SqlitePool,
    show: &str,
    title: &str,
    season: i64,
    episode: i64,
    added_days_ago: u64,
    watch: Option<(i64, i64, u64)>,
) -> String {
    let id = id_added(added_days_ago);
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id,
                           parent_id,season,episode)
         SELECT ?,'episode',?,?,?,module_id,collection_id,id,?,? FROM items WHERE id = ?",
    )
    .bind(&id)
    .bind(title)
    .bind(title.to_lowercase())
    .bind(title.to_lowercase())
    .bind(season)
    .bind(episode)
    .bind(show)
    .execute(db)
    .await
    .unwrap();
    if let Some((position_ms, played, days_ago)) = watch {
        sqlx::query(
            "INSERT INTO watch_state
                    (user_id, item_id, position_ms, duration_ms, played, updated_at)
             SELECT id, ?, ?, 1200000, ?, unixepoch() - ?
               FROM users WHERE username = 'owner'",
        )
        .bind(&id)
        .bind(position_ms)
        .bind(played)
        .bind((days_ago * DAY) as i64)
        .execute(db)
        .await
        .unwrap();
    }
    id
}

async fn owner(auth: &kahawai_hub::auth::Auth) -> String {
    auth.complete_setup("owner", "hunter22222hunter")
        .await
        .unwrap();
    auth.login("owner", "hunter22222hunter")
        .await
        .unwrap()
        .access_token
}

/// The whole rule, one series per clause.
#[tokio::test]
async fn up_next_is_the_episode_after_the_last_finished_one_of_a_current_series() {
    let (api, auth, db) = harness().await;
    let token = owner(&auth).await;

    // Watched lately: two finished, so the third is what is next.
    seed_show(&db, "s_recent", "Recent", None).await;
    seed_episode(&db, "s_recent", "Recent E01", 1, 1, 200, Some((0, 1, 10))).await;
    seed_episode(&db, "s_recent", "Recent E02", 1, 2, 200, Some((0, 1, 5))).await;
    seed_episode(&db, "s_recent", "Recent E03", 1, 3, 200, None).await;

    // Barely opened E02. It has not reached either meaningful-progress
    // threshold, so it stays in Up Next rather than displacing itself into
    // Continue Watching. The offered row retains the small resume position.
    seed_show(&db, "s_paused", "Paused", None).await;
    seed_episode(&db, "s_paused", "Paused E01", 1, 1, 200, Some((0, 1, 6))).await;
    seed_episode(&db, "s_paused", "Paused E02", 1, 2, 200, Some((500, 0, 2))).await;
    seed_episode(&db, "s_paused", "Paused E03", 1, 3, 200, None).await;

    // Exactly at the absolute threshold is meaningfully part-way through E02.
    // This series belongs exclusively to Continue Watching, so neither its
    // resumable E02 nor the unwatched E03 may leak into Up Next.
    seed_show(&db, "s_resumable", "Resumable", None).await;
    seed_episode(
        &db,
        "s_resumable",
        "Resumable E01",
        1,
        1,
        200,
        Some((0, 1, 4)),
    )
    .await;
    seed_episode(
        &db,
        "s_resumable",
        "Resumable E02",
        1,
        2,
        200,
        Some((60_000, 0, 2)),
    )
    .await;
    seed_episode(&db, "s_resumable", "Resumable E03", 1, 3, 200, None).await;

    // Nothing left to offer.
    seed_show(&db, "s_done", "Done", None).await;
    seed_episode(&db, "s_done", "Done E01", 1, 1, 200, Some((0, 1, 4))).await;
    seed_episode(&db, "s_done", "Done E02", 1, 2, 200, Some((0, 1, 4))).await;

    // Watched two months ago and nothing new since: gone quiet.
    seed_show(&db, "s_stale", "Stale", None).await;
    seed_episode(&db, "s_stale", "Stale E01", 1, 1, 200, Some((0, 1, 60))).await;
    seed_episode(&db, "s_stale", "Stale E02", 1, 2, 200, None).await;

    // Watched just as long ago — but a new episode landed last week, and
    // that alone brings the series back.
    seed_show(&db, "s_returned", "Returned", None).await;
    seed_episode(
        &db,
        "s_returned",
        "Returned E01",
        1,
        1,
        200,
        Some((0, 1, 60)),
    )
    .await;
    seed_episode(&db, "s_returned", "Returned E02", 1, 2, 3, None).await;

    // Added last week, never touched. A series nobody has started is not
    // something to be part-way through, and the recently-added shelves
    // already have it.
    seed_show(&db, "s_fresh", "Fresh", None).await;
    seed_episode(&db, "s_fresh", "Fresh E01", 1, 1, 3, None).await;

    let page = get(&api, &token, "/api/v1/up-next").await;
    assert_eq!(
        titles(&page),
        vec!["Recent E03", "Paused E02", "Returned E02"],
        "one episode per eligible series, the series watched most recently first"
    );
    assert_eq!(page["total"], 3, "the total counts the rows it returns");

    let row = &page["items"][0];
    assert_eq!(row["season"], 1);
    assert_eq!(row["episode"], 3);
    assert_eq!(
        row["parent_id"], "s_recent",
        "the row names its series, which is what the card is labelled with"
    );
    assert!(
        row["resume_position_ms"].is_null() && row["played"] == false,
        "an episode you have never opened has no position and is not played"
    );
    let paused = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["title"] == "Paused E02")
        .unwrap();
    assert_eq!(
        paused["resume_position_ms"], 500,
        "a barely opened episode can itself be the Up Next entry"
    );
}

/// What "after" means when the numbering is not a straight line.
#[tokio::test]
async fn up_next_follows_the_last_finished_episode_and_not_the_first_unwatched_one() {
    let (api, auth, db) = harness().await;
    let token = owner(&auth).await;

    // E01 skipped, E02 finished. The answer is E03: what follows what you
    // watched, not the earliest thing you have not.
    seed_show(&db, "s_gap", "Gap", None).await;
    seed_episode(&db, "s_gap", "Gap S01E01", 1, 1, 200, None).await;
    seed_episode(&db, "s_gap", "Gap S01E02", 1, 2, 200, Some((0, 1, 2))).await;
    seed_episode(&db, "s_gap", "Gap S01E03", 1, 3, 200, None).await;
    seed_episode(&db, "s_gap", "Gap S02E01", 2, 1, 200, None).await;

    // Sequence order is not viewing order. E04 was finished first and E02
    // most recently, so "after the last you finished" is E03. Anchoring at
    // the highest finished number instead would incorrectly offer E05.
    seed_show(&db, "s_rewatch", "Rewatch", None).await;
    seed_episode(&db, "s_rewatch", "Rewatch E01", 1, 1, 200, None).await;
    seed_episode(&db, "s_rewatch", "Rewatch E02", 1, 2, 200, Some((0, 1, 1))).await;
    seed_episode(&db, "s_rewatch", "Rewatch E03", 1, 3, 200, None).await;
    seed_episode(&db, "s_rewatch", "Rewatch E04", 1, 4, 200, Some((0, 1, 4))).await;
    seed_episode(&db, "s_rewatch", "Rewatch E05", 1, 5, 200, None).await;

    // Finished a season: the next season's first episode, and the
    // ordering has to be by season THEN episode to get there — S01E09
    // sorts after S02E01 on episode number alone.
    seed_show(&db, "s_season", "Season", None).await;
    seed_episode(&db, "s_season", "Season S01E09", 1, 9, 200, Some((0, 1, 1))).await;
    seed_episode(&db, "s_season", "Season S02E01", 2, 1, 200, None).await;

    // A special with no season or episode number sorts ahead of
    // everything, so finishing it offers S01E01 rather than nothing.
    seed_show(&db, "s_special", "Special", None).await;
    let special = seed_episode(&db, "s_special", "Special extra", 0, 0, 200, None).await;
    sqlx::query("UPDATE items SET season = NULL, episode = NULL WHERE id = ?")
        .bind(&special)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO watch_state (user_id, item_id, position_ms, played, updated_at)
         SELECT id, ?, 0, 1, unixepoch() - 259200 FROM users WHERE username = 'owner'",
    )
    .bind(&special)
    .execute(&db)
    .await
    .unwrap();
    seed_episode(&db, "s_special", "Special S01E01", 1, 1, 200, None).await;

    let mut got = titles(&get(&api, &token, "/api/v1/up-next").await);
    got.sort();
    assert_eq!(
        got,
        vec![
            "Gap S01E03",
            "Rewatch E03",
            "Season S02E01",
            "Special S01E01",
        ],
        "the episode after the last finished one, across a gap, a season \
         boundary and an unnumbered special"
    );
}

/// A withheld library must not leak an episode into the row, and naming
/// a library must scope it.
#[tokio::test]
async fn up_next_respects_library_grants_and_scoping() {
    let (api, auth, db) = harness().await;
    let admin = owner(&auth).await;

    for (id, name) in [("LA", "granted"), ("LB", "withheld")] {
        sqlx::query("INSERT INTO libraries (id, name, media_type) VALUES (?,?,'series')")
            .bind(id)
            .bind(name)
            .execute(&db)
            .await
            .unwrap();
    }
    seed_show(&db, "s_a", "In granted", Some("LA")).await;
    let a1 = seed_episode(&db, "s_a", "Granted E01", 1, 1, 200, None).await;
    seed_episode(&db, "s_a", "Granted E02", 1, 2, 200, None).await;
    seed_show(&db, "s_b", "In withheld", Some("LB")).await;
    let b1 = seed_episode(&db, "s_b", "Withheld E01", 1, 1, 200, None).await;
    seed_episode(&db, "s_b", "Withheld E02", 1, 2, 200, None).await;

    // The owner is an admin, so grants do not bind it. It watched the
    // first episode of both.
    for (id, days) in [(&a1, 2), (&b1, 1)] {
        sqlx::query(
            "INSERT INTO watch_state (user_id, item_id, position_ms, played, updated_at)
             SELECT id, ?, 0, 1, unixepoch() - ? FROM users WHERE username = 'owner'",
        )
        .bind(id)
        .bind(days * DAY as i64)
        .execute(&db)
        .await
        .unwrap();
    }
    let page = get(&api, &admin, "/api/v1/up-next").await;
    assert_eq!(
        titles(&page),
        vec!["Withheld E02", "Granted E02"],
        "an admin is not bound by grants"
    );
    let scoped = get(&api, &admin, "/api/v1/up-next?library=LA").await;
    assert_eq!(
        titles(&scoped),
        vec!["Granted E02"],
        "naming a library scopes the row to it"
    );
    assert_eq!(scoped["total"], 1, "and the total agrees with the rows");
    assert_eq!(
        scoped["items"][0]["library_id"], "LA",
        "a cross-library row names a library it is in"
    );

    // A restricted account holding only LA, having watched an episode in
    // BOTH — so the only thing that can keep the withheld series out is
    // the grant.
    let uid = auth
        .create_user("viewer", "hunter22222hunter", false)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET all_libraries = 0 WHERE id = ?")
        .bind(&uid)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_libraries (user_id, library_id) VALUES (?,'LA')")
        .bind(&uid)
        .execute(&db)
        .await
        .unwrap();
    for (id, days) in [(&a1, 2), (&b1, 1)] {
        sqlx::query(
            "INSERT INTO watch_state (user_id, item_id, position_ms, played, updated_at)
             VALUES (?, ?, 0, 1, unixepoch() - ?)",
        )
        .bind(&uid)
        .bind(id)
        .bind(days * DAY as i64)
        .execute(&db)
        .await
        .unwrap();
    }
    let viewer = auth
        .login("viewer", "hunter22222hunter")
        .await
        .unwrap()
        .access_token;
    let page = get(&api, &viewer, "/api/v1/up-next").await;
    assert_eq!(
        titles(&page),
        vec!["Granted E02"],
        "a withheld library's episode must not reach the row, even though \
         this account has finished the one before it"
    );
    assert_eq!(page["total"], 1, "and the total must agree with the rows");
}
