//! OPS-1 + HUB-10/11: setup gating, token auth, refresh rotation.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use kahawai_hub::auth::Auth;
use kahawai_hub::registry::Registry;
use tower::ServiceExt;

static AUTH_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}
fn set_cookies(response: &axum::response::Response) -> Vec<String> {
    response
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_string())
        .collect()
}

fn cookie_value(cookies: &[String], name: &str) -> String {
    cookies
        .iter()
        .find_map(|cookie| {
            cookie
                .strip_prefix(&format!("{name}="))
                .and_then(|value| value.split_once(';'))
                .map(|(value, _)| value.to_string())
        })
        .unwrap()
}

fn post(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::post(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}
fn post_headers(uri: &str, body: serde_json::Value, headers: &[(&str, &str)]) -> Request<Body> {
    let mut request = post(uri, body);
    for (name, value) in headers {
        request.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            axum::http::HeaderValue::from_str(value).unwrap(),
        );
    }
    request
}

fn get_authed(uri: &str, token: &str) -> Request<Body> {
    Request::get(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn post_authed(uri: &str, token: &str, body: serde_json::Value) -> Request<Body> {
    Request::post(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn setup_maps_validation_and_storage_failures_separately() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let auth = Arc::new(Auth::new(db.clone(), dir.path()).await.unwrap());
    let local = kahawai_hub::api::setup_router(auth.clone());
    let request = |username: &str, password: &str| {
        Request::post("/api/v1/setup")
            .header("host", "localhost:8422")
            .header("origin", "http://localhost:8422")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"username": username, "password": password}).to_string(),
            ))
            .unwrap()
    };

    let invalid = local.clone().oneshot(request("", "short")).await.unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert!(auth.setup_required());

    db.close().await;
    let unavailable = local
        .oneshot(request("admin", "hunter222222"))
        .await
        .unwrap();
    assert_eq!(unavailable.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(auth.setup_required());
}
#[tokio::test]
async fn password_establishment_uses_twelve_unicode_scalars_and_preserves_legacy_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let auth = Auth::new(db.clone(), dir.path()).await.unwrap();

    assert_eq!(
        auth.complete_setup("", "............")
            .await
            .unwrap_err()
            .to_string(),
        "username required"
    );
    assert_eq!(
        auth.complete_setup("root", &"🙂".repeat(11))
            .await
            .unwrap_err()
            .to_string(),
        "password must be at least 12 characters"
    );
    auth.complete_setup("root", "............").await.unwrap();

    assert_eq!(
        auth.create_user("short", &"🙂".repeat(11), false)
            .await
            .unwrap_err()
            .to_string(),
        "password must be at least 12 characters"
    );
    auth.create_user("symbols", "!!!!!!!!!!!!", false)
        .await
        .unwrap();
    auth.create_user("passphrase", &"x".repeat(64), false)
        .await
        .unwrap();
    assert_eq!(
        kahawai_hub::auth::reset_password(&db, "symbols", "eleven-chrs")
            .await
            .unwrap_err()
            .to_string(),
        "password must be at least 12 characters"
    );
    let unicode_password = "🙂".repeat(12);
    kahawai_hub::auth::reset_password(&db, "symbols", &unicode_password)
        .await
        .unwrap();
    auth.login("symbols", &unicode_password).await.unwrap();

    let legacy_hash = kahawai_hub::auth::hash_password("short").unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, is_admin)
         VALUES ('legacy', 'legacy', ?, 0)",
    )
    .bind(legacy_hash)
    .execute(&db)
    .await
    .unwrap();
    auth.login("legacy", "short").await.unwrap();
}

#[tokio::test]
async fn setup_then_auth_flow() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(Registry::new(db.clone(), Default::default()));
    let auth = Arc::new(Auth::new(db.clone(), dir.path()).await.unwrap());
    assert!(auth.setup_required());
    let local = kahawai_hub::api::setup_router(auth.clone());
    let api = test_router(
        registry,
        auth.clone(),
        Arc::new(kahawai_hub::sessions::Sessions::new(
            tempfile::tempdir().unwrap().keep(),
        )),
    );

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
        .oneshot(post(
            "/api/v1/auth/token",
            serde_json::json!({"client": "api", "username": "a", "password": "b"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

    // The public listener has no initial-admin mutation at all.
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/setup",
            serde_json::json!({"username": "attacker", "password": "hunter222222"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(auth.setup_required());

    // A foreign web origin cannot drive the loopback listener (DNS rebinding
    // and blind form posts do not become local presence).
    let resp = local
        .clone()
        .oneshot(
            Request::post("/api/v1/setup")
                .header("host", "localhost:8422")
                .header("origin", "https://attacker.example")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"username": "attacker", "password": "hunter222222"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(auth.setup_required());

    // The local same-origin page creates the admin without minting a token on
    // the disposable setup origin.
    let resp = local
        .clone()
        .oneshot(
            Request::post("/api/v1/setup")
                .header("host", "localhost:8422")
                .header("origin", "http://localhost:8422")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"username": "ingmar", "password": "hunter222222"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM refresh_families")
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
    let tokens = body_json(
        api.clone()
            .oneshot(post("/api/v1/auth/token", serde_json::json!({"client": "api", "username": "ingmar", "password": "hunter222222"})))
            .await
            .unwrap(),
    )
    .await;
    let access = tokens["access_token"].as_str().unwrap().to_string();
    let refresh = tokens["refresh_token"].as_str().unwrap().to_string();

    // Setup cannot run twice.
    let resp = local
        .clone()
        .oneshot(
            Request::post("/api/v1/setup")
                .header("host", "127.0.0.1:8422")
                .header("origin", "http://127.0.0.1:8422")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"username": "eve", "password": "evil-password"}).to_string(),
                ))
                .unwrap(),
        )
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
    let resp = api
        .clone()
        .oneshot(get_authed("/api/v1/items", &access))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Login: wrong password rejected, right password issues tokens.
    let resp = api
        .clone()
        .oneshot(post("/api/v1/auth/token", serde_json::json!({"client": "api", "username": "ingmar", "password": "wrong-password"})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/auth/token",
            serde_json::json!({"client": "api", "username": "ingmar", "password": "hunter222222"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Refresh rotates: new pair works, the old refresh token is dead.
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/auth/refresh",
            serde_json::json!({"client": "api", "refresh_token": refresh}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let rotated = body_json(resp).await;
    assert!(rotated["access_token"].as_str().is_some());
    let rotated_refresh = rotated["refresh_token"].as_str().unwrap().to_string();
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/auth/refresh",
            serde_json::json!({"client": "api", "refresh_token": refresh}),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "refresh tokens are single-use"
    );
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/auth/refresh",
            serde_json::json!({"client": "api", "refresh_token": rotated_refresh}),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "replay must revoke the rotated token's whole family"
    );

    // CLI reset-password: old password dies, new one works.
    kahawai_hub::auth::reset_password(&db, "ingmar", "new-password-9")
        .await
        .unwrap();
    assert!(auth.login("ingmar", "hunter222222").await.is_err());
    let after_reset = auth.login("ingmar", "new-password-9").await.unwrap();
    assert!(
        auth.authenticate(&access).await.is_err(),
        "password reset left the old access token usable"
    );
    assert!(
        kahawai_hub::auth::reset_password(&db, "ghost", "whatever-pass")
            .await
            .is_err(),
        "unknown user must error"
    );

    // Restarted hub with existing users skips setup mode.
    let auth2 = Auth::new(db.clone(), dir.path()).await.unwrap();
    assert!(!auth2.setup_required());
    assert!(
        auth2.authenticate(&access).await.is_err(),
        "password-reset invalidation did not survive Auth restart"
    );

    // Bootstrap is public but understands the narrow media cookie. Catalogue
    // reads remain bearer-only.
    let cookie_bootstrap = |token: String| {
        let api = api.clone();
        async move {
            body_json(
                api.oneshot(
                    Request::get("/api/v1/bootstrap")
                        .header("cookie", format!("other=1; kahawai_media={token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
            )
            .await
        }
    };
    assert_eq!(cookie_bootstrap(access).await["authenticated"], false);
    assert_eq!(
        cookie_bootstrap(after_reset.access_token).await["authenticated"],
        true
    );
    let resp = api
        .clone()
        .oneshot(
            Request::get("/api/v1/items")
                .header("cookie", "kahawai_media=garbage")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Embedded SPA: / redirects, /app/ serves the shell, and client routes fall
    // back to it — but a missing ASSET must not.
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
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("kahawai"));
    }

    // A content-hashed chunk that this build does not embed is a 404, never the
    // shell. Answering it with `index.html` and a 200 is what broke every tab
    // that was open across a hub upgrade: the app code-splits its player, so
    // pressing Play fetched a hash the new binary no longer had, the browser
    // got HTML where a module was promised, and `React.lazy` caches that
    // rejection for the life of the page — the error boundary's Try again could
    // never work. The client turns this 404 into one reload; it cannot do that
    // for a 200.
    let resp = api
        .clone()
        .oneshot(
            Request::get("/app/assets/Player-NOTINTHISBUILD.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "a missing asset must not fall back to the SPA shell"
    );
}

/// Router with default admin plumbing for tests that don't exercise it.
fn test_router(
    registry: Arc<Registry>,
    auth: Arc<Auth>,
    sessions: Arc<kahawai_hub::sessions::Sessions>,
) -> axum::Router {
    test_router_with_net(registry, auth, sessions, Default::default())
}

fn test_router_with_net(
    registry: Arc<Registry>,
    auth: Arc<Auth>,
    sessions: Arc<kahawai_hub::sessions::Sessions>,
    mut net: kahawai_hub::api::NetOptions,
) -> axum::Router {
    net.setup_url = Some("http://127.0.0.1:8422".into());
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
    kahawai_hub::api::router(
        registry,
        auth,
        sessions,
        enrollments,
        Arc::new(kahawai_hub::subtitles::Subtitles::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        Arc::new(kahawai_hub::artwork::Artwork::new(
            tempfile::tempdir().unwrap().keep(),
            Arc::new(kahawai_hub::enrich::Enricher::new(
                tempfile::tempdir().unwrap().keep(),
            )),
        )),
        Arc::new(kahawai_hub::enrich::Enricher::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        net,
    )
}

async fn auth_harness() -> (
    tempfile::TempDir,
    sqlx::SqlitePool,
    Arc<Auth>,
    axum::Router,
    kahawai_hub::auth::TokenPair,
) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(Registry::new(db.clone(), Default::default()));
    let auth = Arc::new(Auth::new(db.clone(), dir.path()).await.unwrap());
    auth.complete_setup("root", "hunter22222hunter")
        .await
        .unwrap();
    let setup = auth.login("root", "hunter22222hunter").await.unwrap();
    let api = test_router(
        registry,
        auth.clone(),
        Arc::new(kahawai_hub::sessions::Sessions::new(
            tempfile::tempdir().unwrap().keep(),
        )),
    );
    (dir, db, auth, api, setup)
}
#[tokio::test]
async fn explicit_auth_modes_split_bearers_from_browser_cookies_and_enforce_origin() {
    let (_dir, _db, _auth, api, _root) = auth_harness().await;
    for body in [
        serde_json::json!({"username": "root", "password": "hunter22222hunter"}),
        serde_json::json!({"client": "desktop", "username": "root", "password": "hunter22222hunter"}),
    ] {
        let response = api
            .clone()
            .oneshot(post("/api/v1/auth/token", body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    let api_login = api
        .clone()
        .oneshot(post(
            "/api/v1/auth/token",
            serde_json::json!({
                "client": "api",
                "username": "root",
                "password": "hunter22222hunter"
            }),
        ))
        .await
        .unwrap();
    assert!(set_cookies(&api_login).is_empty());
    let api_tokens = body_json(api_login).await;
    assert!(api_tokens["access_token"].is_string());
    assert!(api_tokens["refresh_token"].is_string());
    assert!(api_tokens["expires_in"].is_number());

    let api_refresh = api
        .clone()
        .oneshot(post(
            "/api/v1/auth/refresh",
            serde_json::json!({
                "client": "api",
                "refresh_token": api_tokens["refresh_token"]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(api_refresh.status(), StatusCode::OK);
    assert!(set_cookies(&api_refresh).is_empty());
    assert!(body_json(api_refresh).await["refresh_token"].is_string());
    assert_eq!(
        api.clone()
            .oneshot(post(
                "/api/v1/auth/refresh",
                serde_json::json!({"client": "api"}),
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );

    let browser_login = api
        .clone()
        .oneshot(post_headers(
            "/api/v1/auth/token",
            serde_json::json!({
                "client": "browser",
                "username": "root",
                "password": "hunter22222hunter"
            }),
            &[("host", "hub.test:8420")],
        ))
        .await
        .unwrap();
    assert_eq!(browser_login.status(), StatusCode::OK);
    let cookies = set_cookies(&browser_login);
    let refresh = cookie_value(&cookies, "kahawai_refresh");
    let media = cookie_value(&cookies, "kahawai_media");
    assert_eq!(
        cookies,
        vec![
            format!(
                "kahawai_refresh={refresh}; Path=/api/v1/auth; Max-Age=2592000; HttpOnly; SameSite=Strict"
            ),
            format!("kahawai_media={media}; Path=/api/v1; Max-Age=900; HttpOnly; SameSite=Strict"),
        ]
    );
    let browser_body = body_json(browser_login).await;
    assert_eq!(browser_body["access_token"], media);
    assert!(browser_body.get("refresh_token").is_none());
    assert!(browser_body["expires_in"].is_number());

    assert_eq!(
        api.clone()
            .oneshot(post_headers(
                "/api/v1/auth/refresh",
                serde_json::json!({
                    "client": "browser",
                    "refresh_token": refresh
                }),
                &[("host", "hub.test:8420")],
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );

    for origin in [
        None,
        Some("null"),
        Some("https://foreign.test"),
        Some("https://["),
    ] {
        let cookie_header = format!("kahawai_refresh={refresh}");
        let mut request = post_headers(
            "/api/v1/auth/refresh",
            serde_json::json!({"client": "browser"}),
            &[("host", "hub.test:8420"), ("cookie", &cookie_header)],
        );
        if let Some(origin) = origin {
            request.headers_mut().insert(
                axum::http::header::ORIGIN,
                axum::http::HeaderValue::from_str(origin).unwrap(),
            );
        }
        let response = api.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{origin:?}");
        assert!(set_cookies(&response).is_empty());
    }

    let cookie_header = format!("kahawai_refresh={refresh}");
    let browser_refresh = api
        .clone()
        .oneshot(post_headers(
            "/api/v1/auth/refresh",
            serde_json::json!({"client": "browser"}),
            &[
                ("host", "hub.test:8420"),
                ("origin", "http://hub.test:8420"),
                ("cookie", &cookie_header),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(browser_refresh.status(), StatusCode::OK);
    let rotated_cookies = set_cookies(&browser_refresh);
    let rotated_refresh = cookie_value(&rotated_cookies, "kahawai_refresh");
    let refreshed_body = body_json(browser_refresh).await;
    assert!(refreshed_body.get("refresh_token").is_none());
    let invalid_cookie = "kahawai_refresh=garbage";
    let invalid_refresh = api
        .clone()
        .oneshot(post_headers(
            "/api/v1/auth/refresh",
            serde_json::json!({"client": "browser"}),
            &[
                ("host", "hub.test:8420"),
                ("origin", "http://hub.test:8420"),
                ("cookie", invalid_cookie),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(invalid_refresh.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(set_cookies(&invalid_refresh).len(), 2);
    assert!(
        set_cookies(&invalid_refresh)
            .iter()
            .all(|cookie| cookie.contains("Max-Age=0"))
    );

    let cookie_header = format!("kahawai_refresh={rotated_refresh}");
    let authorization = format!(
        "Bearer {}",
        refreshed_body["access_token"].as_str().unwrap()
    );
    let browser_logout = api
        .oneshot(post_headers(
            "/api/v1/auth/logout",
            serde_json::json!({"client": "browser"}),
            &[
                ("host", "hub.test:8420"),
                ("origin", "http://hub.test:8420"),
                ("cookie", &cookie_header),
                ("authorization", &authorization),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(browser_logout.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        set_cookies(&browser_logout),
        vec![
            "kahawai_refresh=; Path=/api/v1/auth; Max-Age=0; HttpOnly; SameSite=Strict",
            "kahawai_media=; Path=/api/v1; Max-Age=0; HttpOnly; SameSite=Strict",
        ]
    );
}

#[tokio::test]
async fn browser_refresh_internal_errors_do_not_clear_cookies() {
    let (_dir, db, _auth, api, _root) = auth_harness().await;
    let login = api
        .clone()
        .oneshot(post_headers(
            "/api/v1/auth/token",
            serde_json::json!({
                "client": "browser",
                "username": "root",
                "password": "hunter22222hunter"
            }),
            &[("host", "hub.test:8420")],
        ))
        .await
        .unwrap();
    let refresh = cookie_value(&set_cookies(&login), "kahawai_refresh");
    db.close().await;
    let cookie_header = format!("kahawai_refresh={refresh}");
    let response = api
        .oneshot(post_headers(
            "/api/v1/auth/refresh",
            serde_json::json!({"client": "browser"}),
            &[
                ("host", "hub.test:8420"),
                ("origin", "http://hub.test:8420"),
                ("cookie", &cookie_header),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(set_cookies(&response).is_empty());
}

#[tokio::test]
async fn media_cookie_is_limited_to_the_explicit_read_allowlist() {
    let (_dir, _db, _auth, api, root) = auth_harness().await;
    let cookie = format!("kahawai_media={}", root.access_token);
    let request = |method: axum::http::Method, uri: &str| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap()
    };

    let bootstrap = api
        .clone()
        .oneshot(request(axum::http::Method::GET, "/api/v1/bootstrap"))
        .await
        .unwrap();
    assert_eq!(body_json(bootstrap).await["authenticated"], true);
    assert_eq!(
        api.clone()
            .oneshot(request(axum::http::Method::GET, "/api/v1/events"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        api.clone()
            .oneshot(request(axum::http::Method::HEAD, "/api/v1/events"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    for (method, uri) in [
        (axum::http::Method::GET, "/api/v1/items"),
        (axum::http::Method::GET, "/admin/v1/users"),
        (axum::http::Method::POST, "/api/v1/playback/sessions"),
    ] {
        assert_eq!(
            api.clone()
                .oneshot(request(method, uri))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED,
            "{uri}"
        );
    }
    assert_eq!(
        api.clone()
            .oneshot(request(
                axum::http::Method::GET,
                "/api/v1/items/missing/artwork",
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND,
        "item grants must run after cookie authentication"
    );
    assert_eq!(
        api.clone()
            .oneshot(request(
                axum::http::Method::GET,
                "/api/v1/playback/sessions/missing/stream",
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND,
        "session ownership must run after cookie authentication"
    );
    let invalid_bearer = Request::get("/api/v1/events")
        .header("authorization", "Bearer invalid")
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        api.oneshot(invalid_bearer).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn configured_and_forwarded_origins_control_browser_cookie_security() {
    let (_dir, db, auth, _api, _root) = auth_harness().await;
    let router = |net| {
        test_router_with_net(
            Arc::new(Registry::new(db.clone(), Default::default())),
            auth.clone(),
            Arc::new(kahawai_hub::sessions::Sessions::new(
                tempfile::tempdir().unwrap().keep(),
            )),
            net,
        )
    };
    let login = |host: &str| {
        post_headers(
            "/api/v1/auth/token",
            serde_json::json!({
                "client": "browser",
                "username": "root",
                "password": "hunter22222hunter"
            }),
            &[("host", host)],
        )
    };

    let configured = router(kahawai_hub::api::NetOptions {
        public_origin: Some(
            kahawai_hub::api::PublicOrigin::parse("https://Public.Example:443").unwrap(),
        ),
        ..Default::default()
    });
    let response = configured
        .clone()
        .oneshot(login("attacker.invalid"))
        .await
        .unwrap();
    let cookies = set_cookies(&response);
    assert!(cookies.iter().all(|cookie| cookie.ends_with("; Secure")));
    let configured_refresh = cookie_value(&cookies, "kahawai_refresh");
    let cookie_header = format!("kahawai_refresh={configured_refresh}");
    assert_eq!(
        configured
            .oneshot(post_headers(
                "/api/v1/auth/refresh",
                serde_json::json!({"client": "browser"}),
                &[
                    ("host", "attacker.invalid"),
                    ("origin", "https://public.example"),
                    ("x-forwarded-proto", "http"),
                    ("x-forwarded-host", "attacker.invalid"),
                    ("cookie", &cookie_header),
                ],
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::OK,
        "configured public_url must take precedence"
    );

    let trust = Arc::new(kahawai_hub::proxy::ProxyTrust::parse(&["127.0.0.1".into()]).unwrap());
    let forwarded = router(kahawai_hub::api::NetOptions {
        proxy_trust: trust.clone(),
        ..Default::default()
    });
    let mut request = login("internal:8420");
    request
        .headers_mut()
        .insert("x-forwarded-proto", "http, https".parse().unwrap());
    request.headers_mut().insert(
        "x-forwarded-host",
        "spoofed.invalid, Public.Example:443".parse().unwrap(),
    );
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = forwarded.oneshot(request).await.unwrap();
    assert!(
        set_cookies(&response)
            .iter()
            .all(|cookie| cookie.ends_with("; Secure")),
        "trusted rightmost forwarded origin must select HTTPS"
    );

    let untrusted = router(kahawai_hub::api::NetOptions {
        proxy_trust: trust,
        ..Default::default()
    });
    let mut request = login("internal:8420");
    request
        .headers_mut()
        .insert("x-forwarded-proto", "https".parse().unwrap());
    request
        .headers_mut()
        .insert("x-forwarded-host", "public.example".parse().unwrap());
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "203.0.113.9:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = untrusted.oneshot(request).await.unwrap();
    assert!(
        set_cookies(&response)
            .iter()
            .all(|cookie| !cookie.contains("; Secure")),
        "untrusted forwarded headers must be ignored"
    );
}

#[tokio::test]
async fn concurrent_local_setup_has_exactly_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let auth = Arc::new(Auth::new(db.clone(), dir.path()).await.unwrap());
    let (a, b) = tokio::join!(
        auth.complete_setup("browser", "hunter222222"),
        auth.complete_setup("cli", "hunter222222")
    );
    assert_ne!(a.is_ok(), b.is_ok(), "two local setup transports won");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users")
            .fetch_one(&db)
            .await
            .unwrap(),
        1
    );
    assert!(!auth.setup_required());
}

#[tokio::test]
async fn items_filter_by_library() {
    use kahawai_hub::registry::FileUpsertRecord;
    let rec = |root: &str, path: &str, size: u64| FileUpsertRecord {
        root_token: kahawai_core::media::root_token(std::path::Path::new(root)),
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
    let api = test_router(
        registry.clone(),
        auth.clone(),
        Arc::new(kahawai_hub::sessions::Sessions::new(
            tempfile::tempdir().unwrap().keep(),
        )),
    );
    auth.complete_setup("ingmar", "hunter222222").await.unwrap();
    let token = auth
        .login("ingmar", "hunter222222")
        .await
        .unwrap()
        .access_token;

    registry
        .record_satellite("01HOST", "mediahost", "nas", "fp")
        .await
        .unwrap();
    registry
        .announce_collection("01HOST", "movies", "movies", &["/srv/movies".into()])
        .await
        .unwrap();
    registry
        .announce_collection("01HOST", "series", "series", &["/srv/series".into()])
        .await
        .unwrap();
    registry
        .upsert_files(
            "01HOST",
            "movies",
            vec![rec("/srv/movies", "Heat (1995).mkv", 100)],
        )
        .await
        .unwrap();
    registry
        .upsert_files(
            "01HOST",
            "series",
            vec![rec("/srv/series", "Andor/Season 1/Andor.S01E01.mkv", 200)],
        )
        .await
        .unwrap();

    let libs = body_json(
        api.clone()
            .oneshot(get_authed("/api/v1/libraries", &token))
            .await
            .unwrap(),
    )
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
    let all = body_json(
        api.clone()
            .oneshot(get_authed("/api/v1/items", &token))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(titles(&all), ["Andor", "Heat"]);
    let movies = body_json(
        api.clone()
            .oneshot(get_authed(
                &format!("/api/v1/items?library={}", lib_id("movies")),
                &token,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(titles(&movies), ["Heat"]);
    let series = body_json(
        api.clone()
            .oneshot(get_authed(
                &format!("/api/v1/items?library={}", lib_id("series")),
                &token,
            ))
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
    let detail = body_json(
        api.clone()
            .oneshot(get_authed(&format!("/api/v1/items/{movie_id}"), &token))
            .await
            .unwrap(),
    )
    .await;
    assert!(detail["parent_id"].is_null());
    let show_id = series["items"][0]["id"].as_str().unwrap();
    let children = body_json(
        api.clone()
            .oneshot(get_authed(
                &format!("/api/v1/items/{show_id}/children"),
                &token,
            ))
            .await
            .unwrap(),
    )
    .await;
    let ep_id = children["children"][0]["id"].as_str().unwrap();
    let detail = body_json(
        api.clone()
            .oneshot(get_authed(&format!("/api/v1/items/{ep_id}"), &token))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(detail["parent_id"].as_str().unwrap(), show_id);
}

#[tokio::test]
async fn admin_creates_users() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(Registry::new(db.clone(), Default::default()));
    let auth = Arc::new(Auth::new(db.clone(), dir.path()).await.unwrap());
    auth.complete_setup("root", "hunter222222").await.unwrap();
    let admin_token = auth
        .login("root", "hunter222222")
        .await
        .unwrap()
        .access_token;
    let api = test_router(
        registry,
        auth,
        Arc::new(kahawai_hub::sessions::Sessions::new(
            tempfile::tempdir().unwrap().keep(),
        )),
    );

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
        serde_json::json!({"username": "tester", "password": "longenough12"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/auth/token",
            serde_json::json!({"client": "api", "username": "tester", "password": "longenough12"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let user_token = body_json(resp).await["access_token"]
        .as_str()
        .unwrap()
        .to_string();
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
    let resp = create(
        admin_token,
        serde_json::json!({"username": "shorty", "password": "short"}),
    )
    .await;
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
    auth.complete_setup("ingmar", "hunter222222").await.unwrap();
    let api = test_router(
        registry,
        auth.clone(),
        Arc::new(kahawai_hub::sessions::Sessions::new(
            tempfile::tempdir().unwrap().keep(),
        )),
    );

    for _ in 0..4 {
        let resp = api
            .clone()
            .oneshot(post(
                "/api/v1/auth/token",
                serde_json::json!({"client": "api", "username": "ingmar", "password": "wrong"}),
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
            serde_json::json!({"client": "api", "username": "ingmar", "password": "wrong"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // …so even the correct password is refused while locked.
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/auth/token",
            serde_json::json!({"client": "api", "username": "ingmar", "password": "hunter222222"}),
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
            serde_json::json!({"client": "api", "username": "someone-else", "password": "wrong"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// The client's first request answers which screen to open on, instead of
/// inferring it from an error status on an unrelated endpoint — which is
/// what made the web UI fetch the whole catalogue on every load.
#[tokio::test]
async fn bootstrap_states_setup_and_auth_without_a_token() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(Registry::new(db.clone(), Default::default()));
    let auth = Arc::new(Auth::new(db.clone(), dir.path()).await.unwrap());
    let api = test_router(
        registry,
        auth.clone(),
        Arc::new(kahawai_hub::sessions::Sessions::new(
            tempfile::tempdir().unwrap().keep(),
        )),
    );
    let probe = || {
        Request::get("/api/v1/bootstrap")
            .body(Body::empty())
            .unwrap()
    };

    // Reachable in setup mode, unlike everything behind require_auth.
    let resp = api.clone().oneshot(probe()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["setup_required"], true);
    assert_eq!(v["setup_available"], false);
    assert_eq!(v["setup_url"], "http://127.0.0.1:8422");
    assert_eq!(v["authenticated"], false);

    auth.complete_setup("ingmar", "hunter222222").await.unwrap();

    // Setup done, no token presented: the login screen, said plainly.
    let v = body_json(api.clone().oneshot(probe()).await.unwrap()).await;
    assert_eq!(v["setup_required"], false);
    assert_eq!(v["authenticated"], false);

    // A garbage token is not authentication.
    let resp = api
        .clone()
        .oneshot(get_authed("/api/v1/bootstrap", "not-a-jwt"))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["authenticated"], false);

    let token = body_json(
        api.clone()
            .oneshot(post("/api/v1/auth/token", serde_json::json!({"client": "api", "username": "ingmar", "password": "hunter222222"})))
            .await
            .unwrap(),
    )
    .await["access_token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = api
        .oneshot(get_authed("/api/v1/bootstrap", &token))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["authenticated"], true);
}

/// Expired refresh families are pruned when Auth opens; live ones stay.
#[tokio::test]
async fn expired_refresh_families_prune_at_open() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let auth = Auth::new(db.clone(), dir.path()).await.unwrap();
    auth.complete_setup("u", "hunter22222hunter").await.unwrap();
    sqlx::query(
        "INSERT INTO refresh_families (id, user_id, current_token_hash, expires_at)
         SELECT 'dead', id, 'dead-hash', unixepoch() - 1 FROM users LIMIT 1",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO refresh_families (id, user_id, current_token_hash, expires_at)
         SELECT 'live', id, 'live-hash', unixepoch() + 3600 FROM users LIMIT 1",
    )
    .execute(&db)
    .await
    .unwrap();

    let _ = Auth::new(db.clone(), dir.path()).await.unwrap();
    let left: Vec<String> =
        sqlx::query_scalar("SELECT id FROM refresh_families WHERE id IN ('dead','live')")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(left, ["live"], "expired pruned, live kept");
}

/// 0050 deliberately cannot preserve an opaque legacy token: it lacks the
/// family selector that makes replay identifiable after rotation.
#[tokio::test]
async fn refresh_family_migration_invalidates_legacy_tokens() {
    use sha2::{Digest, Sha256};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("hub.db");
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&db_path)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    AUTH_MIGRATOR.run_to(49, &pool).await.unwrap();

    sqlx::query(
        "INSERT INTO users (id, username, password_hash, is_admin)
         VALUES ('legacy-user', 'legacy', 'unused', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let legacy = "a-legacy-refresh-token";
    let hash = Sha256::digest(legacy.as_bytes());
    let hash = hash.iter().map(|b| format!("{b:02x}")).collect::<String>();
    sqlx::query(
        "INSERT INTO refresh_tokens (token_hash, user_id, expires_at)
         VALUES (?, 'legacy-user', unixepoch() + 3600)",
    )
    .bind(hash)
    .execute(&pool)
    .await
    .unwrap();

    AUTH_MIGRATOR.run(&pool).await.unwrap();
    let auth = Auth::new(pool.clone(), dir.path()).await.unwrap();
    assert!(matches!(
        auth.refresh(legacy).await,
        Err(kahawai_hub::auth::RefreshError::Invalid)
    ));
    let old_table: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_schema
          WHERE type = 'table' AND name = 'refresh_tokens'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(old_table, 0, "legacy token storage survived migration");
}

/// AUTH-2: only signed, unexpired HS256 access credentials minted by the
/// Kahawai hub for its client API cross the authentication boundary.
#[tokio::test]
async fn access_tokens_require_algorithm_signature_expiry_issuer_audience_and_type() {
    use jsonwebtoken::{Algorithm, EncodingKey, Header};
    use serde::Serialize;

    #[derive(Clone, Serialize)]
    struct TestClaims<'a> {
        sub: &'a str,
        username: &'a str,
        admin: bool,
        auth_version: i64,
        exp: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        iss: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        aud: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        token_type: Option<&'a str>,
    }

    let (dir, db, auth, _api, issued) = auth_harness().await;
    assert!(
        auth.authenticate(&issued.access_token).await.is_ok(),
        "Auth did not accept its own hardened access token"
    );

    let (user_id, auth_version): (String, i64) =
        sqlx::query_as("SELECT id, auth_version FROM users WHERE username = 'root'")
            .fetch_one(&db)
            .await
            .unwrap();
    let secret = std::fs::read(dir.path().join("jwt.secret")).unwrap();
    let sign = |claims: &TestClaims<'_>, algorithm: Algorithm, key: &[u8]| {
        jsonwebtoken::encode(
            &Header::new(algorithm),
            claims,
            &EncodingKey::from_secret(key),
        )
        .unwrap()
    };
    let valid = TestClaims {
        sub: &user_id,
        username: "untrusted-token-copy",
        admin: false,
        auth_version,
        exp: i64::MAX,
        iss: Some(kahawai_hub::auth::ACCESS_TOKEN_ISSUER),
        aud: Some(kahawai_hub::auth::ACCESS_TOKEN_AUDIENCE),
        token_type: Some(kahawai_hub::auth::ACCESS_TOKEN_TYPE),
    };

    for (name, claims) in [
        (
            "missing issuer",
            TestClaims {
                iss: None,
                ..valid.clone()
            },
        ),
        (
            "wrong issuer",
            TestClaims {
                iss: Some("urn:not-kahawai:hub"),
                ..valid.clone()
            },
        ),
        (
            "missing audience",
            TestClaims {
                aud: None,
                ..valid.clone()
            },
        ),
        (
            "wrong audience",
            TestClaims {
                aud: Some("urn:kahawai:not-the-api"),
                ..valid.clone()
            },
        ),
        (
            "missing token type",
            TestClaims {
                token_type: None,
                ..valid.clone()
            },
        ),
        (
            "wrong token type",
            TestClaims {
                token_type: Some("password-reset"),
                ..valid.clone()
            },
        ),
        (
            "expired",
            TestClaims {
                exp: 1,
                ..valid.clone()
            },
        ),
    ] {
        let token = sign(&claims, Algorithm::HS256, &secret);
        assert!(
            auth.authenticate(&token).await.is_err(),
            "accepted token with {name}"
        );
    }

    let wrong_signature = sign(&valid, Algorithm::HS256, b"a different signing secret");
    assert!(auth.authenticate(&wrong_signature).await.is_err());
    let wrong_algorithm = sign(&valid, Algorithm::HS384, &secret);
    assert!(
        auth.authenticate(&wrong_algorithm).await.is_err(),
        "algorithm outside the HS256 allowlist was accepted"
    );
}

/// AUTH-1: migration 52 deliberately creates a fresh credential generation.
/// Access tokens from the old shape do not decode, and existing refresh
/// families cannot mint replacements.
#[tokio::test]
async fn auth_version_migration_invalidates_existing_access_and_refresh() {
    use serde::Serialize;
    use sha2::{Digest, Sha256};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    #[derive(Serialize)]
    struct LegacyClaims<'a> {
        sub: &'a str,
        username: &'a str,
        admin: bool,
        exp: i64,
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hub.db");
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    AUTH_MIGRATOR.run_to(51, &pool).await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, is_admin)
         VALUES ('old-user', 'old', 'unused', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let secret = [7_u8; 32];
    std::fs::write(dir.path().join("jwt.secret"), secret).unwrap();
    let access = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &LegacyClaims {
            sub: "old-user",
            username: "old",
            admin: true,
            exp: i64::MAX,
        },
        &jsonwebtoken::EncodingKey::from_secret(&secret),
    )
    .unwrap();
    let refresh = "v1.0123456789abcdef0123456789abcdef.0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let refresh_hash = Sha256::digest(refresh.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    sqlx::query(
        "INSERT INTO refresh_families
            (id, user_id, current_token_hash, expires_at)
         VALUES ('0123456789abcdef0123456789abcdef', 'old-user', ?, unixepoch() + 3600)",
    )
    .bind(refresh_hash)
    .execute(&pool)
    .await
    .unwrap();

    AUTH_MIGRATOR.run(&pool).await.unwrap();
    let auth = Auth::new(pool.clone(), dir.path()).await.unwrap();
    assert!(auth.authenticate(&access).await.is_err());
    assert!(matches!(
        auth.refresh(refresh).await,
        Err(kahawai_hub::auth::RefreshError::Invalid)
    ));
    let row: (i64, bool) = sqlx::query_as(
        "SELECT auth_version, revoked_at IS NOT NULL FROM users
           JOIN refresh_families ON user_id = users.id
          WHERE users.id = 'old-user'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row, (1, true));
}

/// AUTH-4/5: one concurrent rotation wins. The loser is a presentation of
/// the now-consumed token, so it also revokes the winner's family.
#[tokio::test]
async fn concurrent_refresh_has_one_winner_and_revokes_replay_family() {
    let (_dir, db, auth, _api, _setup) = auth_harness().await;
    let contested = auth
        .login("root", "hunter22222hunter")
        .await
        .unwrap()
        .refresh_token;
    let separate = auth
        .login("root", "hunter22222hunter")
        .await
        .unwrap()
        .refresh_token;
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let run = |auth: Arc<Auth>, barrier: Arc<tokio::sync::Barrier>, token: String| {
        tokio::spawn(async move {
            barrier.wait().await;
            auth.refresh(&token).await
        })
    };
    let first = run(auth.clone(), barrier.clone(), contested.clone());
    let second = run(auth.clone(), barrier.clone(), contested);
    barrier.wait().await;
    let first = first.await.unwrap();
    let second = second.await.unwrap();
    assert_eq!(
        usize::from(first.is_ok()) + usize::from(second.is_ok()),
        1,
        "concurrent refresh did not have exactly one winner"
    );
    let winner = first.or(second).unwrap();
    assert!(matches!(
        auth.refresh(&winner.refresh_token).await,
        Err(kahawai_hub::auth::RefreshError::Invalid)
    ));
    assert!(
        auth.refresh(&separate).await.is_ok(),
        "replay revoked a separate login family"
    );
    let revoked: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM refresh_families WHERE revoked_at IS NOT NULL")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(revoked, 1);
}

/// API logout is authenticated, family-scoped, idempotent and deliberately
/// does not reveal whether a supplied refresh token was current or foreign.
#[tokio::test]
async fn api_logout_revokes_only_the_callers_current_family() {
    let (_dir, _db, auth, api, root) = auth_harness().await;
    auth.create_user("bob", "hunter22222hunter", false)
        .await
        .unwrap();
    assert_eq!(
        api.clone()
            .oneshot(post_authed(
                "/api/v1/auth/logout",
                &root.access_token,
                serde_json::json!({"client": "api"}),
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let bob = auth.login("bob", "hunter22222hunter").await.unwrap();

    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/auth/logout",
            serde_json::json!({"client": "api", "refresh_token": root.refresh_token}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = api
        .clone()
        .oneshot(post_authed(
            "/api/v1/auth/logout",
            &root.access_token,
            serde_json::json!({"client": "api", "refresh_token": bob.refresh_token}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(set_cookies(&resp).is_empty());
    let bob = auth.refresh(&bob.refresh_token).await.unwrap();

    for _ in 0..2 {
        let resp = api
            .clone()
            .oneshot(post_authed(
                "/api/v1/auth/logout",
                &bob.access_token,
                serde_json::json!({"client": "api", "refresh_token": bob.refresh_token}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(set_cookies(&resp).is_empty());
    }
    assert!(matches!(
        auth.refresh(&bob.refresh_token).await,
        Err(kahawai_hub::auth::RefreshError::Invalid)
    ));
    assert!(
        auth.refresh(&root.refresh_token).await.is_ok(),
        "logging out bob revoked root's family"
    );
}

/// The CLI path and the running hub share the transactionally persisted
/// family state: every existing login remains revoked after Auth reopens.
#[tokio::test]
async fn password_reset_revokes_all_families_across_restart() {
    let (dir, db, auth, _api, setup) = auth_harness().await;
    let second = auth.login("root", "hunter22222hunter").await.unwrap();
    // A distinct pool is the separate CLI process: no Auth state is shared with
    // the running hub, only the durable database transaction.
    let cli_db = kahawai_hub::db::open(dir.path()).await.unwrap();
    kahawai_hub::auth::reset_password(&cli_db, "root", "new-password-22")
        .await
        .unwrap();
    cli_db.close().await;

    for access in [&setup.access_token, &second.access_token] {
        assert!(
            auth.authenticate(access).await.is_err(),
            "separate-process reset left running-hub access valid"
        );
    }
    let restarted = Auth::new(db.clone(), dir.path()).await.unwrap();
    for access in [&setup.access_token, &second.access_token] {
        assert!(
            restarted.authenticate(access).await.is_err(),
            "access invalidation did not survive restart"
        );
    }
    for token in [&setup.refresh_token, &second.refresh_token] {
        assert!(matches!(
            restarted.refresh(token).await,
            Err(kahawai_hub::auth::RefreshError::Invalid)
        ));
    }
    assert!(
        restarted
            .login("root", "new-password-22")
            .await
            .unwrap()
            .refresh_token
            .starts_with("v1.")
    );
    let revoked: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM refresh_families WHERE revoked_at IS NOT NULL")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(revoked, 2);
}

/// HUB-10: deleting an account removes what is theirs, refuses the two
/// deletions that would lock the operator out, and takes effect at once
/// rather than whenever their access token happens to expire.
#[tokio::test]
async fn delete_racing_demotion_keeps_an_admin() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let auth = Arc::new(Auth::new(db.clone(), dir.path()).await.unwrap());

    // Re-run the actual race, not merely its two statements in a chosen order.
    // Before the guarded delete this could let each operation observe two
    // administrators and then independently remove one.
    for round in 0..30 {
        sqlx::query("DELETE FROM users").execute(&db).await.unwrap();
        for id in [format!("a-{round}"), format!("b-{round}")] {
            sqlx::query(
                "INSERT INTO users (id, username, password_hash, is_admin)
                 VALUES (?, ?, 'unused', 1)",
            )
            .bind(&id)
            .bind(&id)
            .execute(&db)
            .await
            .unwrap();
        }

        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let delete = {
            let auth = auth.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                auth.delete_user(&format!("a-{round}")).await.unwrap()
            })
        };
        let demote = {
            let auth = auth.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                auth.set_admin(&format!("b-{round}"), false).await.unwrap()
            })
        };
        barrier.wait().await;
        let deleted = delete.await.unwrap();
        let demoted = demote.await.unwrap();
        assert!(
            matches!(deleted, kahawai_hub::auth::DeleteUser::Deleted(_))
                ^ matches!(demoted, kahawai_hub::auth::SetAdmin::Changed),
            "round {round}: exactly one admin-removing operation must win: {deleted:?}, {demoted:?}"
        );
        let admins: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE is_admin = 1")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(admins, 1, "round {round} orphaned the hub");
    }
}

#[tokio::test]
async fn admin_deletes_users() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(Registry::new(db.clone(), Default::default()));
    let auth = Arc::new(Auth::new(db.clone(), dir.path()).await.unwrap());
    auth.complete_setup("root", "hunter222222").await.unwrap();
    let admin_token = auth
        .login("root", "hunter222222")
        .await
        .unwrap()
        .access_token;
    let api = test_router(
        registry,
        auth.clone(),
        Arc::new(kahawai_hub::sessions::Sessions::new(
            tempfile::tempdir().unwrap().keep(),
        )),
    );
    let admin_id: String = sqlx::query_scalar("SELECT id FROM users WHERE username = 'root'")
        .fetch_one(&db)
        .await
        .unwrap();

    let victim = auth
        .create_user("bob", "hunter222222", false)
        .await
        .unwrap();
    // Everything a user owns: a live token, watch state, a preference,
    // and an archived row that has no foreign key to hold it down.
    let bob = auth
        .login("bob", "hunter222222")
        .await
        .unwrap()
        .access_token;
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
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,module_id,collection_id)
                 VALUES('i1','movie','M','m','fixture','default')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO watch_state (user_id, item_id, position_ms) VALUES (?, 'i1', 5)")
        .bind(&victim)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_prefs (user_id, scope, key, value) VALUES (?, 's', 'k', 'v')")
        .bind(&victim)
        .execute(&db)
        .await
        .unwrap();
    // Keyed to content identity (MH-5), not to an item.
    sqlx::query(
        "INSERT INTO watch_state_archive
           (user_id, size, head_xxh3, tail_xxh3, position_ms, played, play_count)
         VALUES (?, 1, 2, 3, 5, 0, 0)",
    )
    .bind(&victim)
    .execute(&db)
    .await
    .unwrap();

    let del = |token: String, id: String| {
        let api = api.clone();
        async move {
            api.oneshot(
                Request::delete(format!("/admin/v1/users/{id}"))
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };

    // A user cannot delete anyone — the admin router refuses first.
    assert_eq!(
        del(bob.clone(), admin_id.clone()).await.status(),
        StatusCode::FORBIDDEN
    );
    // Nor may an admin delete themselves: it would revoke the token
    // mid-request, and for the only admin there is no way back.
    //
    // CONFLICT, not FORBIDDEN — which is the point of the pair. The assertion
    // above is `require_admin` turning away a token that is not an admin at
    // all; this is an admin whose request is refused by the state. Sharing one
    // status meant a client could not tell "re-authenticate" from "pick a
    // different account".
    assert_eq!(
        del(admin_token.clone(), admin_id.clone()).await.status(),
        StatusCode::CONFLICT
    );
    // A token minted while bob was an admin dies with the demotion's durable
    // generation bump. It cannot use a stale role to reach any admin action.
    auth.set_admin(&victim, true).await.unwrap();
    let bob_admin_token = auth
        .login("bob", "hunter222222")
        .await
        .unwrap()
        .access_token;
    auth.set_admin(&victim, false).await.unwrap();
    assert_eq!(
        del(bob_admin_token, admin_id.clone()).await.status(),
        StatusCode::UNAUTHORIZED,
        "a demoted account retained admin access"
    );

    assert_eq!(
        del(admin_token.clone(), "nobody".into()).await.status(),
        StatusCode::NOT_FOUND
    );

    assert_eq!(
        del(admin_token.clone(), victim.clone()).await.status(),
        StatusCode::OK
    );

    // Everything of bob's is gone. Refresh families cascade from users;
    // watch_state_archive has no foreign key and is deleted by hand, since
    // the satellite-restore path would otherwise copy its orphan back.
    let count = |sql: &'static str| {
        let db = db.clone();
        let victim = victim.clone();
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .bind(&victim)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    assert_eq!(count("SELECT COUNT(*) FROM users WHERE id = ?").await, 0);
    assert_eq!(
        count("SELECT COUNT(*) FROM refresh_families WHERE user_id = ?").await,
        0
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM watch_state WHERE user_id = ?").await,
        0
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM user_prefs WHERE user_id = ?").await,
        0
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM watch_state_archive WHERE user_id = ?").await,
        0,
        "archived rows outlived the user and would break a later restore"
    );

    // The token bob is holding verifies fine — right signature, not yet
    // expired. It must stop working anyway.
    let resp = api
        .clone()
        .oneshot(
            Request::get("/api/v1/items")
                .header("authorization", format!("Bearer {bob}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "a deleted user's live access token still worked"
    );
    let restarted = Auth::new(db.clone(), dir.path()).await.unwrap();
    assert!(
        restarted.authenticate(&bob).await.is_err(),
        "deleted-user invalidation did not survive Auth restart"
    );

    // And the last admin is refused, so a hub cannot be orphaned: an
    // emptied one does not fall back into setup mode until it restarts.
    let second = auth
        .create_user("root2", "hunter222222", true)
        .await
        .unwrap();
    assert_eq!(
        del(admin_token.clone(), second).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        auth.delete_user(&admin_id).await.unwrap(),
        kahawai_hub::auth::DeleteUser::LastAdmin,
        "the last admin was deletable"
    );
}
