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
    auth.complete_setup("boss", "hunter22222hunter")
        .await
        .unwrap();
    let boss = auth
        .login("boss", "hunter22222hunter")
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
        "INSERT INTO items(id,kind,title,norm_title,year,module_id,collection_id)
         VALUES('m1','movie','Test Alpha','test alpha',2020,'m','c1'),
               ('s1','show','Test Bravo','test bravo',2021,'m','c2'),
               ('g1','movie','Test Gamma','test gamma',2022,'m','c3')",
    )
    .execute(&db)
    .await
    .unwrap();
    // The episode carries the source; membership projects it onto the
    // show, which is what makes the parent hop below worth asserting.
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id)
         VALUES('e1','episode','Episode One','episode one','s1',1,1,'m','c2')",
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

    // A nested subtitle id is not a bearer capability. Even an unrestricted
    // account cannot borrow a physical track from another item route.
    let root: i64 = sqlx::query_scalar(
        "INSERT INTO collection_roots(module_id,collection_id,root_token,normalized_path)
         VALUES('m','c2','root-c2','/m/c2') RETURNING id",
    )
    .fetch_one(&h.db)
    .await
    .unwrap();
    let source: i64 = sqlx::query_scalar(
        "INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,
                           head_xxh3,tail_xxh3,oshash,streams_json)
         VALUES('m','c2',?,'episode.mkv',1,1,1,1,1,'{}') RETURNING id",
    )
    .bind(root)
    .fetch_one(&h.db)
    .await
    .unwrap();
    kahawai_hub::registry::bind_file_to_item(&mut h.db.acquire().await.unwrap(), source, "e1")
        .await
        .unwrap();
    let track: i64 = sqlx::query_scalar(
        "INSERT INTO subtitle_tracks(source_id,origin,stream_index,format)
         VALUES(?,'embedded',0,'srt') RETURNING id",
    )
    .bind(source)
    .fetch_one(&h.db)
    .await
    .unwrap();
    let (status, _) = get(
        &h.api,
        &h.guest,
        &format!("/api/v1/items/m1/subtitles/{track}.vtt"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

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

    // Artwork answers 404 either way — there is none — so the STATUS cannot
    // tell the two apart, and neither can the CODE: both are `not_found`, on
    // purpose. A distinct code for the refusal would answer "that item exists,
    // you just may not have it" on the one route whose denials are supposed to
    // be indistinguishable from absence. Only the message differs, and no
    // client is meant to read it — this test does, because it is the only
    // handle on the two paths from outside.
    let (_, denied) = call(&h.api, &h.kid, "GET", "/api/v1/items/s1/artwork", None).await;
    let (_, granted) = call(&h.api, &h.kid, "GET", "/api/v1/items/m1/artwork", None).await;
    assert_eq!(denied, r#"{"code":"not_found","message":"no such item"}"#);
    assert_eq!(granted, r#"{"code":"not_found","message":"no artwork"}"#);

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

/// Promotion and demotion (HUB-10). The endpoint exists so an operator can
/// hand out admin rights from the panel; the reason it needs tests is the
/// two ways it must refuse, because both of them are how a hub ends up with
/// nobody who can administer it.
#[tokio::test]
async fn admin_role_changes_invalidate_access_and_keep_one_admin() {
    let h = harness().await;
    let boss_id: String = sqlx::query_scalar("SELECT id FROM users WHERE username = 'boss'")
        .fetch_one(&h.db)
        .await
        .unwrap();
    let is_admin = |id: String| {
        let db = h.db.clone();
        async move {
            sqlx::query_scalar::<_, bool>("SELECT is_admin FROM users WHERE id = ?")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    let promote = |tok: String, id: String, admin: bool| {
        let api = h.api.clone();
        async move {
            call(
                &api,
                &tok,
                "PUT",
                &format!("/admin/v1/users/{id}/admin"),
                Some(json!({ "admin": admin })),
            )
            .await
            .0
        }
    };

    // The obvious attack first: an ordinary account promoting itself.
    assert_eq!(
        promote(h.kid.clone(), h.kid_id.clone(), true).await,
        StatusCode::FORBIDDEN,
        "a non-admin must not be able to promote anyone, least of all itself"
    );
    assert!(!is_admin(h.kid_id.clone()).await);

    // What the panel is for.
    assert_eq!(
        promote(h.boss.clone(), h.kid_id.clone(), true).await,
        StatusCode::OK
    );
    assert!(is_admin(h.kid_id.clone()).await);

    // Idempotent: the panel shows the state and sets it with one control,
    // so it will re-send what is already true.
    assert_eq!(
        promote(h.boss.clone(), h.kid_id.clone(), true).await,
        StatusCode::OK
    );
    assert!(is_admin(h.kid_id.clone()).await);

    // Mint a token for the newly promoted account. Role changes bump the
    // durable access generation, so its original non-admin token remains
    // invalid rather than gaining rights from the database lookup.
    let login = |username: &'static str| {
        let api = h.api.clone();
        async move {
            let (status, body) = call(
                &api,
                "",
                "POST",
                "/api/v1/auth/token",
                Some(json!({ "client": "api", "username": username, "password": "hunter22222hunter" })),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            serde_json::from_str::<Value>(&body).unwrap()["access_token"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };
    let kid_admin_token = login("kid").await;

    // Self-demotion is now safe: the role and generation move in one write.
    // The token that authorized it is rejected on the very next request.
    assert_eq!(
        promote(h.boss.clone(), boss_id.clone(), false).await,
        StatusCode::OK
    );
    assert!(!is_admin(boss_id.clone()).await);
    assert_eq!(
        promote(h.boss.clone(), boss_id.clone(), true).await,
        StatusCode::UNAUTHORIZED,
        "a role-changing token remained usable"
    );

    // Hand administration back, then prove the same invalidation for the
    // other account without relying on a special self-change guard.
    assert_eq!(
        promote(kid_admin_token.clone(), boss_id.clone(), true).await,
        StatusCode::OK
    );
    assert_eq!(
        promote(kid_admin_token.clone(), h.kid_id.clone(), false).await,
        StatusCode::OK
    );
    assert!(!is_admin(h.kid_id.clone()).await);
    assert_eq!(
        promote(kid_admin_token, h.kid_id.clone(), true).await,
        StatusCode::UNAUTHORIZED,
        "a demoted account could still use its old admin token"
    );

    // A freshly authenticated sole admin reaches the database backstop when
    // attempting self-demotion. This is a state conflict, not stale auth.
    let boss = login("boss").await;
    let (status, body) = call(
        &h.api,
        &boss,
        "PUT",
        &format!("/admin/v1/users/{boss_id}/admin"),
        Some(json!({ "admin": false })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("last admin"));
    assert!(is_admin(boss_id.clone()).await, "nothing was written");

    // Still asserted at the layer below, so it stays true for any caller that
    // does not go through the handler.
    let dir = tempfile::tempdir().unwrap();
    let auth = kahawai_hub::auth::Auth::new(h.db.clone(), dir.path())
        .await
        .unwrap();
    assert_eq!(
        auth.set_admin(&boss_id, false).await.unwrap(),
        kahawai_hub::auth::SetAdmin::LastAdmin
    );
    assert!(is_admin(boss_id).await);

    // And a stranger is a 404, not a silent success.
    assert_eq!(
        promote(boss, "nope".to_string(), true).await,
        StatusCode::NOT_FOUND
    );
}

/// Two admins demoting each other at the same time must not leave zero.
///
/// The tempting argument is that this cannot happen because nobody can demote
/// themselves — and that is true, and not enough: neither call here IS a
/// self-demotion, so the route's guard never fires. What used to save it was
/// a `SELECT COUNT(*)` taken before the `UPDATE`, which both callers passed.
///
/// Probabilistic by nature, so it runs the race repeatedly; against the
/// read-then-write version it reached zero within about twenty attempts.
#[tokio::test]
async fn concurrent_mutual_demotion_keeps_an_admin() {
    let h = harness().await;
    let boss_id: String = sqlx::query_scalar("SELECT id FROM users WHERE username = 'boss'")
        .fetch_one(&h.db)
        .await
        .unwrap();

    // Its own handle on the same pool — the race is in SQL, not in process
    // state, so where the `Auth` came from does not matter.
    let dir = tempfile::tempdir().unwrap();
    let auth = std::sync::Arc::new(
        kahawai_hub::auth::Auth::new(h.db.clone(), dir.path())
            .await
            .unwrap(),
    );

    for attempt in 0..30 {
        // Both admins again at the top of every round.
        for id in [&boss_id, &h.kid_id] {
            sqlx::query("UPDATE users SET is_admin = 1 WHERE id = ?")
                .bind(id)
                .execute(&h.db)
                .await
                .unwrap();
        }

        let a = auth.clone();
        let b = auth.clone();
        let (one, two) = (boss_id.clone(), h.kid_id.clone());
        let (r1, r2) = tokio::join!(
            tokio::spawn(async move { a.set_admin(&two, false).await.unwrap() }),
            tokio::spawn(async move { b.set_admin(&one, false).await.unwrap() }),
        );
        let (r1, r2) = (r1.unwrap(), r2.unwrap());

        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE is_admin = 1")
            .fetch_one(&h.db)
            .await
            .unwrap();
        assert_eq!(
            left, 1,
            "attempt {attempt}: {r1:?} / {r2:?} left {left} admins — a hub with none \
             cannot be recovered without editing the database by hand"
        );
        // Exactly one of them got through; the other was told why.
        assert!(
            matches!(
                (&r1, &r2),
                (
                    kahawai_hub::auth::SetAdmin::Changed,
                    kahawai_hub::auth::SetAdmin::LastAdmin
                ) | (
                    kahawai_hub::auth::SetAdmin::LastAdmin,
                    kahawai_hub::auth::SetAdmin::Changed
                )
            ),
            "attempt {attempt}: expected one Changed and one LastAdmin, got {r1:?} / {r2:?}"
        );
    }
}

/// The library a browse row names must be one the caller may open.
///
/// An item can belong to several libraries — `library_collections` is keyed
/// per collection, and 0036 says so explicitly. When one of those is withheld
/// and one is granted, the row used to report `MIN(library_id)` over both,
/// which is the withheld one whenever its id sorts first. Two things wrong at
/// once: a denial that answers, and a client sent to a library it will be
/// refused.
#[tokio::test]
async fn a_browse_row_names_only_a_library_you_may_open() {
    let h = harness().await;

    // A second library whose id sorts BEFORE the granted one, holding the
    // same collection as L1 — so `m1` is in both and MIN prefers the withheld.
    sqlx::query("INSERT INTO libraries (id, name, media_type) VALUES ('AAA','Withheld','movies')")
        .execute(&h.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO library_collections (library_id, module_id, collection_id)
         VALUES ('AAA','m','c1')",
    )
    .execute(&h.db)
    .await
    .unwrap();

    // The kid holds L1 only. Confirm the fixture really is ambiguous.
    let both: Vec<String> = sqlx::query_scalar(
        "SELECT lc.library_id FROM items i JOIN library_collections lc
           ON (lc.module_id,lc.collection_id)=(i.module_id,i.collection_id)
          WHERE i.id='m1' ORDER BY lc.library_id",
    )
    .fetch_all(&h.db)
    .await
    .unwrap();
    assert_eq!(both, ["AAA", "L1"], "the fixture must be in both libraries");

    let named = |v: &Value| -> Vec<String> {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["id"] == "m1")
            .map(|i| i["library_id"].as_str().unwrap_or("null").to_string())
            .collect()
    };

    // Every shape that can carry the column, including the one that named a
    // library explicitly — that request agreeing with itself is the sharpest
    // of the three.
    for path in [
        "/api/v1/items",
        "/api/v1/items?library=L1",
        "/api/v1/items?q=test",
    ] {
        let (status, v) = get(&h.api, &h.kid, path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(
            named(&v),
            ["L1"],
            "{path}: a restricted account must be told the library it holds"
        );
    }

    // An unrestricted account is not narrowed by this: it may open both, so
    // either answer is honest and it keeps the cheap MIN.
    let (_, v) = get(&h.api, &h.guest, "/api/v1/items").await;
    assert_eq!(named(&v), ["AAA"]);

    // ...but a request that NAMES a library must be answered with that one,
    // and only an account holding both can tell the difference. Above, the
    // grant filter collapses the set to a single element, so the assertion
    // passes whether or not the query prefers what was asked for — the check
    // could not fail for the behaviour it is written about.
    let (status, v) = get(&h.api, &h.guest, "/api/v1/items?library=L1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        named(&v),
        ["L1"],
        "browsing L1 must name L1, not whichever id happens to sort first"
    );
    // The other way round is a CONTROL, not evidence: `AAA` is what `MIN`
    // returns anyway, so this cannot fail for the behaviour above. It is here
    // to kill the other wrong answer — a query that always names the library
    // asked for by ignoring membership entirely, which the assertion above
    // would be perfectly happy with.
    let (_, v) = get(&h.api, &h.guest, "/api/v1/items?library=AAA").await;
    assert_eq!(named(&v), ["AAA"], "naming AAA must not narrow it away");
}
