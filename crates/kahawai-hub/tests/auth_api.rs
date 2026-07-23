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
    let registry = Arc::new(Registry::new(db.clone()));
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
    kahawai_hub::api::router(registry, auth, sessions, enrollments, Default::default())
}
