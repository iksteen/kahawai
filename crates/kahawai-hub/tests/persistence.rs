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
        let reg = Registry::new(db.clone());
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
    let reg = Arc::new(Registry::new(db.clone()));

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
    let api = kahawai_hub::api::router(reg.clone(), auth, Arc::new(kahawai_hub::sessions::Sessions::default()));
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
