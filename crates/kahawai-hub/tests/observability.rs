//! NFR-6: health and metrics.
//!
//! The point of these is to be reachable and truthful when something is
//! wrong, so what is asserted is the contract a monitor depends on:
//! health answers without a credential, metrics does not, and a module
//! that is away is visible as such rather than collapsing the whole
//! server into "down".

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use std::sync::Arc;

/// Same shape the other API tests build; kept local rather than shared
/// so this file has no reason to be edited when they change.
async fn harness() -> (axum::Router, String) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(kahawai_hub::registry::Registry::new(db.clone(), Default::default()));
    let auth = Arc::new(kahawai_hub::auth::Auth::new(db, dir.path()).await.unwrap());
    let sessions =
        Arc::new(kahawai_hub::sessions::Sessions::new(tempfile::tempdir().unwrap().keep()));
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
        Arc::new(kahawai_hub::subtitles::Subtitles::new(tempfile::tempdir().unwrap().keep())),
        Arc::new(kahawai_hub::artwork::Artwork::new(
            tempfile::tempdir().unwrap().keep(),
            enricher.clone(),
        )),
        enricher,
        kahawai_hub::api::NetOptions::default(),
    );
    let token = auth
        .complete_setup(
            &auth.setup_token().unwrap(),
            "admin",
            "hunter22222hunter",
        )
        .await
        .unwrap();
    std::mem::forget(dir); // the router holds paths under it
    (api, token.access_token)
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test]
async fn health_answers_without_a_credential_and_metrics_does_not() {
    let (api, token) = harness().await;

    // A load balancer or uptime check holds no token. If this ever needs
    // one, every such check starts failing silently at the worst moment.
    let resp = api
        .clone()
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
    assert_eq!(v["status"], "ok", "a hub with no satellites is not unhealthy");
    assert!(v["version"].is_string());

    // Metrics report library scale and module names, so they are not
    // public. Prometheus scrapes with a bearer token.
    let resp = api
        .clone()
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = api
        .clone()
        .oneshot(
            Request::get("/metrics")
                .header("authorization", format!("Bearer {}", token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = body_text(resp).await;
    // Prometheus rejects a body without TYPE/HELP pairing, and a name
    // typo is invisible until a dashboard is silently empty.
    for name in [
        "kahawai_build_info",
        "kahawai_sessions_active",
        "kahawai_items",
        "kahawai_files",
        "kahawai_items_unmatched",
    ] {
        assert!(text.contains(&format!("# TYPE {name} ")), "{name} has no TYPE line");
        assert!(text.contains(&format!("# HELP {name} ")), "{name} has no HELP line");
    }
    for line in text.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
        assert!(
            line.rsplit(' ').next().unwrap().parse::<f64>().is_ok(),
            "not a number: {line}"
        );
    }
}
