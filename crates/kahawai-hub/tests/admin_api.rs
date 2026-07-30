//! Admin surface (SEC-3/6, HUB-20): admin-gated routes, enrollment
//! approval over HTTP, satellite deletion with revocation + cascade, and
//! watch-state archive/restore keyed to content identity.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use kahawai_hub::auth::Auth;
use kahawai_hub::enrollment_service::EnrollmentService;
use kahawai_hub::pki::HubCa;
use kahawai_hub::registry::{FileUpsertRecord, Registry};
use kahawai_proto::v1::enrollment_server::Enrollment as _;
use kahawai_transport::mtls::AllowedCerts;
use sqlx::Row;
use tower::ServiceExt;

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let b = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&b).unwrap_or(serde_json::Value::Null)
}

fn rec(path: &str, size: u64, head: u64, tail: u64) -> FileUpsertRecord {
    FileUpsertRecord {
        path_rel: path.into(),
        size,
        mtime_unix: 1,
        head_xxh3: head,
        tail_xxh3: tail,
        oshash: 9,
        streams_json: r#"{"container":"matroska"}"#.into(),
    }
}

#[tokio::test]
async fn admin_flow_enrollments_satellites_archive_restore() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let ca = Arc::new(HubCa::load_or_create(dir.path()).unwrap());
    let allowed = AllowedCerts::default();
    let registry = Arc::new(Registry::new(db.clone(), allowed.clone()));
    let sessions = Arc::new(kahawai_hub::sessions::Sessions::new(
        tempfile::tempdir().unwrap().keep(),
    ));
    let enrollments = Arc::new(EnrollmentService::new(
        ca.clone(),
        registry.clone(),
        Duration::from_secs(900),
        90,
    ));
    let auth = Arc::new(Auth::new(db.clone(), dir.path()).await.unwrap());
    let pair = auth
        .complete_setup(&auth.setup_token().unwrap(), "admin", "password-123")
        .await
        .unwrap();
    let admin_bearer = format!("Bearer {}", pair.access_token);

    // A non-admin user, created directly (no user management API yet).
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, is_admin) VALUES ('u2','pleb',?,0)",
    )
    .bind(kahawai_hub::auth::hash_password("pleb-password").unwrap())
    .execute(&db)
    .await
    .unwrap();
    let pleb = auth.login("pleb", "pleb-password").await.unwrap();
    let pleb_bearer = format!("Bearer {}", pleb.access_token);

    let api = kahawai_hub::api::router(
        registry.clone(),
        auth,
        sessions,
        enrollments.clone(),
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
        kahawai_hub::api::NetOptions::default(),
    );
    let get = |uri: &str, bearer: &str| {
        Request::get(uri)
            .header("authorization", bearer.to_string())
            .body(Body::empty())
            .unwrap()
    };

    // Admin gate: pleb is refused, admin passes.
    let resp = api
        .clone()
        .oneshot(get("/admin/v1/satellites", &pleb_bearer))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = api
        .clone()
        .oneshot(get("/admin/v1/satellites", &admin_bearer))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Enrollment via HTTP: submit a CSR (gRPC surface, called in-process),
    // see it listed, approve it with the console code.
    let bundle = kahawai_core::pki::new_satellite_csr("mediahost", "01ADM", "nas").unwrap();
    enrollments
        .submit(tonic::Request::new(kahawai_proto::v1::SubmitRequest {
            csr_der: bundle.csr_der.clone(),
        }))
        .await
        .unwrap();
    let v = body_json(
        api.clone()
            .oneshot(get("/admin/v1/enrollments", &admin_bearer))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(v["pending"][0]["module_id"], "01ADM");

    let wrong = api
        .clone()
        .oneshot(
            Request::post("/admin/v1/enrollments/approve")
                .header("authorization", admin_bearer.clone())
                .header("content-type", "application/json")
                .body(Body::from(r#"{"code":"AAAA-AAAA"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::FORBIDDEN);

    // Resubmit (wrong code rejected the sole pending CSR per §7.2).
    enrollments
        .submit(tonic::Request::new(kahawai_proto::v1::SubmitRequest {
            csr_der: bundle.csr_der.clone(),
        }))
        .await
        .unwrap();
    let code = kahawai_core::enroll::enrollment_code(&bundle.csr_der);
    let resp = api
        .clone()
        .oneshot(
            Request::post("/admin/v1/enrollments/approve")
                .header("authorization", admin_bearer.clone())
                .header("content-type", "application/json")
                .body(Body::from(format!("{{\"code\":\"{code}\"}}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The satellite row exists and shows disconnected.
    let v = body_json(
        api.clone()
            .oneshot(get("/admin/v1/satellites", &admin_bearer))
            .await
            .unwrap(),
    )
    .await;
    let sat = &v["satellites"][0];
    assert_eq!(sat["module_id"], "01ADM");
    assert_eq!(sat["connected"], false);
    let fp = sat["cert_fingerprint"].as_str().unwrap().to_string();
    assert!(
        allowed.contains(&fp),
        "approval must admit the cert (SEC-5)"
    );

    // Give it a collection, a file, and admin watch state on the item.
    registry
        .announce_collection("01ADM", "movies", "movies", &[])
        .await
        .unwrap();
    registry
        .upsert_files("01ADM", "movies", vec![rec("Heat (1995).mkv", 100, 11, 22)])
        .await
        .unwrap();
    let item: String = sqlx::query_scalar("SELECT id FROM items")
        .fetch_one(&db)
        .await
        .unwrap();
    let admin_id: String = sqlx::query_scalar("SELECT id FROM users WHERE username = 'admin'")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO watch_state (user_id, item_id, position_ms, played, play_count)
         VALUES (?, ?, 4321, 1, 2)",
    )
    .bind(&admin_id)
    .bind(&item)
    .execute(&db)
    .await
    .unwrap();

    // DELETE the satellite: revoked + cascaded + archived.
    let resp = api
        .clone()
        .oneshot(
            Request::delete("/admin/v1/satellites/01ADM")
                .header("authorization", admin_bearer.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        !allowed.contains(&fp),
        "deletion must remove the cert from the allowlist (SEC-6)"
    );
    let counts: (i64, i64, i64, i64) = (
        sqlx::query_scalar("SELECT COUNT(*) FROM files")
            .fetch_one(&db)
            .await
            .unwrap(),
        sqlx::query_scalar("SELECT COUNT(*) FROM items")
            .fetch_one(&db)
            .await
            .unwrap(),
        sqlx::query_scalar("SELECT COUNT(*) FROM watch_state")
            .fetch_one(&db)
            .await
            .unwrap(),
        sqlx::query_scalar("SELECT COUNT(*) FROM watch_state_archive")
            .fetch_one(&db)
            .await
            .unwrap(),
    );
    assert_eq!(
        counts,
        (0, 0, 0, 1),
        "cascade deleted, watch state archived"
    );
    let audit: Vec<String> = sqlx::query_scalar("SELECT action FROM satellite_audit ORDER BY id")
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(
        audit,
        vec!["enrolled".to_string(), "deleted".to_string()],
        "audit trail"
    );

    // The same bytes return on a DIFFERENT host: watch state restored.
    registry
        .announce_collection("01NEW", "movies", "movies", &[])
        .await
        .unwrap();
    registry
        .upsert_files(
            "01NEW",
            "movies",
            vec![rec("moved/Heat 1995.mkv", 100, 11, 22)],
        )
        .await
        .unwrap();
    let restored: (i64, i64) =
        sqlx::query_as("SELECT position_ms, play_count FROM watch_state WHERE user_id = ?")
            .bind(&admin_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        restored,
        (4321, 2),
        "watch state restored by content identity"
    );
    let archived: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM watch_state_archive")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(archived, 0, "archive row consumed");
}

/// HUB-8: the review queue lists misses/weak, manual picks stick,
/// confirm promotes, reject clears and stays out of auto-retries.
#[tokio::test]
async fn review_queue_flow() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(Registry::new(db.clone(), Default::default()));
    let auth = Arc::new(
        kahawai_hub::auth::Auth::new(db.clone(), dir.path())
            .await
            .unwrap(),
    );
    let setup = auth.setup_token().unwrap();
    let sessions = Arc::new(kahawai_hub::sessions::Sessions::new(
        tempfile::tempdir().unwrap().keep(),
    ));
    let ca =
        Arc::new(HubCa::load_or_create(tempfile::tempdir().unwrap().keep().as_path()).unwrap());
    let enr = Arc::new(EnrollmentService::new(
        ca,
        registry.clone(),
        std::time::Duration::from_secs(60),
        90,
    ));
    let api = kahawai_hub::api::router(
        registry.clone(),
        auth,
        sessions,
        enr,
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
        kahawai_hub::api::NetOptions::default(),
    );
    let resp = api
        .clone()
        .oneshot(
            axum::http::Request::post("/api/v1/setup")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({"token": setup, "username": "a", "password": "password-1"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let token = body_json(resp).await["access_token"]
        .as_str()
        .unwrap()
        .to_string();

    // Two movie items with metadata states: one miss, one weak.
    registry
        .record_satellite("01HOST", "mediahost", "nas", "fp")
        .await
        .unwrap();
    registry
        .announce_collection("01HOST", "movies", "movies", &["/srv/m".into()])
        .await
        .unwrap();
    registry
        .upsert_files(
            "01HOST",
            "movies",
            vec![
                rec("Foobar.mkv", 10, 1, 1),
                rec("Weakling (1999).mkv", 11, 2, 2),
            ],
        )
        .await
        .unwrap();
    let miss_id: String = sqlx::query_scalar("SELECT id FROM items WHERE title = 'Foobar'")
        .fetch_one(&db)
        .await
        .unwrap();
    let weak_id: String = sqlx::query_scalar("SELECT id FROM items WHERE title = 'Weakling'")
        .fetch_one(&db)
        .await
        .unwrap();
    // The new model: the provider's own answer, plus an assignment when it
    // actually matched something. A miss is an answer with no record.
    for (id, pid, conf) in [(&miss_id, "", "miss"), (&weak_id, "42", "weak")] {
        sqlx::query(
            "INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence, updated_at)
             VALUES (?, 'tmdb', ?, 'Guess', ?, 0)",
        )
        .bind(id)
        .bind(pid)
        .bind(conf)
        .execute(&db)
        .await
        .unwrap();
    }

    let authed = |method: &str, uri: String, body: Option<serde_json::Value>| {
        let mut b = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"));
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        b.body(axum::body::Body::from(
            body.map(|v| v.to_string()).unwrap_or_default(),
        ))
        .unwrap()
    };

    let v = body_json(
        api.clone()
            .oneshot(authed("GET", "/admin/v1/enrich/review".into(), None))
            .await
            .unwrap(),
    )
    .await;
    let entries = v["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "{v}");
    assert_eq!(entries[0]["confidence"], "miss"); // misses sort first
    assert_eq!(entries[1]["confidence"], "weak");

    // Pick a candidate for the miss.
    let resp = api
        .clone()
        .oneshot(authed(
            "POST",
            format!("/admin/v1/items/{miss_id}/match"),
            Some(serde_json::json!({
                "action": "pick",
                "provider": "tmdb",
                "candidate": {"id": 603, "title": "The Matrix", "release_date": "1999-03-30"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    // Confirm the weak one; then reject it again.
    for action in ["confirm", "reject"] {
        let resp = api
            .clone()
            .oneshot(authed(
                "POST",
                format!("/admin/v1/items/{weak_id}/match"),
                Some(serde_json::json!({ "action": action })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK, "{action}");
    }

    let states: Vec<(String, String)> = sqlx::query(
        "SELECT item_id, confidence FROM resolved_metadata
                      WHERE confidence IS NOT NULL ORDER BY item_id",
    )
    .fetch_all(&db)
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.get("item_id"), r.get("confidence")))
    .collect();
    let get = |id: &str| {
        states
            .iter()
            .find(|(i, _)| i == id)
            .map(|(_, c)| c.as_str())
            .unwrap()
    };
    assert_eq!(get(&miss_id), "manual");
    assert_eq!(get(&weak_id), "rejected");
    // The picked title shows through the display API.
    let v = body_json(
        api.clone()
            .oneshot(authed("GET", format!("/api/v1/items/{miss_id}"), None))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(v["title"], "The Matrix");
    assert_eq!(v["year"], 1999);
}
