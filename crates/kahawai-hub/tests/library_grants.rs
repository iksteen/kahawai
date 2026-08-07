//! Per-library access grants (HUB-10).
//!
//! Two things have to hold at once and they pull in opposite directions:
//! a restricted account must not reach past its grants through ANY route
//! that takes an item id, and an unrestricted one must behave exactly as
//! it did before grants existed. So every case here is asserted for both
//! kinds of account — a denial that is really "this hub returns 404 now"
//! would pass half a test file.
//!
//! The catalogue: `L1` holds a movie, `L2` holds a show with one episode,
//! and one movie sits in a collection attached to no library at all.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Hub {
    api: axum::Router,
    db: sqlx::SqlitePool,
    /// The setup admin: bypasses grants whatever its rows say.
    boss: String,
    /// all_libraries = 0, granted L1 only.
    kid: String,
    /// all_libraries = 1 (the default a migrated account gets).
    guest: String,
    kid_id: String,
}

async fn call(
    api: &axum::Router,
    token: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    let req = match body {
        Some(v) => req
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = api.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn get(api: &axum::Router, token: &str, uri: &str) -> (StatusCode, Value) {
    let (status, body) = call(api, token, "GET", uri, None).await;
    (
        status,
        serde_json::from_str(&body).unwrap_or(Value::String(body)),
    )
}

/// Titles on a browse page, in the order served.
fn titles(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap().to_string())
        .collect()
}

fn library_names(v: &Value) -> Vec<String> {
    v["libraries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["name"].as_str().unwrap().to_string())
        .collect()
}

async fn harness() -> Hub {
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
        kahawai_hub::api::NetOptions::default(),
    );
    let boss = auth
        .complete_setup(&auth.setup_token().unwrap(), "boss", "hunter22222hunter")
        .await
        .unwrap()
        .access_token;

    sqlx::query(
        "INSERT INTO libraries (id, name, media_type)
         VALUES ('L1','Films','movies'), ('L2','Anime','shows')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO satellites (module_id, module_type, name, cert_fingerprint, enrolled_at, disabled)
         VALUES ('m','mediahost','m','',unixepoch(),0)",
    )
    .execute(&db)
    .await
    .unwrap();
    for c in ["c1", "c2", "c3"] {
        sqlx::query(
            "INSERT INTO collections (module_id, collection_id, media_type, roots_json, sync_version)
             VALUES ('m', ?, 'movies', '[\"/m\"]', 1)",
        )
        .bind(c)
        .execute(&db)
        .await
        .unwrap();
    }
    // c3 is deliberately attached to nothing: an item no grant can reach.
    sqlx::query(
        "INSERT INTO library_collections (library_id, module_id, collection_id)
         VALUES ('L1','m','c1'), ('L2','m','c2')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO items (id, kind, title, norm_title, year)
         VALUES ('m1','movie','Test Alpha','test alpha',2020),
                ('s1','show','Test Bravo','test bravo',2021),
                ('g1','movie','Test Gamma','test gamma',2022)",
    )
    .execute(&db)
    .await
    .unwrap();
    // The episode carries the source; membership projects it onto the
    // show, which is what makes the parent hop below worth asserting.
    sqlx::query(
        "INSERT INTO items (id, kind, title, norm_title, parent_id, season, episode)
         VALUES ('e1','episode','Episode One','episode one','s1',1,1)",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO item_sources (item_id, module_id, collection_id, path_rel)
         VALUES ('m1','m','c1','alpha.mkv'),
                ('e1','m','c2','bravo-s01e01.mkv'),
                ('g1','m','c3','gamma.mkv')",
    )
    .execute(&db)
    .await
    .unwrap();

    let kid_id = auth
        .create_user("kid", "hunter22222hunter", false)
        .await
        .unwrap();
    assert!(
        kahawai_hub::grants::set_access(&db, &kid_id, false, &["L1".to_string()])
            .await
            .unwrap(),
        "the account we just made must exist"
    );
    let kid = auth
        .login("kid", "hunter22222hunter")
        .await
        .unwrap()
        .access_token;
    auth.create_user("guest", "hunter22222hunter", false)
        .await
        .unwrap();
    let guest = auth
        .login("guest", "hunter22222hunter")
        .await
        .unwrap()
        .access_token;

    std::mem::forget(dir);
    Hub {
        api,
        db,
        boss,
        kid,
        guest,
        kid_id,
    }
}

#[tokio::test]
async fn a_grant_bounds_browse_search_and_detail() {
    let h = harness().await;

    // The list every client builds its whole UI from.
    let (_, v) = get(&h.api, &h.kid, "/api/v1/libraries").await;
    assert_eq!(library_names(&v), ["Films"]);
    let (_, v) = get(&h.api, &h.guest, "/api/v1/libraries").await;
    assert_eq!(library_names(&v), ["Anime", "Films"]);
    let (_, v) = get(&h.api, &h.boss, "/api/v1/libraries").await;
    assert_eq!(library_names(&v), ["Anime", "Films"]);

    // A library page: granted, and not.
    let (status, v) = get(&h.api, &h.kid, "/api/v1/items?library=L1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(titles(&v), ["Test Alpha"]);
    let (status, _) = get(&h.api, &h.kid, "/api/v1/items?library=L2").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "L2 was never granted");
    let (status, v) = get(&h.api, &h.guest, "/api/v1/items?library=L2").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(titles(&v), ["Test Bravo"]);

    // Unscoped browse. The orphan in c3 belongs to no library, so no
    // grant reaches it — while an unrestricted account still sees it.
    let (_, v) = get(&h.api, &h.kid, "/api/v1/items").await;
    assert_eq!(titles(&v), ["Test Alpha"]);
    assert_eq!(v["total"], 1, "the total must count what the page shows");
    let (_, v) = get(&h.api, &h.guest, "/api/v1/items").await;
    assert_eq!(titles(&v), ["Test Alpha", "Test Bravo", "Test Gamma"]);
    assert_eq!(v["total"], 3);

    // Cross-library search, and search inside the one library held.
    let (_, v) = get(&h.api, &h.kid, "/api/v1/items?q=test").await;
    assert_eq!(titles(&v), ["Test Alpha"]);
    assert_eq!(v["total"], 1);
    let (_, v) = get(&h.api, &h.kid, "/api/v1/items?q=test&library=L1").await;
    assert_eq!(titles(&v), ["Test Alpha"]);
    let (_, v) = get(&h.api, &h.guest, "/api/v1/items?q=test").await;
    assert_eq!(titles(&v), ["Test Alpha", "Test Bravo", "Test Gamma"]);

    // Every route keyed by an item id, through the one middleware.
    for uri in [
        "/api/v1/items/s1",
        "/api/v1/items/s1/children",
        // Fonts needs a connected mediahost and 500s in this fixture.
        // Included anyway, and asserted as "not 404" for the accounts
        // that may see it: reaching the handler at all is the property
        // under test, and a gate that fired would have said 404 first.
        "/api/v1/items/s1/fonts",
        // The episode: reachable only through its show's membership, so
        // this is the parent hop, not a second grant.
        "/api/v1/items/e1",
        // And the item no library holds.
        "/api/v1/items/g1",
    ] {
        let (status, _) = get(&h.api, &h.kid, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri} must be hidden");
        let (status, _) = get(&h.api, &h.guest, uri).await;
        assert_ne!(status, StatusCode::NOT_FOUND, "{uri} for an open account");
        let (status, _) = get(&h.api, &h.boss, uri).await;
        assert_ne!(status, StatusCode::NOT_FOUND, "{uri} for an admin");
    }
    for uri in ["/api/v1/items/s1", "/api/v1/items/e1", "/api/v1/items/g1"] {
        let (status, _) = get(&h.api, &h.guest, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri} for an open account");
    }
    for uri in ["/api/v1/items/m1", "/api/v1/items/m1/children"] {
        let (status, _) = get(&h.api, &h.kid, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri} is granted");
    }

    // Artwork answers 404 either way — there is none — so the STATUS
    // cannot tell the two apart and the body has to.
    let (_, denied) = call(&h.api, &h.kid, "GET", "/api/v1/items/s1/artwork", None).await;
    let (_, granted) = call(&h.api, &h.kid, "GET", "/api/v1/items/m1/artwork", None).await;
    assert_eq!(denied, "no such item");
    assert_eq!(granted, "no artwork");

    // QUERY — the negotiation endpoint, and the one route in the group
    // that is a METHOD-ROUTER FALLBACK rather than a method. Worth its
    // own case: if a route_layer did not cover fallbacks, "can I play
    // this" would be the single hole left in the gate.
    let body = json!({ "profile": null });
    let (status, _) = call(
        &h.api,
        &h.kid,
        "QUERY",
        "/api/v1/items/s1",
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&h.api, &h.guest, "QUERY", "/api/v1/items/s1", Some(body)).await;
    assert_ne!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn playback_refuses_an_item_outside_the_grant() {
    let h = harness().await;
    let body = json!({ "item_id": "s1", "mode": "direct" });

    let (status, _) = call(
        &h.api,
        &h.kid,
        "POST",
        "/api/v1/playback/sessions",
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The open account gets past the grant check and fails on the real
    // thing instead — no mediahost is connected. Anything but 404 proves
    // the refusal above came from the grant and not from the machinery.
    let (status, _) = call(
        &h.api,
        &h.guest,
        "POST",
        "/api/v1/playback/sessions",
        Some(body),
    )
    .await;
    assert_ne!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn collections_stop_at_the_granted_libraries() {
    let h = harness().await;
    let ids = |v: &Value| -> Vec<String> {
        v["collections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["collection_id"].as_str().unwrap().to_string())
            .collect()
    };
    let (_, v) = get(&h.api, &h.kid, "/api/v1/collections").await;
    assert_eq!(ids(&v), ["c1"]);
    let (_, v) = get(&h.api, &h.guest, "/api/v1/collections").await;
    assert_eq!(ids(&v), ["c1", "c2", "c3"]);
}

#[tokio::test]
async fn no_grants_and_no_flag_is_no_access() {
    let h = harness().await;
    assert!(
        kahawai_hub::grants::set_access(&h.db, &h.kid_id, false, &[])
            .await
            .unwrap()
    );
    // No such account: a return value, not an error to read prose out of.
    assert!(
        !kahawai_hub::grants::set_access(&h.db, "01NOSUCHUSER", false, &[])
            .await
            .unwrap()
    );

    let (_, v) = get(&h.api, &h.kid, "/api/v1/libraries").await;
    assert!(library_names(&v).is_empty());
    let (_, v) = get(&h.api, &h.kid, "/api/v1/items").await;
    assert!(titles(&v).is_empty());
    assert_eq!(v["total"], 0);
    let (status, _) = get(&h.api, &h.kid, "/api/v1/items/m1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_admin_api_round_trips_access() {
    let h = harness().await;

    let (status, v) = get(&h.api, &h.boss, "/admin/v1/users").await;
    assert_eq!(status, StatusCode::OK);
    let users = v["users"].as_array().unwrap();
    let kid = users
        .iter()
        .find(|u| u["username"] == "kid")
        .expect("kid is listed");
    assert_eq!(kid["all_libraries"], false);
    assert_eq!(kid["libraries"], json!(["L1"]));
    let guest = users.iter().find(|u| u["username"] == "guest").unwrap();
    assert_eq!(guest["all_libraries"], true);
    assert_eq!(guest["libraries"], json!([]));

    // Grant L2 as well — and hand in a library id that no longer exists,
    // which must be dropped rather than fail the call.
    let uri = format!("/admin/v1/users/{}/libraries", h.kid_id);
    let (status, body) = call(
        &h.api,
        &h.boss,
        "PUT",
        &uri,
        Some(json!({ "all_libraries": false, "libraries": ["L1", "L2", "gone"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let stored: Value = serde_json::from_str(&body).unwrap();
    let mut got: Vec<&str> = stored["libraries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    got.sort();
    assert_eq!(got, ["L1", "L2"]);

    // In force on the next request, not on the next token.
    let (_, v) = get(&h.api, &h.kid, "/api/v1/libraries").await;
    assert_eq!(library_names(&v), ["Anime", "Films"]);
    let (status, _) = get(&h.api, &h.kid, "/api/v1/items/s1").await;
    assert_eq!(status, StatusCode::OK);

    // An account that is not there: 404, decided by the row count the
    // update touched rather than by matching an error string.
    let (status, _) = call(
        &h.api,
        &h.boss,
        "PUT",
        "/admin/v1/users/01NOSUCHUSER/libraries",
        Some(json!({ "all_libraries": true })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A non-admin cannot read or write any of it.
    let (status, _) = get(&h.api, &h.kid, "/admin/v1/users").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &h.api,
        &h.kid,
        "PUT",
        &uri,
        Some(json!({ "all_libraries": true })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Deleting a library takes its grants with it (FK cascade), so a
    // library id can never outlive what it pointed at.
    let (status, _) = call(&h.api, &h.boss, "DELETE", "/admin/v1/libraries/L2", None).await;
    assert!(status.is_success(), "{status}");
    let left: Vec<String> =
        sqlx::query_scalar("SELECT library_id FROM user_libraries WHERE user_id = ?")
            .bind(&h.kid_id)
            .fetch_all(&h.db)
            .await
            .unwrap();
    assert_eq!(left, ["L1"]);
}

#[tokio::test]
async fn deleting_an_account_takes_its_grants() {
    let h = harness().await;
    let (status, _) = call(
        &h.api,
        &h.boss,
        "DELETE",
        &format!("/admin/v1/users/{}", h.kid_id),
        None,
    )
    .await;
    assert!(status.is_success(), "{status}");
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_libraries WHERE user_id = ?")
        .bind(&h.kid_id)
        .fetch_one(&h.db)
        .await
        .unwrap();
    assert_eq!(left, 0, "grants must not outlive the account");
}
