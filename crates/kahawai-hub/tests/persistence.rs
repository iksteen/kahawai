//! HUB-13/NFR-3: state survives a hub restart; movie resolution dedups
//! sources onto one item (HUB-3/4); the browse API serves it all.

use std::sync::Arc;

use kahawai_hub::registry::{FileUpsertRecord, Registry};
use tower::ServiceExt;

fn rec(path: &str, size: u64) -> FileUpsertRecord {
    FileUpsertRecord {
        path_rel: path.into(),
        size,
        mtime_unix: 1,
        head_xxh3: 1,
        tail_xxh3: 2,
        oshash: 3,
        streams_json: r#"{"container":"matroska"}"#.into(),
    }
}

#[tokio::test]
async fn files_and_items_survive_restart() {
    let dir = tempfile::tempdir().unwrap();

    {
        let db = kahawai_hub::db::open(dir.path()).await.unwrap();
        let reg = Registry::new(db.clone(), Default::default());
        reg.announce_collection("01H", "movies", "movies", &[]).await.unwrap();
        reg.upsert_files(
            "01H",
            "movies",
            vec![
                // Same movie, two qualities → one item, two sources.
                rec("Heat (1995)/Heat.1995.2160p.mkv", 100),
                rec("Heat.1995.1080p.BluRay.x264-GRP.mkv", 50),
                rec("Ronin (1998).mkv", 60),
            ],
        )
        .await
        .unwrap();
        db.close().await;
    }

    // "Restart": fresh pool over the same directory.
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let reg = Arc::new(Registry::new(db.clone(), Default::default()));

    // The DB (password hashes, sessions) must not be world-readable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("hub.db")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "hub.db must be 0600");
    }

    let cols = reg.collections().await.unwrap();
    assert_eq!(cols.len(), 1);
    assert_eq!(cols[0].file_count, 3);
    assert!(!cols[0].available, "no mediahost connected after restart");

    let titles: Vec<(String, Option<i64>, i64)> = sqlx::query_as(
        "SELECT i.title, i.year, COUNT(s.item_id) FROM items i
         JOIN item_sources s ON s.item_id = i.id
         GROUP BY i.id ORDER BY i.title",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(
        titles,
        vec![("Heat".into(), Some(1995), 2), ("Ronin".into(), Some(1998), 1)]
    );

    // Browse API over the same state (setup + login first).
    let auth = Arc::new(kahawai_hub::auth::Auth::new(db.clone(), dir.path()).await.unwrap());
    let token = auth.setup_token().unwrap();
    let pair = auth.complete_setup(&token, "admin", "password-123").await.unwrap();
    let bearer = format!("Bearer {}", pair.access_token);
    let get = |uri: String| {
        axum::http::Request::get(uri)
            .header("authorization", bearer.clone())
            .body(axum::body::Body::empty())
            .unwrap()
    };
    let api = test_router(reg.clone(), auth, Arc::new(kahawai_hub::sessions::Sessions::new(tempfile::tempdir().unwrap().keep())));
    let resp = api
        .clone()
        .oneshot(get("/api/v1/items".into()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["title"], "Heat");
    assert_eq!(items[0]["sources"], 2);

    // Detail includes sources with parsed stream info and availability.
    let id = items[0]["id"].as_str().unwrap();
    let resp = api
        .clone()
        .oneshot(get(format!("/api/v1/items/{id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let sources = json["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0]["size"], 100, "sources ranked by size");
    assert_eq!(sources[0]["streams"]["container"], "matroska");
    assert_eq!(sources[0]["available"], false);

    // Unknown item → 404.
    let resp = api.oneshot(get("/api/v1/items/nope".into())).await.unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn reconcile_drops_files_missing_from_scan() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let reg = Registry::new(db.clone(), Default::default());
    reg.announce_collection("01H", "movies", "movies", &[]).await.unwrap();
    reg.upsert_files(
        "01H",
        "movies",
        vec![rec("Heat (1995).mkv", 100), rec("Ronin (1998).mkv", 50)],
    )
    .await
    .unwrap();

    // A user watched Ronin; its state must die with the item.
    sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ('u1','u','x')")
        .execute(&db)
        .await
        .unwrap();
    let ronin: String = sqlx::query_scalar("SELECT id FROM items WHERE title = 'Ronin'")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO watch_state (user_id, item_id, position_ms) VALUES ('u1', ?, 1234)")
        .bind(&ronin)
        .execute(&db)
        .await
        .unwrap();

    // Rescan saw only Heat.
    let seen: std::collections::HashSet<String> = ["Heat (1995).mkv".to_string()].into();
    let removed = reg.reconcile_files("01H", "movies", &seen).await.unwrap();
    assert_eq!(removed, 1);

    let files: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM files").fetch_one(&db).await.unwrap();
    let items: Vec<String> = sqlx::query_scalar("SELECT title FROM items").fetch_all(&db).await.unwrap();
    let watch: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM watch_state").fetch_one(&db).await.unwrap();
    assert_eq!(files, 1);
    assert_eq!(items, vec!["Heat".to_string()]);
    assert_eq!(watch, 0, "watch state cascades with the removed item");

    // Idempotent when nothing changed.
    assert_eq!(reg.reconcile_files("01H", "movies", &seen).await.unwrap(), 0);
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
    kahawai_hub::api::router(registry, auth, sessions, enrollments)
}
