//! OPS-1 + HUB-10/11: setup gating, token auth, refresh rotation.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use kahawai_hub::auth::Auth;
use kahawai_hub::registry::Registry;
use tower::ServiceExt;

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

fn post(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::post(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get_authed(uri: &str, token: &str) -> Request<Body> {
    Request::get(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn setup_then_auth_flow() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(Registry::new(db.clone(), Default::default()));
    let auth = Arc::new(Auth::new(db.clone(), dir.path()).await.unwrap());
    let setup_token = auth.setup_token().expect("fresh hub must be in setup mode");
    let api = test_router(registry, auth.clone(), Arc::new(kahawai_hub::sessions::Sessions::new(tempfile::tempdir().unwrap().keep())));

    // Setup mode: nothing else is reachable (OPS-1)…
    let resp = api
        .clone()
        .oneshot(Request::get("/api/v1/items").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    // …and login is blocked too.
    let resp = api
        .clone()
        .oneshot(post("/api/v1/auth/token", serde_json::json!({"username": "a", "password": "b"})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

    // Wrong setup token → rejected, still in setup mode.
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/setup",
            serde_json::json!({"token": "NOPE-NOPE", "username": "ingmar", "password": "hunter22222"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(auth.setup_required());

    // Correct token creates the admin and returns a working token pair.
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/setup",
            serde_json::json!({"token": setup_token, "username": "ingmar", "password": "hunter22222"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let tokens = body_json(resp).await;
    let access = tokens["access_token"].as_str().unwrap().to_string();
    let refresh = tokens["refresh_token"].as_str().unwrap().to_string();

    // Setup cannot run twice.
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/setup",
            serde_json::json!({"token": setup_token, "username": "eve", "password": "evil-password"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Protected route: no token → 401; with token → 200.
    let resp = api
        .clone()
        .oneshot(Request::get("/api/v1/items").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = api.clone().oneshot(get_authed("/api/v1/items", &access)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Login: wrong password rejected, right password issues tokens.
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/auth/token",
            serde_json::json!({"username": "ingmar", "password": "wrong-password"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/auth/token",
            serde_json::json!({"username": "ingmar", "password": "hunter22222"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Refresh rotates: new pair works, the old refresh token is dead.
    let resp = api
        .clone()
        .oneshot(post("/api/v1/auth/refresh", serde_json::json!({"refresh_token": refresh})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let rotated = body_json(resp).await;
    assert!(rotated["access_token"].as_str().is_some());
    let resp = api
        .clone()
        .oneshot(post("/api/v1/auth/refresh", serde_json::json!({"refresh_token": refresh})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "refresh tokens are single-use");

    // CLI reset-password: old password dies, new one works.
    kahawai_hub::auth::reset_password(&db, "ingmar", "new-password-9").await.unwrap();
    assert!(auth.login("ingmar", "hunter22222").await.is_err());
    assert!(auth.login("ingmar", "new-password-9").await.is_ok());
    assert!(
        kahawai_hub::auth::reset_password(&db, "ghost", "whatever-pass").await.is_err(),
        "unknown user must error"
    );

    // Restarted hub with existing users skips setup mode.
    let auth2 = Auth::new(db.clone(), dir.path()).await.unwrap();
    assert!(!auth2.setup_required());

    // Cookie auth: media elements can't set headers, so the kahawai_token
    // cookie must satisfy the middleware too.
    let resp = api
        .clone()
        .oneshot(
            Request::get("/api/v1/items")
                .header("cookie", format!("other=1; kahawai_token={access}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = api
        .clone()
        .oneshot(
            Request::get("/api/v1/items")
                .header("cookie", "kahawai_token=garbage")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Embedded SPA: / redirects, /app/ serves the shell, client routes
    // fall back to it, hashed assets are immutable.
    let resp = api
        .clone()
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
    for uri in ["/app/", "/app/some/client/route"] {
        let resp = api
            .clone()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        assert_eq!(resp.headers()["content-type"], "text/html; charset=utf-8");
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("kahawai"));
    }
}

/// Router with default admin plumbing for tests that don't exercise it.
fn test_router(
    registry: std::sync::Arc<kahawai_hub::registry::Registry>,
    auth: std::sync::Arc<kahawai_hub::auth::Auth>,
    sessions: std::sync::Arc<kahawai_hub::sessions::Sessions>,
) -> axum::Router {
    let ca = std::sync::Arc::new(
        kahawai_hub::pki::HubCa::load_or_create(tempfile::tempdir().unwrap().keep().as_path())
            .unwrap(),
    );
    let enrollments = std::sync::Arc::new(kahawai_hub::enrollment_service::EnrollmentService::new(
        ca,
        registry.clone(),
        std::time::Duration::from_secs(900),
        90,
    ));
    kahawai_hub::api::router(registry, auth, sessions, enrollments, Arc::new(kahawai_hub::subtitles::Subtitles::new(tempfile::tempdir().unwrap().keep())), Arc::new(kahawai_hub::artwork::Artwork::new(tempfile::tempdir().unwrap().keep(), Arc::new(kahawai_hub::enrich::Enricher::new(tempfile::tempdir().unwrap().keep())))), Arc::new(kahawai_hub::enrich::Enricher::new(tempfile::tempdir().unwrap().keep())), kahawai_hub::api::NetOptions::default())
}

#[tokio::test]
async fn items_filter_by_library() {
    use kahawai_hub::registry::FileUpsertRecord;
    let rec = |path: &str, size: u64| FileUpsertRecord {
        path_rel: path.into(),
        size,
        mtime_unix: 1,
        head_xxh3: size,
        tail_xxh3: size + 1,
        oshash: size + 2,
        streams_json: "{}".into(),
    };

    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(Registry::new(db.clone(), Default::default()));
    let auth = Arc::new(Auth::new(db.clone(), dir.path()).await.unwrap());
    let setup_token = auth.setup_token().unwrap();
    let api = test_router(
        registry.clone(),
        auth.clone(),
        Arc::new(kahawai_hub::sessions::Sessions::new(tempfile::tempdir().unwrap().keep())),
    );
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/setup",
            serde_json::json!({"token": setup_token, "username": "ingmar", "password": "hunter22222"}),
        ))
        .await
        .unwrap();
    let token = body_json(resp).await["access_token"].as_str().unwrap().to_string();

    registry.record_satellite("01HOST", "mediahost", "nas", "fp").await.unwrap();
    registry
        .announce_collection("01HOST", "movies", "movies", &["/srv/movies".into()])
        .await
        .unwrap();
    registry
        .announce_collection("01HOST", "series", "series", &["/srv/series".into()])
        .await
        .unwrap();
    registry
        .upsert_files("01HOST", "movies", vec![rec("Heat (1995).mkv", 100)])
        .await
        .unwrap();
    registry
        .upsert_files("01HOST", "series", vec![rec("Andor/Season 1/Andor.S01E01.mkv", 200)])
        .await
        .unwrap();

    let libs =
        body_json(api.clone().oneshot(get_authed("/api/v1/libraries", &token)).await.unwrap())
            .await;
    let lib_id = |name: &str| {
        libs["libraries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["name"] == name)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    };

    // Unfiltered: movie + show. Per-library: exactly one each — the show
    // matches through its episodes' sources, not its own (it has none).
    let titles = |v: &serde_json::Value| {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["title"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    let all =
        body_json(api.clone().oneshot(get_authed("/api/v1/items", &token)).await.unwrap()).await;
    assert_eq!(titles(&all), ["Andor", "Heat"]);
    let movies = body_json(
        api.clone()
            .oneshot(get_authed(&format!("/api/v1/items?library={}", lib_id("movies")), &token))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(titles(&movies), ["Heat"]);
    let series = body_json(
        api.clone()
            .oneshot(get_authed(&format!("/api/v1/items?library={}", lib_id("series")), &token))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(titles(&series), ["Andor"]);
    // Unknown library id → empty, not everything.
    let none = body_json(
        api.clone()
            .oneshot(get_authed("/api/v1/items?library=NOPE", &token))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(titles(&none), Vec::<String>::new());

    // Item detail carries navigation lineage: episode → its parent show.
    let movie_id = movies["items"][0]["id"].as_str().unwrap();
    let detail =
        body_json(api.clone().oneshot(get_authed(&format!("/api/v1/items/{movie_id}"), &token)).await.unwrap())
            .await;
    assert!(detail["parent_id"].is_null());
    let show_id = series["items"][0]["id"].as_str().unwrap();
    let children = body_json(
        api.clone()
            .oneshot(get_authed(&format!("/api/v1/items/{show_id}/children"), &token))
            .await
            .unwrap(),
    )
    .await;
    let ep_id = children["children"][0]["id"].as_str().unwrap();
    let detail =
        body_json(api.clone().oneshot(get_authed(&format!("/api/v1/items/{ep_id}"), &token)).await.unwrap())
            .await;
    assert_eq!(detail["parent_id"].as_str().unwrap(), show_id);
}

#[tokio::test]
async fn admin_creates_users() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(Registry::new(db.clone(), Default::default()));
    let auth = Arc::new(Auth::new(db.clone(), dir.path()).await.unwrap());
    let setup_token = auth.setup_token().unwrap();
    let api = test_router(
        registry,
        auth,
        Arc::new(kahawai_hub::sessions::Sessions::new(tempfile::tempdir().unwrap().keep())),
    );
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/setup",
            serde_json::json!({"token": setup_token, "username": "root", "password": "hunter22222"}),
        ))
        .await
        .unwrap();
    let admin_token = body_json(resp).await["access_token"].as_str().unwrap().to_string();

    let create = |token: String, body: serde_json::Value| {
        let api = api.clone();
        async move {
            api.oneshot(
                Request::post("/admin/v1/users")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };

    // Admin creates a plain user; the new user can log in but not create.
    let resp = create(
        admin_token.clone(),
        serde_json::json!({"username": "tester", "password": "longenough"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/auth/token",
            serde_json::json!({"username": "tester", "password": "longenough"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let user_token = body_json(resp).await["access_token"].as_str().unwrap().to_string();
    let resp = create(
        user_token,
        serde_json::json!({"username": "sneaky", "password": "longenough"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    // Duplicates and short passwords are rejected cleanly.
    let resp = create(
        admin_token.clone(),
        serde_json::json!({"username": "tester", "password": "longenough"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp =
        create(admin_token, serde_json::json!({"username": "shorty", "password": "short"})).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn login_throttles_after_repeated_failures() {
    // OPS-2: five consecutive bad passwords lock the account — the
    // sixth attempt gets 429 even with the CORRECT password.
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(Registry::new(db.clone(), Default::default()));
    let auth = Arc::new(Auth::new(db.clone(), dir.path()).await.unwrap());
    let setup_token = auth.setup_token().unwrap();
    let api = test_router(
        registry,
        auth.clone(),
        Arc::new(kahawai_hub::sessions::Sessions::new(
            tempfile::tempdir().unwrap().keep(),
        )),
    );
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/setup",
            serde_json::json!({"token": setup_token, "username": "ingmar", "password": "hunter22222"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    for _ in 0..4 {
        let resp = api
            .clone()
            .oneshot(post(
                "/api/v1/auth/token",
                serde_json::json!({"username": "ingmar", "password": "wrong"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
    // Fifth failure crosses the threshold and starts the lockout…
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/auth/token",
            serde_json::json!({"username": "ingmar", "password": "wrong"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // …so even the correct password is refused while locked.
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/auth/token",
            serde_json::json!({"username": "ingmar", "password": "hunter22222"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    // A different account is unaffected (throttle is per key, and the
    // in-process test has no source address to share a bucket on).
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/auth/token",
            serde_json::json!({"username": "someone-else", "password": "wrong"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
