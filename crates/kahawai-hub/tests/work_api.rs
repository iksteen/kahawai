//! `/admin/v1/work`: every background queue in one shape, admin only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use kahawai_hub::auth::Auth;
use kahawai_hub::registry::Registry;
use kahawai_proto::v1 as p;
use prost::Message;
use tower::ServiceExt;

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// A catalogue with one probed movie, so both hub queues have a row and
/// the mediahost has a collection to report on.
async fn store_with_a_film() -> kahawai_mediadb::Store {
    let store = kahawai_mediadb::Store::in_memory().await.unwrap();
    store.put_mediahost("host", "NAS").await.unwrap();
    store
        .offer_collection(
            "host",
            &p::CatalogCollection {
                id: "films".into(),
                media_type: "movies".into(),
                epoch: "epoch".into(),
                current_version: 1,
                roots: vec![p::CollectionRoot::new("root", "/fixture")],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    store
        .apply_catalogue(
            "host",
            &p::CatalogDelta {
                collection_id: "films".into(),
                epoch: "epoch".into(),
                snapshot: true,
                done: true,
                through_version: 1,
                records: vec![p::CatalogRecord {
                    version: 1,
                    kind: "file".into(),
                    key: b"root\0Film (2000).mkv".to_vec(),
                    payload: p::FileUpsert {
                        collection_id: "films".into(),
                        files: vec![p::FileRecord {
                            source: Some(p::SourcePath::new("root", "Film (2000).mkv")),
                            size: 10,
                            mtime_unix: 1,
                            streams_json: serde_json::json!({"container":"mkv"}).to_string(),
                            ..Default::default()
                        }],
                    }
                    .encode_to_vec(),
                    deleted: false,
                }],
            },
        )
        .await
        .unwrap();
    // Enrichment rows exist only for collections a library composes.
    let collection = store.collections("host").await.unwrap().remove(0).id;
    store
        .create_library("Movies", kahawai_mediadb::MediaType::Movies, &[collection])
        .await
        .unwrap();
    store
}

async fn harness() -> (
    tempfile::TempDir,
    Arc<Registry>,
    axum::Router,
    String,
    String,
) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(Registry::new(
        db.clone(),
        Default::default(),
        store_with_a_film().await,
    ));
    let auth = Arc::new(Auth::new(db.clone(), dir.path()).await.unwrap());
    auth.complete_setup("root", "hunter222222").await.unwrap();
    let admin = auth
        .login("root", "hunter222222")
        .await
        .unwrap()
        .access_token;
    auth.create_user("viewer", "longenough12", false)
        .await
        .unwrap();
    let viewer = auth
        .login("viewer", "longenough12")
        .await
        .unwrap()
        .access_token;
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
    let enricher = Arc::new(kahawai_hub::enrich::Enricher::new(
        tempfile::tempdir().unwrap().keep(),
    ));
    let net = kahawai_hub::api::NetOptions {
        setup_url: Some("http://127.0.0.1:8422".into()),
        ..Default::default()
    };
    let api = kahawai_hub::api::router(
        registry.clone(),
        auth,
        Arc::new(kahawai_hub::sessions::Sessions::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        enrollments,
        Arc::new(kahawai_hub::subtitles::Subtitles::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        Arc::new(kahawai_hub::artwork::Artwork::new(
            tempfile::tempdir().unwrap().keep(),
            enricher.clone(),
        )),
        enricher,
        net,
    );
    (dir, registry, api, admin, viewer)
}

fn get(token: &str) -> Request<Body> {
    Request::get("/admin/v1/work")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn rerun(token: &str, body: serde_json::Value) -> Request<Body> {
    Request::post("/admin/v1/work/rerun")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn every_queue_is_listed_in_one_shape_for_administrators_only() {
    let (_dir, registry, api, admin, viewer) = harness().await;
    assert_eq!(
        api.clone().oneshot(get(&viewer)).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    // A mediahost has reported on the collection: discovery rows appear
    // beside the hub's own queues.
    registry
        .ensure_local_satellite("host", "NAS")
        .await
        .unwrap();
    registry.connected("host", "mediahost", "NAS", "fp", "test");
    let generation = registry
        .register_link(
            "host",
            tokio::sync::mpsc::channel(1).0,
            kahawai_proto::PROTOCOL_MINOR,
            0,
        )
        .0;
    registry.report_discovery(
        "host",
        generation,
        p::DiscoveryStatus {
            collection_id: "films".into(),
            scanning: true,
            pending_cheap: 3,
            pending_segments: 2,
            segments_enabled: Some(false),
            ..Default::default()
        },
    );

    let resp = api.clone().oneshot(get(&admin)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let queues = body_json(resp).await["queues"].as_array().unwrap().clone();
    let find = |area: &str, queue: &str| {
        queues
            .iter()
            .find(|q| q["area"] == area && q["queue"] == queue)
            .cloned()
            .unwrap_or_else(|| panic!("{area}/{queue} listed"))
    };
    let text = find("subtitles", "text");
    assert_eq!(text["pending"], 1);
    assert_eq!(text["rerun"], true);
    assert_eq!(find("subtitles", "sets")["pending"], 1);
    assert_eq!(find("subtitles", "ocr")["rerun"], true);
    let tmdb = find("enrichment", "tmdb");
    assert_eq!(tmdb["pending"], 1);
    assert_eq!(tmdb["rerun"], true);
    let scan = find("discovery", "scan");
    assert_eq!(scan["running"], 1);
    assert_eq!(scan["host"], "NAS");
    assert_eq!(scan["collection"], "films");
    assert_eq!(scan["rerun"], false);
    assert_eq!(find("discovery", "cheap")["pending"], 3);
    assert!(
        !queues
            .iter()
            .any(|q| q["area"] == "discovery" && q["queue"] == "segments"),
        "detection disabled on the host is not a queue"
    );
}

#[tokio::test]
async fn a_rerun_releases_hub_queues_and_refuses_discovery() {
    let (_dir, registry, api, admin, _viewer) = harness().await;
    let store = registry.catalogue();
    // Six reported failures park the text row.
    let source = kahawai_proto::v1::SourcePath::new("root", "Film (2000).mkv");
    for round in 1..=kahawai_mediadb::BLOCK_AFTER_ATTEMPTS {
        assert_eq!(
            store
                .claim_subtitle_jobs("text", "host", &[], round * 100, 1, 1)
                .await
                .unwrap()
                .len(),
            1
        );
        store
            .fail_subtitle_source("host", "films", &source, "text", 0, "demux failed")
            .await
            .unwrap();
    }
    let blocked = |queues: &[serde_json::Value]| {
        queues
            .iter()
            .find(|q| q["area"] == "subtitles" && q["queue"] == "text")
            .unwrap()["blocked"]
            .clone()
    };
    let before = body_json(api.clone().oneshot(get(&admin)).await.unwrap()).await;
    assert_eq!(blocked(before["queues"].as_array().unwrap()), 1);

    let resp = api
        .clone()
        .oneshot(rerun(
            &admin,
            serde_json::json!({"area": "subtitles", "queue": "text"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let after = body_json(api.clone().oneshot(get(&admin)).await.unwrap()).await;
    assert_eq!(blocked(after["queues"].as_array().unwrap()), 0);

    for body in [
        serde_json::json!({"area": "discovery", "queue": "scan"}),
        serde_json::json!({"area": "subtitles", "queue": "bogus"}),
    ] {
        assert_eq!(
            api.clone()
                .oneshot(rerun(&admin, body))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        api.oneshot(rerun(
            &admin,
            serde_json::json!({"area": "enrichment", "queue": "tmdb"}),
        ))
        .await
        .unwrap()
        .status(),
        StatusCode::OK
    );
}
