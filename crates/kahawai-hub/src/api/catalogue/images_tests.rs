use super::*;
use axum::{body::Body, http::Request};
use kahawai_proto::v1 as p;
use prost::Message;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

#[tokio::test]
async fn image_reads_are_authorized_cache_only_and_preserve_transparent_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let db = crate::db::open_in_memory().await.unwrap();
    let store = m::Store::in_memory().await.unwrap();
    store.put_mediahost("host", "Fixture").await.unwrap();
    let (collection, _) = store
        .offer_collection(
            "host",
            &p::CatalogCollection {
                id: "movies".into(),
                media_type: "movies".into(),
                epoch: "epoch".into(),
                current_version: 1,
                roots: vec![p::CollectionRoot::new("root", "/fixture")],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let path = "Film.2000.mkv";
    store
        .apply_catalogue(
            "host",
            &p::CatalogDelta {
                collection_id: "movies".into(),
                epoch: "epoch".into(),
                snapshot: true,
                done: true,
                through_version: 1,
                records: vec![p::CatalogRecord {
                    version: 1,
                    kind: "file".into(),
                    key: format!("root\0{path}").into_bytes(),
                    deleted: false,
                    payload: p::FileUpsert {
                        collection_id: "movies".into(),
                        files: vec![p::FileRecord {
                            source: Some(p::SourcePath::new("root", path)),
                            streams_json: "{}".into(),
                            ..Default::default()
                        }],
                    }
                    .encode_to_vec(),
                }],
            },
        )
        .await
        .unwrap();
    let library = store
        .create_library(
            "Movies",
            m::MediaType::Movies,
            std::slice::from_ref(&collection),
        )
        .await
        .unwrap();
    let copy = store.collection_items(&collection).await.unwrap().remove(0);
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let mut png = std::io::Cursor::new(Vec::new());
    image::RgbaImage::from_pixel(40, 20, image::Rgba([0, 0, 0, 0]))
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let png = png.into_inner();
    let mock = Router::new().route(
        "/logo.png",
        get(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            let png = png.clone();
            async move { ([(axum::http::header::CONTENT_TYPE, "image/png")], png) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/logo.png", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    });
    let mut record = m::ProviderRecord {
        provider: "tmdb".into(),
        namespace: "movie".into(),
        external_id: "1".into(),
        language: "en".into(),
        media_type: m::MediaType::Movies,
        title: copy.detected.title.clone(),
        year: copy.detected.year,
        description: m::Description {
            logo: Some(url.clone()),
            banner: Some(url.clone()),
            backdrop: Some(url.clone()),
            cast: Some(vec![m::Credit {
                name: "Actor".into(),
                id: Some("tmdb:7".into()),
                image: Some(url.clone()),
                ..Default::default()
            }]),
            ..Default::default()
        },
        children: None,
        season_artwork: None,
    };
    let record_id = store.put_provider_record(&record).await.unwrap();
    store
        .assign_metadata(&copy.id, Some(&record_id))
        .await
        .unwrap();
    let item = store
        .enrichment_input(&copy.id)
        .await
        .unwrap()
        .library_item_id;
    let registry = Arc::new(Registry::new(db.clone(), Default::default(), store));
    let auth = Arc::new(Auth::new(db, dir.path()).await.unwrap());
    auth.complete_setup("owner", "test-password").await.unwrap();
    let token = auth
        .login("owner", "test-password")
        .await
        .unwrap()
        .access_token;
    auth.create_user("viewer", "test-password", false)
        .await
        .unwrap();
    let viewer = auth
        .login("viewer", "test-password")
        .await
        .unwrap()
        .access_token;
    let enricher = Arc::new(crate::enrich::Enricher::new(dir.path().join("enrich")));
    let artwork = Arc::new(crate::artwork::Artwork::new(
        dir.path().join("artwork"),
        enricher.clone(),
    ));
    let ca = Arc::new(crate::pki::HubCa::load_or_create(&dir.path().join("pki")).unwrap());
    let state = AppState {
        registry: registry.clone(),
        auth,
        sessions: Arc::new(crate::sessions::Sessions::new(dir.path().join("sessions"))),
        enrollments: Arc::new(crate::enrollment_service::EnrollmentService::new(
            ca,
            registry.clone(),
            std::time::Duration::from_secs(60),
            90,
        )),
        subtitles: Arc::new(crate::subtitles::Subtitles::new(
            dir.path().join("subtitles"),
        )),
        artwork: artwork.clone(),
        enricher,
        proxy_trust: Default::default(),
        metrics_token: Arc::new(None),
        setup_url: Arc::new(None),
        public_origin: None,
    };
    let api = Router::new()
        .route(
            "/api/v1/catalogue/libraries/{id}/items/{item_id}/artwork",
            get(catalogue_artwork),
        )
        .route(
            "/api/v1/catalogue/libraries/{id}/items/{item_id}/cast/{person}/artwork",
            get(catalogue_cast_artwork),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer_or_media,
        ))
        .with_state(state)
        .layer(axum::middleware::from_fn(crate::error::request_context));
    let request = |path: String, token: Option<&str>| {
        let mut request = Request::get(path);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        api.clone().oneshot(request.body(Body::empty()).unwrap())
    };
    let base = format!("/api/v1/catalogue/libraries/{library}/items/{item}");
    assert_eq!(
        request(format!("{base}/artwork?kind=logo"), None)
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(format!("{base}/artwork?kind=logo"), Some(&viewer))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(format!("{base}/artwork?kind=logo"), Some(&token))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a cache miss must not ask the provider"
    );
    assert!(artwork.prefetch_remote(&url).await.unwrap());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    for suffix in [
        "/artwork?kind=logo&size=card",
        "/artwork?kind=banner",
        "/artwork?kind=backdrop",
        "/cast/tmdb:7/artwork?size=thumb",
    ] {
        let response = request(format!("{base}{suffix}"), Some(&token))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "image/png");
        assert_eq!(response.headers()["cache-control"], "private, no-cache");
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(
            image::load_from_memory(&bytes)
                .unwrap()
                .into_rgba8()
                .get_pixel(0, 0)[3],
            0
        );
    }
    assert_eq!(
        request(format!("{base}/cast/tmdb:8/artwork"), Some(&token))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(format!("{base}/artwork?kind=invalid"), Some(&token))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(format!("{base}/artwork?kind=logo&season=1"), Some(&token))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "all image reads use the populated cache"
    );
    record.description.logo = Some(url.replace("logo.png", "missing.png"));
    registry
        .catalogue()
        .put_provider_record(&record)
        .await
        .unwrap();
    assert_eq!(
        request(format!("{base}/artwork?kind=logo"), Some(&token))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
