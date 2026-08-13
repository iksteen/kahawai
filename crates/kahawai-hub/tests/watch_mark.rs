//! Marking an item watched without playing it (HUB-10).
//!
//! `POST /playback/sessions/{id}/progress` was the only writer of
//! `watch_state`, and it needs a live session — so nothing could tick off
//! something watched elsewhere, or undo a mistaken tick.
//!
//! The assertions read the table rather than a response body: the response
//! is what the handler believes, and the point of the endpoint is what
//! survives in the database.
//!
//! Declaration-only, so it keeps its own light harness instead of
//! `common::Harness` (a real mediahost over real mTLS) — nothing here
//! plays bytes.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

async fn harness() -> (axum::Router, String, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    sqlx::query(
        "INSERT INTO satellites(module_id,module_type,name,cert_fingerprint)
                 VALUES('fixture','mediahost','fixture','fp')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO collections(module_id,collection_id,media_type)
                 VALUES('fixture','default','movies')",
    )
    .execute(&db)
    .await
    .unwrap();
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
        kahawai_hub::api::NetOptions::default(),
    );
    let token = auth
        .complete_setup(&auth.setup_token().unwrap(), "marker", "hunter22222hunter")
        .await
        .unwrap()
        .access_token;
    std::mem::forget(dir);
    (api, token, db)
}

/// `PUT /api/v1/items/{id}/watched` → (status, body). `items` names a
/// batch; None marks the addressed item alone.
async fn mark_many(
    api: &axum::Router,
    token: &str,
    id: &str,
    played: bool,
    items: Option<&[&str]>,
) -> (StatusCode, serde_json::Value) {
    let resp = api
        .clone()
        .oneshot(
            Request::put(format!("/api/v1/items/{id}/watched"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    match items {
                        Some(list) => serde_json::json!({ "played": played, "items": list }),
                        None => serde_json::json!({ "played": played }),
                    }
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let b = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&b).unwrap_or(serde_json::Value::Null),
    )
}

async fn mark(
    api: &axum::Router,
    token: &str,
    id: &str,
    played: bool,
) -> (StatusCode, serde_json::Value) {
    mark_many(api, token, id, played, None).await
}

/// The one row a single mark reports on.
fn only(body: &serde_json::Value) -> &serde_json::Value {
    let rows = body["updated"].as_array().expect("an updated list");
    assert_eq!(rows.len(), 1, "one item marked, one row reported");
    &rows[0]
}

/// The row as the interface reads it: resume bar, tick, and "seen ×N".
async fn state(db: &sqlx::SqlitePool, id: &str) -> (i64, Option<i64>, i64, i64) {
    sqlx::query_as(
        "SELECT position_ms, duration_ms, played, play_count
           FROM watch_state WHERE item_id = ?",
    )
    .bind(id)
    .fetch_one(db)
    .await
    .unwrap()
}

#[tokio::test]
async fn marking_watched_clears_resume_and_never_loses_the_count() {
    let (api, token, db) = harness().await;
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,module_id,collection_id)
         VALUES('i1','movie','Heat','heat','fixture','default')",
    )
    .execute(&db)
    .await
    .unwrap();

    // Half-watched, the way progress would have left it.
    sqlx::query(
        "INSERT INTO watch_state (user_id, item_id, position_ms, duration_ms, played, play_count)
         SELECT id, 'i1', 50000, 100000, 0, 0 FROM users",
    )
    .execute(&db)
    .await
    .unwrap();

    let (status, body) = mark(&api, &token, "i1", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(only(&body)["played"], true);
    assert_eq!(only(&body)["play_count"], 1);
    let (pos, dur, played, count) = state(&db, "i1").await;
    assert_eq!(pos, 0, "a watched item must not also be 50 s in");
    assert_eq!(
        dur,
        Some(100_000),
        "the known duration survives a mark: this endpoint did not measure it, \
         so it has nothing better to say"
    );
    assert_eq!((played, count), (1, 1));

    // Idempotent: marking watched twice is one viewing, not two. The
    // `AND NOT played` guard in the upsert is what says so.
    let (_, body) = mark(&api, &token, "i1", true).await;
    assert_eq!(only(&body)["play_count"], 1);

    // Unmarking shows it as unwatched. It does not rewrite history: the
    // count is what you have seen, not what the tick currently says.
    let (_, body) = mark(&api, &token, "i1", false).await;
    assert_eq!(only(&body)["played"], false);
    assert_eq!(only(&body)["play_count"], 1);
    let (_, _, played, count) = state(&db, "i1").await;
    assert_eq!((played, count), (0, 1));

    // ...and marking it again counts the second viewing.
    let (_, body) = mark(&api, &token, "i1", true).await;
    assert_eq!(only(&body)["play_count"], 2);
}

#[tokio::test]
async fn an_unknown_item_is_404_not_a_foreign_key_500() {
    let (api, token, db) = harness().await;
    // The grant gate passes any id for an admin, so without the handler's
    // own existence check this reaches the item_id foreign key.
    let (status, _) = mark(&api, &token, "01NOPE", true).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM watch_state")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(rows, 0, "a refused mark writes nothing");
}

#[tokio::test]
async fn a_whole_season_is_one_call_and_cannot_reach_outside_the_show() {
    let (api, token, db) = harness().await;
    // Two shows, so the boundary has something to keep out.
    for (id, kind, parent) in [
        ("show", "show", None),
        ("s1e1", "episode", Some("show")),
        ("s1e2", "episode", Some("show")),
        ("s2e1", "episode", Some("show")),
        ("other", "show", None),
        ("oth1", "episode", Some("other")),
    ] {
        sqlx::query(
            "INSERT INTO items(id,kind,title,norm_title,parent_id,module_id,collection_id)
             VALUES(?,?,?,?,?,'fixture','default')",
        )
        .bind(id)
        .bind(kind)
        .bind(id)
        .bind(id)
        .bind(parent)
        .execute(&db)
        .await
        .unwrap();
    }
    // One of them is already marked, so the climb-only guard has a case.
    let (_, body) = mark(&api, &token, "s1e1", true).await;
    assert_eq!(only(&body)["play_count"], 1);

    // A season: the client decides which episodes are in it, because the
    // season a viewer sees can be a projection of absolute numbering.
    let (status, body) =
        mark_many(&api, &token, "show", true, Some(&["s1e1", "s1e2", "oth1"])).await;
    assert_eq!(status, StatusCode::OK);
    let rows = body["updated"].as_array().unwrap();
    let marked: Vec<&str> = rows
        .iter()
        .map(|r| r["item_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        marked.len(),
        2,
        "an episode of another show is not this show's to mark: {marked:?}"
    );
    assert!(marked.contains(&"s1e1") && marked.contains(&"s1e2"));

    let (_, _, played, count) = state(&db, "s1e1").await;
    assert_eq!(
        (played, count),
        (1, 1),
        "already watched: marked again, counted once"
    );
    let (_, _, played, count) = state(&db, "s1e2").await;
    assert_eq!((played, count), (1, 1));
    // The other show's episode was silently skipped, not marked.
    let leaked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM watch_state WHERE item_id = 'oth1'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(leaked, 0, "a batch cannot reach outside the item it is on");
    // And the season it did not name is untouched.
    let untouched: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM watch_state WHERE item_id = 's2e1'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(untouched, 0);
}

#[tokio::test]
async fn a_batch_that_matches_nothing_is_404_and_writes_nothing() {
    let (api, token, db) = harness().await;
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,module_id,collection_id)
                 VALUES('m','movie','M','m','fixture','default')",
    )
    .execute(&db)
    .await
    .unwrap();
    let (status, _) = mark_many(&api, &token, "m", true, Some(&["nope", "also-nope"])).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM watch_state")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}
