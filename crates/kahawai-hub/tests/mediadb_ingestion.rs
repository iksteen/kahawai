//! Runtime cutover checks: the real router and local mediahost link, persisted
//! independent databases, authorization and committed ACKs.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use kahawai_hub::{auth::Auth, registry::Registry};
use kahawai_mediadb::{MediaType, Store};
use kahawai_proto::v1 as p;
use prost::Message;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

struct Fixture {
    dir: tempfile::TempDir,
    registry: Arc<Registry>,
    api: axum::Router,
    sessions: Arc<kahawai_hub::sessions::Sessions>,
    token: String,
    subtitles: Arc<kahawai_hub::subtitles::Subtitles>,
    enricher: Arc<kahawai_hub::enrich::Enricher>,
}
impl Fixture {
    async fn new() -> Self {
        Self::with_provider(None).await
    }
    async fn with_provider(
        provider: Option<Arc<dyn kahawai_hub::opensubtitles::SubtitleProvider>>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = kahawai_hub::db::open(dir.path()).await.unwrap();
        let store = kahawai_hub::db::open_catalogue(dir.path()).await.unwrap();
        let credentials = Arc::new(
            kahawai_hub::secrets::Credentials::open(dir.path(), db.clone())
                .await
                .unwrap(),
        );
        let registry = Arc::new(
            Registry::new(db.clone(), Default::default(), store).with_credentials(credentials),
        );
        sqlx::query("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','Fixture','fixture-cert')").execute(&db).await.unwrap();
        let auth = Arc::new(Auth::new(db.clone(), dir.path()).await.unwrap());
        auth.complete_setup("admin", "test-password").await.unwrap();
        let token = auth
            .login("admin", "test-password")
            .await
            .unwrap()
            .access_token;
        let sessions = Arc::new(kahawai_hub::sessions::Sessions::new(
            dir.path().join("sessions"),
        ));
        let ca =
            Arc::new(kahawai_hub::pki::HubCa::load_or_create(&dir.path().join("pki")).unwrap());
        let enrollment = Arc::new(kahawai_hub::enrollment_service::EnrollmentService::new(
            ca,
            registry.clone(),
            Duration::from_secs(60),
            90,
        ));
        let mut subtitles = kahawai_hub::subtitles::Subtitles::new(dir.path().join("subtitles"));
        if let Some(provider) = provider {
            subtitles = subtitles.with_provider(provider);
        }
        let subtitles = Arc::new(subtitles);
        let enricher = Arc::new(kahawai_hub::enrich::Enricher::new(dir.path().into()));
        let artwork = Arc::new(kahawai_hub::artwork::Artwork::new(
            dir.path().join("artwork"),
            enricher.clone(),
        ));
        let api = kahawai_hub::api::router(
            registry.clone(),
            auth,
            sessions.clone(),
            enrollment,
            subtitles.clone(),
            artwork,
            enricher.clone(),
            Default::default(),
        );
        Self {
            dir,
            registry,
            sessions,
            api,
            token,
            subtitles,
            enricher,
        }
    }
    fn link(
        &self,
    ) -> (
        tokio::sync::mpsc::Sender<p::HostToHub>,
        tokio::sync::mpsc::Receiver<Result<p::HubToHost, tonic::Status>>,
    ) {
        kahawai_hub::link_service::local_link(
            self.registry.clone(),
            self.subtitles.clone(),
            self.enricher.clone(),
            "host",
            "Fixture",
        )
    }
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Value,
        token: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(token) = token {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let response = self
            .api
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}
fn offer(remote: &str, version: u64) -> p::CatalogCollection {
    p::CatalogCollection {
        id: remote.into(),
        media_type: "movies".into(),
        epoch: "epoch".into(),
        current_version: version,
        roots: vec![p::CollectionRoot::new(
            kahawai_core::media::root_token(std::path::Path::new("/fixtures")),
            "/fixtures",
        )],
        ..Default::default()
    }
}
fn delta(remote: &str, version: u64, path: &str, snapshot: bool, done: bool) -> p::CatalogDelta {
    let token = offer(remote, version).roots[0].root_token.clone();
    let file = p::FileRecord {
        source: Some(p::SourcePath::new(&token, path)),
        size: 10,
        mtime_unix: 1,
        streams_json: "{}".into(),
        ..Default::default()
    };
    p::CatalogDelta {
        collection_id: remote.into(),
        epoch: "epoch".into(),
        snapshot,
        done,
        through_version: if snapshot && !done { 0 } else { version },
        records: vec![p::CatalogRecord {
            version,
            kind: "file".into(),
            key: format!("{token}\0{path}").into_bytes(),
            payload: p::FileUpsert {
                collection_id: remote.into(),
                files: vec![file],
            }
            .encode_to_vec(),
            deleted: false,
        }],
    }
}
async fn send(tx: &tokio::sync::mpsc::Sender<p::HostToHub>, msg: p::host_to_hub::Msg) {
    tx.send(p::HostToHub { msg: Some(msg) }).await.unwrap();
}
async fn receive(
    rx: &mut tokio::sync::mpsc::Receiver<Result<p::HubToHost, tonic::Status>>,
) -> p::hub_to_host::Msg {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .msg
        .unwrap()
}

#[tokio::test]
async fn ingestion_ack_browse_restart_archive_and_history_preservation() {
    let f = Fixture::new().await;
    let (tx, mut rx) = f.link();
    send(
        &tx,
        p::host_to_hub::Msg::CatalogOffer(p::CatalogOffer {
            collections: vec![offer("movies", 2)],
        }),
    )
    .await;
    assert!(
        matches!(receive(&mut rx).await,p::hub_to_host::Msg::CatalogCursor(c) if c.snapshot && c.version==0)
    );
    send(
        &tx,
        p::host_to_hub::Msg::CatalogDelta(delta("movies", 1, "Dark.City.1998.mkv", true, false)),
    )
    .await;
    send(
        &tx,
        p::host_to_hub::Msg::CatalogDelta(delta("movies", 2, "The.Matrix.1999.mkv", false, true)),
    )
    .await;
    assert!(matches!(receive(&mut rx).await,p::hub_to_host::Msg::CatalogAck(a) if a.version==2));
    // The ACK is checked against a separately opened connection, not cached state.
    let disk = Store::open(&f.dir.path().join("mediadb.db")).await.unwrap();
    let collection = disk.collections("host").await.unwrap()[0].id.clone();
    assert_eq!(disk.catalogue_cursor(&collection).await.unwrap().version, 2);
    assert_eq!(disk.files(&collection).await.unwrap().len(), 2);
    let (status, lib) = f
        .request(
            "POST",
            "/admin/v1/catalogue/libraries",
            json!({"name":"Movies","media_type":"movies","collection_ids":[collection]}),
            Some(&f.token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{lib}");
    let path = format!(
        "/api/v1/catalogue/libraries/{}/items",
        lib["id"].as_str().unwrap()
    );
    let (status, page) = f.request("GET", &path, Value::Null, Some(&f.token)).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    let (status, review) = f
        .request("GET", "/admin/v1/enrich/items", Value::Null, Some(&f.token))
        .await;
    assert_eq!(status, StatusCode::OK, "{review}");
    assert_eq!(review.as_array().unwrap().len(), 2);
    let copy = review[0]["id"].as_str().unwrap();
    let (status, detail) = f
        .request(
            "GET",
            &format!("/admin/v1/enrich/items/{copy}"),
            Value::Null,
            Some(&f.token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(
        f.request(
            "GET",
            "/admin/v1/enrich/progress",
            Value::Null,
            Some(&f.token)
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        f.request("GET", "/admin/v1/enrich/items", Value::Null, None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.request(
            "POST",
            &format!("/admin/v1/enrich/items/{copy}/match"),
            json!({"revision":-1,"action":"retry"}),
            Some(&f.token)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );

    let id = page["items"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        f.request("GET", &format!("{path}/{id}"), Value::Null, Some(&f.token))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        f.request("GET", &path, Value::Null, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.request("GET", "/api/v1/items", Value::Null, Some(&f.token))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.request("GET", "/api/v1/items", Value::Null, None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.request(
            "GET",
            &format!("{path}?limit=0"),
            Value::Null,
            Some(&f.token)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.request(
            "GET",
            &format!("{path}?limit=1&offset=1"),
            Value::Null,
            Some(&f.token)
        )
        .await
        .1["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let (new_tx, mut new_rx) = f.link();
    send(
        &new_tx,
        p::host_to_hub::Msg::CatalogOffer(p::CatalogOffer {
            collections: vec![offer("movies", 2)],
        }),
    )
    .await;
    assert!(
        matches!(receive(&mut new_rx).await,p::hub_to_host::Msg::CatalogCursor(c) if !c.snapshot && c.version==2)
    );
    // Superseded connection cannot write another file.
    let _ = tx
        .send(p::HostToHub {
            msg: Some(p::host_to_hub::Msg::CatalogDelta(delta(
                "movies",
                3,
                "Stale.2000.mkv",
                false,
                true,
            ))),
        })
        .await;
    send(
        &new_tx,
        p::host_to_hub::Msg::CatalogOffer(p::CatalogOffer::default()),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !disk.collections("host").await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(disk.library_item_record(&id).await.unwrap().archived);
    let backup = kahawai_hub::backup::backup(f.dir.path(), None, &f.dir.path().join("backup"))
        .await
        .unwrap();
    assert!(backup.mediadb_bytes.is_some());
    f.registry.delete_satellite("host").await.unwrap();
    disk.close().await;
}

#[tokio::test]
async fn external_grants_fail_closed_and_do_not_require_old_library_rows() {
    let f = Fixture::new().await;
    let store = f.registry.catalogue();
    let one = store
        .create_library("One", MediaType::Movies, &[])
        .await
        .unwrap();
    let two = store
        .create_library("Two", MediaType::Movies, &[])
        .await
        .unwrap();
    let hash = kahawai_hub::auth::hash_password("test-password").unwrap();
    sqlx::query("INSERT INTO users(id,username,password_hash,is_admin,all_libraries) VALUES('restricted','restricted',?,0,0)").bind(hash).execute(f.registry.db()).await.unwrap();
    let auth = Auth::new(f.registry.db().clone(), f.dir.path())
        .await
        .unwrap();
    let token = auth
        .login("restricted", "test-password")
        .await
        .unwrap()
        .access_token;
    let granted = f
        .request(
            "PUT",
            "/admin/v1/users/restricted/libraries",
            json!({"grants_version":0,"all_libraries":false,"libraries":[one,"missing"]}),
            Some(&f.token),
        )
        .await;
    assert_eq!(granted.0, StatusCode::OK, "{:?}", granted.1);
    assert_eq!(granted.1["libraries"], json!([one]));
    assert_eq!(
        f.request(
            "GET",
            "/api/v1/catalogue/libraries",
            Value::Null,
            Some(&token)
        )
        .await
        .1
        .as_array()
        .unwrap()
        .len(),
        1
    );
    assert_eq!(
        f.request(
            "GET",
            &format!("/api/v1/catalogue/libraries/{two}/items"),
            Value::Null,
            Some(&token)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    store
        .offer_catalogue(
            "host",
            "Fixture",
            &p::CatalogOffer {
                collections: vec![offer("movies", 1)],
            },
        )
        .await
        .unwrap();
    store
        .apply_catalogue("host", &delta("movies", 1, "Hidden.2000.mkv", true, true))
        .await
        .unwrap();
    let collection = store.collections("host").await.unwrap()[0].id.clone();
    store
        .set_library_collections(&two, std::slice::from_ref(&collection))
        .await
        .unwrap();
    let hidden_item = store.collection_items(&collection).await.unwrap()[0]
        .library_item_id
        .clone();
    for item in [&hidden_item, "missing"] {
        assert_eq!(
            f.request(
                "GET",
                &format!("/api/v1/catalogue/libraries/{one}/items/{item}"),
                Value::Null,
                Some(&token)
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        f.request(
            "GET",
            "/admin/v1/catalogue/collections",
            Value::Null,
            Some(&token)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.request(
            "PUT",
            "/admin/v1/users/restricted/libraries",
            json!({"grants_version":0,"all_libraries":false,"libraries":[two]}),
            Some(&f.token)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.request(
            "DELETE",
            &format!("/admin/v1/catalogue/libraries/{one}"),
            Value::Null,
            Some(&f.token)
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert!(
        f.request(
            "GET",
            "/api/v1/catalogue/libraries",
            Value::Null,
            Some(&token)
        )
        .await
        .1
        .as_array()
        .unwrap()
        .is_empty()
    );
}

#[tokio::test]
async fn revocation_serializes_with_queued_ingestion() {
    let f = Fixture::new().await;
    let (tx, mut rx) = f.link();
    send(
        &tx,
        p::host_to_hub::Msg::CatalogOffer(p::CatalogOffer {
            collections: vec![offer("movies", 1)],
        }),
    )
    .await;
    receive(&mut rx).await;
    send(
        &tx,
        p::host_to_hub::Msg::CatalogDelta(delta("movies", 1, "One.2000.mkv", true, true)),
    )
    .await;
    receive(&mut rx).await;
    let producer = tokio::spawn(async move {
        for version in 2..100 {
            if tx
                .send(p::HostToHub {
                    msg: Some(p::host_to_hub::Msg::CatalogDelta(delta(
                        "movies",
                        version,
                        "One.2000.mkv",
                        false,
                        true,
                    ))),
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });
    f.registry.delete_satellite("host").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), producer)
        .await
        .unwrap()
        .unwrap();
    assert!(
        f.registry
            .catalogue()
            .collections("host")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(f.registry.catalogue().stats().await.unwrap().files, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM satellites WHERE module_id='host'")
            .fetch_one(f.registry.db())
            .await
            .unwrap(),
        0
    );
    // A delayed local adapter also cannot re-create a revoked namespace.
    let (tx, mut rx) = f.link();
    send(
        &tx,
        p::host_to_hub::Msg::CatalogOffer(p::CatalogOffer {
            collections: vec![offer("movies", 1)],
        }),
    )
    .await;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(
        f.registry
            .catalogue()
            .collections("host")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn grants_upgrade_preserves_grants_and_user_foreign_key_after_catalogue_removal() {
    use sqlx::Connection;
    let dir = tempfile::tempdir().unwrap();
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.path().join("hub.db"))
        .create_if_missing(true)
        .foreign_keys(true);
    let mut connection = sqlx::SqliteConnection::connect_with(&options)
        .await
        .unwrap();
    let all = sqlx::migrate!("./migrations");
    let baseline = sqlx::migrate::Migrator::with_migrations(
        all.iter().filter(|m| m.version < 87).cloned().collect(),
    );
    baseline.run(&mut connection).await.unwrap();
    sqlx::query("INSERT INTO users(id,username,password_hash,is_admin) VALUES('old-user','old-user','unused-hash',0)").execute(&mut connection).await.unwrap();
    sqlx::query("INSERT INTO libraries(id,name,media_type) VALUES('old-library','Old','movies')")
        .execute(&mut connection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_libraries VALUES('old-user','old-library')")
        .execute(&mut connection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO library_items(id,kind,title,norm_title,sort_title,added_id) VALUES('old-item','movie','Old','old','old','old')").execute(&mut connection).await.unwrap();
    sqlx::query("INSERT INTO user_item_state(user_id,item_id,position_ms,played,play_count) VALUES('old-user','old-item',123456,1,7)").execute(&mut connection).await.unwrap();
    connection.close().await.unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let grants: Vec<(String, String)> =
        sqlx::query_as("SELECT user_id,library_id FROM user_libraries")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(grants, vec![("old-user".into(), "old-library".into())]);
    assert!(
        sqlx::query("SELECT * FROM user_item_state")
            .fetch_all(&db)
            .await
            .is_err()
    );
    sqlx::query("INSERT INTO user_libraries VALUES('old-user','external-mediadb-id')")
        .execute(&db)
        .await
        .unwrap();
    assert!(
        sqlx::query("INSERT INTO user_libraries VALUES('absent-user','external-mediadb-id')")
            .execute(&db)
            .await
            .is_err()
    );
    db.close().await;
}

#[tokio::test]
async fn detail_sources_keep_parts_and_private_collection_boundaries() {
    let f = Fixture::new().await;
    let store = f.registry.catalogue();
    store
        .offer_catalogue(
            "host",
            "Fixture",
            &p::CatalogOffer {
                collections: vec![offer("visible", 2), offer("private", 1)],
            },
        )
        .await
        .unwrap();
    for (collection, version, path, snapshot, done) in [
        ("visible", 1, "Dark.City.1998.CD1.mkv", true, false),
        ("visible", 2, "Dark.City.1998.CD2.mkv", false, true),
        ("private", 1, "Dark.City.1998.private.mkv", true, true),
    ] {
        let mut d = delta(collection, version, path, snapshot, done);
        let mut file = p::FileUpsert::decode(d.records[0].payload.as_slice()).unwrap();
        file.files[0].streams_json = json!({"container":"matroska","duration_ms":60000,
            "chapters":[{"start_ms":0,"title":"Opening"}],
            "artwork":"private-cover.jpg", "nfo":"private.nfo"})
        .to_string();
        d.records[0].payload = file.encode_to_vec();
        store.apply_catalogue("host", &d).await.unwrap();
    }
    let collections = store.collections("host").await.unwrap();
    let visible = collections
        .iter()
        .find(|c| c.remote_id == "visible")
        .unwrap();
    let library = store
        .create_library(
            "Visible",
            MediaType::Movies,
            std::slice::from_ref(&visible.id),
        )
        .await
        .unwrap();
    let row = store.browse(&library, 0, 10).await.unwrap().remove(0);
    let all = store
        .create_library(
            "All",
            MediaType::Movies,
            &collections.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .library_item(&all, &row.id)
            .await
            .unwrap()
            .copy_ids
            .len(),
        2
    );

    let path = format!("/api/v1/catalogue/libraries/{library}/items/{}", row.id);
    let (status, detail) = f.request("GET", &path, Value::Null, Some(&f.token)).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let sources = detail["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0]["source_id"], sources[1]["source_id"]);
    assert_eq!(sources[0]["part"], 1);
    assert_eq!(sources[1]["part"], 2);
    assert_eq!(sources[0]["parts"], 2);
    assert_eq!(sources[0]["available"], false);
    assert_eq!(sources[0]["host_name"], "Fixture");
    assert_eq!(sources[0]["collection_id"], "visible");
    assert_eq!(sources[0]["streams"]["container"], "matroska");
    assert_eq!(detail["copies"].as_array().unwrap().len(), 1);
    assert_eq!(detail["duration_ms"], 120000);
    assert_eq!(detail["chapters"][1]["start_ms"], 60000);
    let wire = detail.to_string();
    for private in ["/fixtures", "private.mkv", "private.nfo"] {
        assert!(!wire.contains(private), "leaked {private}: {wire}");
    }
    assert!(
        !serde_json::to_string(sources)
            .unwrap()
            .contains("private-cover.jpg")
    );
    store.set_library_collections(&library, &[]).await.unwrap();
    assert_eq!(
        f.request("GET", &path, Value::Null, Some(&f.token)).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn candidate_artwork_accepts_only_admin_media_credentials_and_never_authorizes_writes() {
    let f = Fixture::new().await;
    let path = "/api/v1/catalogue/collection-items/missing/artwork";
    assert_eq!(
        f.request("GET", path, Value::Null, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let request = |method: &str, path: &str, token: &str| {
        Request::builder()
            .method(method)
            .uri(path)
            .header("cookie", format!("kahawai_media={token}"))
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap()
    };
    assert_eq!(
        f.api
            .clone()
            .oneshot(request("GET", path, &f.token))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.api
            .clone()
            .oneshot(request(
                "POST",
                "/admin/v1/enrich/items/missing/match",
                &f.token
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let (status, _) = f
        .request(
            "POST",
            "/admin/v1/users",
            json!({"username":"viewer","password":"viewer-password-long"}),
            Some(&f.token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, login) = f
        .request(
            "POST",
            "/api/v1/auth/token",
            json!({"client":"api","username":"viewer","password":"viewer-password-long"}),
            None,
        )
        .await;
    let token = login["access_token"].as_str().unwrap();
    assert_eq!(
        f.api
            .clone()
            .oneshot(request("GET", path, token))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn child_api_preserves_positions_sources_and_library_scope() {
    let f = Fixture::new().await;
    let store = f.registry.catalogue();
    let mut collection = offer("series", 2);
    collection.media_type = "series".into();
    store
        .offer_catalogue(
            "host",
            "Fixture",
            &p::CatalogOffer {
                collections: vec![collection],
            },
        )
        .await
        .unwrap();
    let mut d = delta("series", 1, "Show (2000)/Show.S01E01-E02.mkv", true, true);
    let mut files = p::FileUpsert::decode(d.records[0].payload.as_slice()).unwrap();
    files.files[0].streams_json =
        json!({"duration_ms":60000,"chapters":[{"start_ms":0,"title":"Whole file"}]}).to_string();
    d.records[0].payload = files.encode_to_vec();
    store.apply_catalogue("host", &d).await.unwrap();
    store
        .apply_catalogue(
            "host",
            &delta("series", 2, "Show (2000)/Show.S01E03.mkv", false, true),
        )
        .await
        .unwrap();
    let col = store.collections("host").await.unwrap().remove(0);
    let library = store
        .create_library("Shows", MediaType::Series, &[col.id])
        .await
        .unwrap();
    let parent = store.browse(&library, 0, 10).await.unwrap().remove(0);
    let path = format!(
        "/api/v1/catalogue/libraries/{library}/items/{}/children",
        parent.id
    );
    let (status, page) = f
        .request(
            "GET",
            &format!("{path}?offset=1&limit=1&season=1"),
            Value::Null,
            Some(&f.token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["total"], 3);
    assert_eq!(page["groups"][0]["total"], 3);
    assert_eq!(page["children"].as_array().unwrap().len(), 1);
    assert_eq!(page["children"][0]["position"]["episode"], 2);
    let id = page["children"][0]["id"].as_str().unwrap();
    let detail = format!("/api/v1/catalogue/libraries/{library}/items/{id}");
    let (status, body) = f.request("GET", &detail, Value::Null, Some(&f.token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["child"]["parent_id"], parent.id);
    assert_eq!(body["sources"].as_array().unwrap().len(), 1);
    assert_eq!(body["copies"][0]["paths"].as_array().unwrap().len(), 1);
    assert!(!body.to_string().contains("S01E03"));
    assert!(body["sources"][0]["media_entry_id"].as_str().is_some());
    assert!(
        body["duration_ms"].is_null(),
        "combined-file runtime is not an episode runtime"
    );
    assert_eq!(body["chapters"], json!([]));
    assert!(!body.to_string().contains("/fixtures"));
    assert_eq!(
        f.request("GET", &detail, Value::Null, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.request(
            "GET",
            &format!("{path}?limit=201"),
            Value::Null,
            Some(&f.token)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let hidden = store
        .create_library("Empty", MediaType::Series, &[])
        .await
        .unwrap();
    assert_eq!(
        f.request(
            "GET",
            &format!("/api/v1/catalogue/libraries/{hidden}/items/{id}"),
            Value::Null,
            Some(&f.token)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.request(
            "GET",
            &format!(
                "/api/v1/catalogue/libraries/{library}/items/{}:description:0",
                parent.id
            ),
            Value::Null,
            Some(&f.token)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // User state follows the stable child, not a source copy or a library.
    let mark = format!("{detail}/watched");
    assert_eq!(
        f.request("PUT", &mark, json!({"played":true}), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, updated) = f
        .request("PUT", &mark, json!({"played":true}), Some(&f.token))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert!(updated["updated"][0].get("play_count").is_none());
    assert_eq!(
        f.request("GET", &detail, Value::Null, Some(&f.token))
            .await
            .1["played"],
        true
    );
    let page = f
        .request(
            "GET",
            &format!("{path}?limit=1"),
            Value::Null,
            Some(&f.token),
        )
        .await
        .1;
    assert_eq!(page["groups"][0]["played"], 1, "counts include later pages");
    let collections = store
        .libraries()
        .await
        .unwrap()
        .into_iter()
        .find(|l| l.id == library)
        .unwrap()
        .collection_ids;
    let other = store
        .create_library("Another view", MediaType::Series, &collections)
        .await
        .unwrap();
    let other_detail = format!("/api/v1/catalogue/libraries/{other}/items/{id}");
    assert_eq!(
        f.request("GET", &other_detail, Value::Null, Some(&f.token))
            .await
            .1["played"],
        true
    );
    assert_eq!(
        f.request(
            "PUT",
            &format!("/api/v1/catalogue/libraries/{hidden}/items/{id}/watched"),
            json!({"played":false}),
            Some(&f.token)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let parent_mark = format!(
        "/api/v1/catalogue/libraries/{library}/items/{}/watched",
        parent.id
    );
    assert_eq!(
        f.request(
            "PUT",
            &parent_mark,
            json!({"played":false,"items":[id,"child1:invalid:e:1:1"]}),
            Some(&f.token)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.request("GET", &detail, Value::Null, Some(&f.token))
            .await
            .1["played"],
        true,
        "invalid batches do not partially write"
    );

    let hash = kahawai_hub::auth::hash_password("test-password").unwrap();
    sqlx::query("INSERT INTO users(id,username,password_hash,is_admin,all_libraries) VALUES('viewer','viewer',?,0,0)").bind(hash).execute(f.registry.db()).await.unwrap();
    let auth = Auth::new(f.registry.db().clone(), f.dir.path())
        .await
        .unwrap();
    let viewer = auth
        .login("viewer", "test-password")
        .await
        .unwrap()
        .access_token;
    assert_eq!(
        f.request("PUT", &mark, json!({"played":true}), Some(&viewer))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    sqlx::query("INSERT INTO user_libraries(user_id,library_id) VALUES('viewer',?)")
        .bind(&library)
        .execute(f.registry.db())
        .await
        .unwrap();
    assert_eq!(
        f.request("GET", &detail, Value::Null, Some(&viewer))
            .await
            .1["played"],
        false,
        "marks are private to each user"
    );
    assert_eq!(
        f.request("PUT", &mark, json!({"played":true}), Some(&viewer))
            .await
            .0,
        StatusCode::OK
    );
    sqlx::query("DELETE FROM users WHERE id='viewer'")
        .execute(f.registry.db())
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM catalogue_watch_state WHERE user_id='viewer'"
        )
        .fetch_one(f.registry.db())
        .await
        .unwrap(),
        0
    );

    store.set_library_collections(&library, &[]).await.unwrap();
    assert_eq!(
        f.request("GET", &detail, Value::Null, Some(&f.token))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    store
        .set_library_collections(&library, &collections)
        .await
        .unwrap();
    let reopened = kahawai_hub::db::open(f.dir.path()).await.unwrap();
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT played FROM catalogue_watch_state WHERE item_id=?")
            .bind(id)
            .fetch_one(&reopened)
            .await
            .unwrap()
    );
    assert_eq!(
        f.request("GET", &detail, Value::Null, Some(&f.token))
            .await
            .1["played"],
        true
    );

    // A season mark reaches beyond the 200-row page, as one atomic write.
    store
        .apply_catalogue(
            "host",
            &delta("series", 3, "Show (2000)/Show.S01E04-E205.mkv", false, true),
        )
        .await
        .unwrap();
    let (status, body) = f
        .request(
            "PUT",
            &parent_mark,
            json!({"played":true,"season":"1"}),
            Some(&f.token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["updated"].as_array().unwrap().len(), 205);
    let last = f
        .request(
            "GET",
            &format!("{path}?offset=200&limit=5"),
            Value::Null,
            Some(&f.token),
        )
        .await
        .1;
    assert_eq!(last["groups"][0]["played"], 205);
    for child in last["children"].as_array().unwrap() {
        assert_eq!(last["watch"][child["id"].as_str().unwrap()]["played"], true);
    }
    let user: String = sqlx::query_scalar("SELECT id FROM users WHERE username='admin'")
        .fetch_one(f.registry.db())
        .await
        .unwrap();
    kahawai_hub::watch::progress(
        f.registry.db(),
        &user,
        &[kahawai_hub::watch::Progress {
            id: id.into(),
            parent: parent.id.clone(),
            position: 5000,
            duration: Some(10000),
            track: false,
        }],
    )
    .await
    .unwrap();
    let resumed = f
        .request("GET", &detail, Value::Null, Some(&f.token))
        .await
        .1;
    assert_eq!(resumed["played"], false);
    assert_eq!(resumed["resume_position_ms"], 5000);
    f.request("PUT", &mark, json!({"played":false}), Some(&f.token))
        .await;
    assert!(
        f.request("GET", &detail, Value::Null, Some(&f.token))
            .await
            .1["resume_position_ms"]
            .is_null()
    );
}

#[tokio::test]
async fn home_feeds_use_private_visible_history_and_native_episode_order() {
    let f = Fixture::new().await;
    let store = f.registry.catalogue();
    let mut shows = offer("series", 1);
    shows.media_type = "series".into();
    store
        .offer_catalogue(
            "host",
            "Fixture",
            &p::CatalogOffer {
                collections: vec![shows, offer("movies", 1)],
            },
        )
        .await
        .unwrap();
    store
        .apply_catalogue(
            "host",
            &delta("series", 1, "Show (2000)/Show.S01E01-E05.mkv", true, true),
        )
        .await
        .unwrap();
    store
        .apply_catalogue("host", &delta("movies", 1, "Movie.2000.mkv", true, true))
        .await
        .unwrap();
    let collections = store.collections("host").await.unwrap();
    let series_col = &collections
        .iter()
        .find(|c| c.remote_id == "series")
        .unwrap()
        .id;
    let movie_col = &collections
        .iter()
        .find(|c| c.remote_id == "movies")
        .unwrap()
        .id;
    let library = store
        .create_library("Shows", MediaType::Series, std::slice::from_ref(series_col))
        .await
        .unwrap();
    let duplicate = store
        .create_library(
            "Same shows",
            MediaType::Series,
            std::slice::from_ref(series_col),
        )
        .await
        .unwrap();
    let movies = store
        .create_library("Movies", MediaType::Movies, std::slice::from_ref(movie_col))
        .await
        .unwrap();
    let show = store.browse(&library, 0, 10).await.unwrap().remove(0).id;
    let movie = store.browse(&movies, 0, 10).await.unwrap().remove(0).id;
    let children = store
        .library_children(&library, &show, 0, 200, &Default::default())
        .await
        .unwrap()
        .children;
    let ids: Vec<_> = children.iter().map(|c| c.id.clone()).collect();
    let user: String = sqlx::query_scalar("SELECT id FROM users WHERE username='admin'")
        .fetch_one(f.registry.db())
        .await
        .unwrap();
    let next = "/api/v1/catalogue/up-next";
    let continuing = "/api/v1/catalogue/continue-watching";
    for path in [next, continuing] {
        assert_eq!(
            f.request("GET", path, Value::Null, None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            f.request("GET", path, Value::Null, Some(&f.token)).await.1["total"],
            0,
            "legacy history is not imported"
        );
        assert_eq!(
            f.request(
                "GET",
                &format!("{path}?limit=0"),
                Value::Null,
                Some(&f.token)
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    kahawai_hub::watch::mark(
        f.registry.db(),
        &user,
        &show,
        &[ids[0].clone(), ids[2].clone()],
        true,
    )
    .await
    .unwrap();
    let page = f.request("GET", next, Value::Null, Some(&f.token)).await.1;
    assert_eq!(
        page["total"], 1,
        "same identity across libraries is not duplicated"
    );
    assert_eq!(
        page["items"][0]["id"], ids[3],
        "batch tie selects highest native position, skipping earlier gaps"
    );
    assert_eq!(page["items"][0]["parent_title"], "Show");
    // Temporal recency wins over the highest episode when watching out of order.
    sqlx::query("UPDATE catalogue_watch_state SET updated_at=updated_at+1 WHERE item_id=?")
        .bind(&ids[0])
        .execute(f.registry.db())
        .await
        .unwrap();
    assert_eq!(
        f.request("GET", next, Value::Null, Some(&f.token)).await.1["items"][0]["id"],
        ids[1]
    );
    // The earlier gap is now watched; already-finished episode 3 is skipped.
    kahawai_hub::watch::mark(
        f.registry.db(),
        &user,
        &show,
        std::slice::from_ref(&ids[1]),
        true,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE catalogue_watch_state SET updated_at=1")
        .execute(f.registry.db())
        .await
        .unwrap();
    assert_eq!(
        f.request("GET", next, Value::Null, Some(&f.token)).await.1["items"][0]["id"],
        ids[3],
        "recent physical arrival revives an old series"
    );
    for (position, duration, meaningful) in [
        (59999, Some(600000), false),
        (60000, Some(600000), true),
        (60000, Some(6100000), false),
        (61000, Some(6100000), true),
        (60000, None, true),
    ] {
        kahawai_hub::watch::progress(
            f.registry.db(),
            &user,
            &[kahawai_hub::watch::Progress {
                id: ids[3].clone(),
                parent: show.clone(),
                position,
                duration,
                track: false,
            }],
        )
        .await
        .unwrap();
        let a = f
            .request("GET", continuing, Value::Null, Some(&f.token))
            .await
            .1;
        let b = f.request("GET", next, Value::Null, Some(&f.token)).await.1;
        assert_eq!(
            a["total"],
            usize::from(meaningful),
            "position={position}, duration={duration:?}"
        );
        assert_eq!(b["total"], usize::from(!meaningful));
        if meaningful {
            assert_eq!(a["items"][0]["id"], ids[3]);
            assert_eq!(a["items"][0]["resume_position_ms"], position);
        }
    }
    kahawai_hub::watch::progress(
        f.registry.db(),
        &user,
        &[kahawai_hub::watch::Progress {
            id: movie.clone(),
            parent: movie.clone(),
            position: 120000,
            duration: Some(1000000),
            track: false,
        }],
    )
    .await
    .unwrap();
    let page = f
        .request(
            "GET",
            &format!("{continuing}?limit=1&offset=1"),
            Value::Null,
            Some(&f.token),
        )
        .await
        .1;
    assert_eq!(page["total"], 2);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        f.request(
            "GET",
            &format!("{continuing}?library={movies}"),
            Value::Null,
            Some(&f.token)
        )
        .await
        .1["items"][0]["id"],
        movie
    );
    kahawai_hub::watch::mark(
        f.registry.db(),
        &user,
        &movie,
        std::slice::from_ref(&movie),
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        f.request("GET", continuing, Value::Null, Some(&f.token))
            .await
            .1["total"],
        1
    );
    // Another account gets none of this user's marks, and denied scopes fail closed.
    let hash = kahawai_hub::auth::hash_password("test-password").unwrap();
    sqlx::query("INSERT INTO users(id,username,password_hash,is_admin,all_libraries) VALUES('feed-viewer','feed-viewer',?,0,0)").bind(hash).execute(f.registry.db()).await.unwrap();
    let auth = Auth::new(f.registry.db().clone(), f.dir.path())
        .await
        .unwrap();
    let viewer = auth
        .login("feed-viewer", "test-password")
        .await
        .unwrap()
        .access_token;
    for path in [continuing, next] {
        assert_eq!(
            f.request("GET", path, Value::Null, Some(&viewer)).await.1["total"],
            0
        );
        assert_eq!(
            f.request(
                "GET",
                &format!("{path}?library={library}"),
                Value::Null,
                Some(&viewer)
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
    sqlx::query("INSERT INTO user_libraries(user_id,library_id) VALUES('feed-viewer',?)")
        .bind(&duplicate)
        .execute(f.registry.db())
        .await
        .unwrap();
    kahawai_hub::watch::mark(
        f.registry.db(),
        "feed-viewer",
        &show,
        std::slice::from_ref(&ids[0]),
        true,
    )
    .await
    .unwrap();
    let page = f.request("GET", next, Value::Null, Some(&viewer)).await.1;
    assert_eq!(page["items"][0]["library_id"], duplicate);
    assert_eq!(page["items"][0]["id"], ids[1]);
    store.set_library_collections(&library, &[]).await.unwrap();
    store
        .set_library_collections(&duplicate, &[])
        .await
        .unwrap();
    assert_eq!(
        f.request("GET", continuing, Value::Null, Some(&f.token))
            .await
            .1["total"],
        0,
        "removed sources are not resume targets"
    );
    assert_eq!(
        f.request("GET", next, Value::Null, Some(&viewer)).await.1["total"],
        0
    );
    store
        .set_library_collections(&duplicate, std::slice::from_ref(series_col))
        .await
        .unwrap();
    assert_eq!(
        f.request("GET", continuing, Value::Null, Some(&f.token))
            .await
            .1["items"][0]["id"],
        ids[3]
    );
    kahawai_hub::watch::mark(f.registry.db(), &user, &show, &ids, true)
        .await
        .unwrap();
    assert_eq!(
        f.request("GET", next, Value::Null, Some(&f.token)).await.1["total"],
        0,
        "a completed series has no next episode"
    );
}

#[tokio::test]
async fn playback_uses_catalogue_sources_and_captured_watch_identity() {
    movie_playback_case(MediaType::Movies).await;
}

#[tokio::test]
async fn anime_movies_play_and_resume_with_captured_watch_identity() {
    movie_playback_case(MediaType::Anime).await;
}

async fn movie_playback_case(kind: MediaType) {
    let f = Fixture::new().await;
    let store = f.registry.catalogue();
    store.put_mediahost("host", "Fixture").await.unwrap();
    let mut offered = offer("movies", 1);
    offered.media_type = kind.as_str().into();
    let (collection, _) = store.offer_collection("host", &offered).await.unwrap();
    let mut change = delta("movies", 1, "Dark.City.1998.mp4", true, true);
    let mut upsert = p::FileUpsert::decode(change.records[0].payload.as_slice()).unwrap();
    upsert.files[0].streams_json=json!({"container":"mp4","duration_ms":600000,"external_subtitles":[{"path_rel":"Dark.City.1998.en.srt","format":"srt","language":"eng"}]}).to_string();
    change.records[0].payload = upsert.encode_to_vec();
    store.apply_catalogue("host", &change).await.unwrap();
    let library = store
        .create_library("Films", kind, std::slice::from_ref(&collection))
        .await
        .unwrap();
    let copy = store.collection_items(&collection).await.unwrap().remove(0);
    let item = copy.library_item_id.clone();
    let file = f.dir.path().join("video.mp4");
    std::fs::write(&file, b"0123456789").unwrap();
    let subtitle = f.dir.path().join("subtitle.srt");
    std::fs::write(
        &subtitle,
        "1\n00:00:00,000 --> 00:00:01,000\nCatalogue subtitle\n",
    )
    .unwrap();
    f.sessions.set_local_source("host", move |_, _, path| {
        Ok(if path.ends_with(".srt") {
            subtitle.clone()
        } else {
            file.clone()
        })
    });
    f.registry
        .connected("host", "mediahost", "Fixture", "fixture-cert", "test");
    let route = format!("/api/v1/catalogue/libraries/{library}/items/{item}");
    let (status, facts) = f.request("GET", &route, Value::Null, Some(&f.token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(facts["negotiated"].is_null());
    assert!(!facts["sources"].as_array().unwrap().is_empty());
    let (status, refusal) = f.request("POST", &route, json!({}), Some(&f.token)).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(refusal["code"], "method_not_allowed");

    let (status, preview) = f
        .request("QUERY", &route, json!({"mode":"direct"}), Some(&f.token))
        .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["kind"], "movie", "{preview}");
    assert_eq!(preview["negotiated"]["mode"], "direct", "{preview}");
    let source = preview["sources"][0]["media_entry_id"].clone();
    let (status, bad) = f
        .request(
            "POST",
            "/api/v1/playback/sessions",
            json!({"library_id":library,"item_id":item,"media_entry_id":"missing","mode":"direct"}),
            Some(&f.token),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{bad}");
    let (status, session) = f
        .request(
            "POST",
            "/api/v1/playback/sessions",
            json!({"library_id":library,"item_id":item,"media_entry_id":source,"mode":"direct"}),
            Some(&f.token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    assert_eq!(session["media_entry_id"], source);
    let sid = session["session_id"].as_str().unwrap();
    let response = f
        .api
        .clone()
        .oneshot(
            Request::builder()
                .uri(session["stream_url"].as_str().unwrap())
                .header("authorization", format!("Bearer {}", f.token))
                .header("range", "bytes=2-5")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 100)
            .await
            .unwrap()
            .as_ref(),
        b"2345"
    );
    assert_eq!(session["subtitle_listing"][0]["format"], "srt");
    let response = f
        .api
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/playback/sessions/{sid}/subtitles/1.vtt"))
                .header("authorization", format!("Bearer {}", f.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1000)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("Catalogue subtitle"));
    let (status, user) = f
        .request(
            "POST",
            "/admin/v1/users",
            json!({"username":"outsider","password":"test-password","admin":false}),
            Some(&f.token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{user}");
    let (_, login) = f
        .request(
            "POST",
            "/api/v1/auth/token",
            json!({"client":"api","username":"outsider","password":"test-password"}),
            None,
        )
        .await;
    let other = login["access_token"].as_str().unwrap();
    sqlx::query("UPDATE users SET all_libraries=0 WHERE username='outsider'")
        .execute(f.registry.db())
        .await
        .unwrap();
    assert_eq!(
        f.request(
            "POST",
            "/api/v1/playback/sessions",
            json!({"library_id":library,"item_id":item,"mode":"direct"}),
            Some(other)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.request(
            "POST",
            &format!("/api/v1/playback/sessions/{sid}/progress"),
            json!({"position_ms":1}),
            Some(other)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let progress = format!("/api/v1/playback/sessions/{sid}/progress");
    assert_eq!(
        f.request(
            "POST",
            &progress,
            json!({"position_ms":120000}),
            Some(&f.token)
        )
        .await
        .0,
        StatusCode::OK
    );
    let feed = f
        .request(
            "GET",
            "/api/v1/catalogue/continue-watching",
            Value::Null,
            Some(&f.token),
        )
        .await
        .1;
    assert_eq!(feed["items"][0]["id"], item);
    let record = store
        .put_provider_record(&kahawai_mediadb::ProviderRecord {
            children: None,
            provider: "fixture".into(),
            namespace: "movie".into(),
            external_id: "matrix".into(),
            language: "en".into(),
            media_type: MediaType::Movies,
            title: "The Matrix".into(),
            year: Some(1999),
            description: Default::default(),
        })
        .await
        .unwrap();
    store
        .assign_metadata(&copy.id, Some(&record))
        .await
        .unwrap();
    let moved = store
        .enrichment_input(&copy.id)
        .await
        .unwrap()
        .library_item_id;
    assert_ne!(moved, item);
    let (status, reported) = f
        .request(
            "POST",
            &progress,
            json!({"position_ms":550000}),
            Some(&f.token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{reported}");
    assert_eq!(reported["played"], true);
    let rows: Vec<(String, bool)> =
        sqlx::query_as("SELECT item_id,played FROM catalogue_watch_state")
            .fetch_all(f.registry.db())
            .await
            .unwrap();
    assert_eq!(rows, vec![(item.clone(), true)]);
    assert_eq!(
        f.request(
            "POST",
            "/api/v1/playback/sessions",
            json!({"library_id":library,"item_id":item,"mode":"direct"}),
            Some(&f.token)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.request(
            "DELETE",
            &format!("/api/v1/playback/sessions/{sid}"),
            Value::Null,
            Some(&f.token)
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (status, new_session) = f
        .request(
            "POST",
            "/api/v1/playback/sessions",
            json!({"library_id":library,"item_id":moved,"mode":"direct"}),
            Some(&f.token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{new_session}");
    assert_eq!(
        f.request(
            "PUT",
            &format!("/admin/v1/catalogue/libraries/{library}/collections"),
            json!({"collection_ids":[]}),
            Some(&f.token)
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert!(
        f.sessions
            .get(new_session["session_id"].as_str().unwrap())
            .is_none()
    );
}

#[tokio::test]
async fn combined_episode_playback_finishes_captured_coverage_and_next_skips_the_file() {
    let f = Fixture::new().await;
    let store = f.registry.catalogue();
    store.put_mediahost("host", "Fixture").await.unwrap();
    let mut offer = offer("shows", 2);
    offer.media_type = "series".into();
    let (collection, _) = store.offer_collection("host", &offer).await.unwrap();
    let mut change = delta("shows", 1, "Show (2000)/Show.S01E01-E02.mp4", true, false);
    let mut upsert = p::FileUpsert::decode(change.records[0].payload.as_slice()).unwrap();
    upsert.files[0].streams_json = json!({"container":"mp4","duration_ms":600000}).to_string();
    change.records[0].payload = upsert.encode_to_vec();
    store.apply_catalogue("host", &change).await.unwrap();
    store
        .apply_catalogue(
            "host",
            &delta("shows", 2, "Show (2000)/Show.S01E03.mp4", true, true),
        )
        .await
        .unwrap();
    let library = store
        .create_library(
            "Shows",
            MediaType::Series,
            std::slice::from_ref(&collection),
        )
        .await
        .unwrap();
    let parent = store.collection_items(&collection).await.unwrap()[0]
        .library_item_id
        .clone();
    let children = store
        .library_children(&library, &parent, 0, 200, &Default::default())
        .await
        .unwrap()
        .children;
    let file = f.dir.path().join("file.mp4");
    std::fs::write(&file, b"0123456789").unwrap();
    f.sessions
        .set_local_source("host", move |_, _, _| Ok(file.clone()));
    f.registry
        .connected("host", "mediahost", "Fixture", "fixture-cert", "test");
    let (status, session) = f
        .request(
            "POST",
            "/api/v1/playback/sessions",
            json!({"library_id":library,"item_id":children[0].id,"mode":"direct"}),
            Some(&f.token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    assert_eq!(
        session["library_item_ids"],
        json!([children[0].id, children[1].id])
    );
    assert!(
        session.get("coverage").is_none(),
        "no invented episode boundaries"
    );
    let next = f
        .request(
            "GET",
            &format!(
                "/api/v1/catalogue/libraries/{library}/items/{}/next?media_entry_id={}",
                children[0].id,
                session["media_entry_id"].as_str().unwrap()
            ),
            Value::Null,
            Some(&f.token),
        )
        .await;
    assert_eq!(next.0, StatusCode::OK, "{:?}", next.1);
    assert_eq!(next.1["id"], children[2].id);
    let sid = session["session_id"].as_str().unwrap();
    let progress = format!("/api/v1/playback/sessions/{sid}/progress");
    assert_eq!(
        f.request(
            "POST",
            &progress,
            json!({"position_ms":600000}),
            Some(&f.token)
        )
        .await
        .0,
        StatusCode::OK
    );
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT item_id FROM catalogue_watch_state WHERE played=1 ORDER BY item_id",
    )
    .fetch_all(f.registry.db())
    .await
    .unwrap();
    assert_eq!(rows, vec![children[0].id.clone(), children[1].id.clone()]);
    let next = f
        .request(
            "GET",
            "/api/v1/catalogue/up-next",
            Value::Null,
            Some(&f.token),
        )
        .await;
    assert_eq!(next.1["items"][0]["id"], children[2].id, "{:?}", next.1);
    f.sessions.end(sid).await;
}

#[tokio::test]
async fn skip_segments_follow_the_selected_medium_and_multipart_timeline() {
    let f = Fixture::new().await;
    let store = f.registry.catalogue();
    store.put_mediahost("host", "Fixture").await.unwrap();
    let (collection, _) = store
        .offer_collection("host", &offer("movies", 8))
        .await
        .unwrap();
    let paths = [
        "Film.2000.1080p.mkv",
        "Film.2000.720p.mkv",
        "Film.2000.CD1.mkv",
        "Film.2000.CD2.mkv",
    ];
    let mut change = delta("movies", 8, paths[0], true, true);
    change.records.clear();
    for (index, path) in paths.iter().enumerate() {
        let version = index as u64 * 2 + 1;
        let mut file = delta("movies", version, path, false, true)
            .records
            .remove(0);
        let mut payload = p::FileUpsert::decode(file.payload.as_slice()).unwrap();
        payload.files[0].streams_json =
            json!({"container":"mp4", "duration_ms":600000}).to_string();
        let fact = p::SegmentDetectionResult {
            detector: kahawai_core::segments::DETECTOR_GENERATION,
            collection_id: "movies".into(),
            episodes: vec![p::SegmentEpisodeResult {
                source: payload.files[0].source.clone(),
                observed_size: 10,
                observed_mtime_unix: 1,
                segments: vec![p::DetectedSegment {
                    kind: "intro".into(),
                    start_ms: 5000 + index as u64 * 10000,
                    end_ms: 25000 + index as u64 * 10000,
                    analyzer: "chromaprint".into(),
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        file.payload = payload.encode_to_vec();
        let key = file.key.clone();
        change.records.push(file);
        change.records.push(p::CatalogRecord {
            version: version + 1,
            kind: "file_segments".into(),
            key,
            payload: fact.encode_to_vec(),
            deleted: false,
        });
    }
    store.apply_catalogue("host", &change).await.unwrap();
    let library = store
        .create_library("Films", MediaType::Movies, &[collection])
        .await
        .unwrap();
    let items = store.browse(&library, 0, 10).await.unwrap();
    assert_eq!(items.len(), 1);
    let item = &items[0].id;
    let snapshot = store.playback_item(&library, item).await.unwrap();
    assert_eq!(snapshot.renditions.len(), 3);
    let file = f.dir.path().join("video.mp4");
    std::fs::write(&file, b"0123456789").unwrap();
    f.sessions
        .set_local_source("host", move |_, _, _| Ok(file.clone()));
    f.registry
        .connected("host", "mediahost", "Fixture", "fixture-cert", "test");
    let mut captured = None;
    for rendition in snapshot.renditions {
        let expected: Vec<u64> = rendition
            .files
            .iter()
            .enumerate()
            .map(|(part, file)| {
                let index = paths.iter().position(|p| *p == file.path).unwrap();
                part as u64 * 600000 + 5000 + index as u64 * 10000
            })
            .collect();
        let body = json!({"library_id":library,"item_id":item,"media_entry_id":rendition.entry.id,"mode":"direct"});
        let (status, preview) = f
            .request(
                "QUERY",
                &format!("/api/v1/catalogue/libraries/{library}/items/{item}"),
                body.clone(),
                Some(&f.token),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{preview}");
        let starts = |v: &Value| {
            v["segments"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["start_ms"].as_u64().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(starts(&preview), expected);
        // The fixture contains arbitrary bytes: multipart start would require
        // an actual remux pipeline. Preview exercises its captured timeline.
        if rendition.files.len() > 1 {
            continue;
        }
        let (status, session) = f
            .request("POST", "/api/v1/playback/sessions", body, Some(&f.token))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{session}");
        assert_eq!(starts(&session), expected);
        if rendition.files[0].path == paths[0] {
            captured = f.sessions.get(session["session_id"].as_str().unwrap());
            continue;
        }
        f.request(
            "DELETE",
            &format!(
                "/api/v1/playback/sessions/{}",
                session["session_id"].as_str().unwrap()
            ),
            Value::Null,
            Some(&f.token),
        )
        .await;
    }
    // Replacing a physical file invalidates its observations, while its logical
    // library identity and other renditions remain intact.
    let mut replacement = delta("movies", 9, paths[0], false, true);
    let mut payload = p::FileUpsert::decode(replacement.records[0].payload.as_slice()).unwrap();
    payload.files[0].mtime_unix = 2;
    payload.files[0].streams_json = json!({"container":"mp4", "duration_ms":600000}).to_string();
    replacement.records[0].payload = payload.encode_to_vec();
    store.apply_catalogue("host", &replacement).await.unwrap();
    let snapshot = store.playback_item(&library, item).await.unwrap();
    let replaced = snapshot
        .renditions
        .iter()
        .find(|r| r.files[0].path == paths[0])
        .unwrap();
    let (status, preview) = f
        .request(
            "QUERY",
            &format!("/api/v1/catalogue/libraries/{library}/items/{item}"),
            json!({"media_entry_id":replaced.entry.id,"mode":"direct"}),
            Some(&f.token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["segments"], json!([]));
    assert_eq!(captured.unwrap().catalogue.segments[0].start_ms, 5000);
    // Facts from obsolete detector generations and failed analyses never become
    // skip buttons, even when they describe the current physical file version.
    for (index, failed) in [false, true].into_iter().enumerate() {
        let version = 10 + index as u64;
        let mut fact =
            p::SegmentDetectionResult::decode(change.records[1].payload.as_slice()).unwrap();
        fact.episodes[0].observed_mtime_unix = 2;
        if failed {
            fact.episodes[0].error = "analysis failed".into();
        } else {
            fact.detector += 1;
        }
        let mut update = delta("movies", version, paths[0], false, true);
        update.records[0].kind = "file_segments".into();
        update.records[0].payload = fact.encode_to_vec();
        store.apply_catalogue("host", &update).await.unwrap();
        let (status, preview) = f
            .request(
                "QUERY",
                &format!("/api/v1/catalogue/libraries/{library}/items/{item}"),
                json!({"media_entry_id":replaced.entry.id,"mode":"direct"}),
                Some(&f.token),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(preview["segments"], json!([]));
    }
}

#[tokio::test]
async fn catalogue_exposes_selected_and_linked_video_ids_in_the_correct_namespace() {
    for (kind, path, namespace) in [
        (MediaType::Movies, "Film.2000.mkv", "movie"),
        (MediaType::Series, "Show.S01E02.mkv", "show"),
        (MediaType::Anime, "Film.2000.mkv", "movie"),
    ] {
        let f = Fixture::new().await;
        let store = f.registry.catalogue();
        store.put_mediahost("host", "Fixture").await.unwrap();
        let mut offered = offer("video", 1);
        offered.media_type = kind.as_str().into();
        let (collection, _) = store.offer_collection("host", &offered).await.unwrap();
        store
            .apply_catalogue("host", &delta("video", 1, path, true, true))
            .await
            .unwrap();
        let library = store
            .create_library("Video", kind, std::slice::from_ref(&collection))
            .await
            .unwrap();
        let copy = store.collection_items(&collection).await.unwrap().remove(0);
        let record = |provider: &str, namespace: &str, id: &str| kahawai_mediadb::ProviderRecord {
            children: None,
            provider: provider.into(),
            namespace: namespace.into(),
            external_id: id.into(),
            language: "en".into(),
            media_type: kind,
            title: copy.detected.title.clone(),
            year: copy.detected.year,
            description: Default::default(),
        };
        let native = store
            .put_provider_record(&record("fixture", namespace, "native"))
            .await
            .unwrap();
        let tmdb = store
            .put_provider_record(&record("tmdb", namespace, "1234"))
            .await
            .unwrap();
        let tvdb = store
            .put_provider_record(&record(
                "tvdb",
                if namespace == "movie" {
                    "show"
                } else {
                    "movie"
                },
                "5678",
            ))
            .await
            .unwrap();
        store
            .assign_metadata(&copy.id, Some(&native))
            .await
            .unwrap();
        store
            .set_supplements(&copy.id, &[tmdb.clone(), tvdb])
            .await
            .unwrap();
        let item = store
            .collection_items(&collection)
            .await
            .unwrap()
            .remove(0)
            .library_item_id;
        let id = if kind == MediaType::Series {
            kahawai_mediadb::ChildId {
                parent: item,
                position: kahawai_mediadb::ChildPosition::Episode {
                    season: Some(1),
                    episode: 2,
                },
            }
            .encode()
        } else {
            item
        };
        let url = format!("/api/v1/catalogue/libraries/{library}/items/{id}");
        let (status, detail) = f.request("GET", &url, Value::Null, Some(&f.token)).await;
        assert_eq!(status, StatusCode::OK, "{detail}");
        assert_eq!(detail["tmdb_id"], 1234);
        assert!(
            detail["tvdb_id"].is_null(),
            "wrong namespace must not become a community lookup: {detail}"
        );
        store.assign_metadata(&copy.id, Some(&tmdb)).await.unwrap();
        let (status, detail) = f.request("GET", &url, Value::Null, Some(&f.token)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["tmdb_id"], 1234);
    }
}

#[derive(Default)]
struct SubtitleProviderFixture {
    downloads: std::sync::atomic::AtomicUsize,
    hashes: std::sync::Mutex<Vec<Option<u64>>>,
}
#[async_trait::async_trait]
impl kahawai_hub::opensubtitles::SubtitleProvider for SubtitleProviderFixture {
    fn name(&self) -> &'static str {
        "opensubtitles"
    }
    fn quota(&self) -> kahawai_hub::opensubtitles::Quota {
        Default::default()
    }
    async fn search(
        &self,
        query: &kahawai_hub::opensubtitles::SearchQuery,
    ) -> anyhow::Result<Vec<kahawai_hub::opensubtitles::Candidate>> {
        self.hashes.lock().unwrap().push(query.moviehash);
        Ok(vec![kahawai_hub::opensubtitles::Candidate {
            provider: self.name(),
            file_id: "fixture".into(),
            language: Some("eng".into()),
            release_name: Some("Selected release".into()),
            hash_match: query.moviehash.is_some(),
            downloads: 1,
            uploader: None,
            rating: None,
            fps: None,
        }])
    }
    async fn download(&self, file: &str) -> anyhow::Result<kahawai_hub::opensubtitles::Downloaded> {
        self.downloads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let ass = "[Script Info]\nScriptType: v4.00+\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:00.00,0:00:01.00,Default,,0,0,0,,Only this release\n";
        Ok(kahawai_hub::opensubtitles::Downloaded {
            bytes: if file == "ass" {
                ass.as_bytes().to_vec()
            } else {
                b"1\n00:00:00,000 --> 00:00:01,000\nOnly this release\n".to_vec()
            },
            format: if file == "ass" { "ass" } else { "srt" }.into(),
            release_name: Some("Selected release".into()),
        })
    }
}

#[tokio::test]
async fn downloaded_subtitles_follow_source_versions_not_titles() {
    downloaded_subtitle_case("srt").await;
}
#[tokio::test]
async fn downloaded_ass_uses_captured_text_and_accepts_negative_track_choices() {
    downloaded_subtitle_case("ass").await;
}
async fn downloaded_subtitle_case(format: &str) {
    let provider = Arc::new(SubtitleProviderFixture::default());
    let f = Fixture::with_provider(Some(provider.clone())).await;
    let store = f.registry.catalogue();
    store.put_mediahost("host", "Fixture").await.unwrap();
    let mut collections = vec![];
    let mut changes = vec![];
    for (name, hash) in [("release-a", 123), ("release-b", 456)] {
        let (collection, _) = store
            .offer_collection("host", &offer(name, 1))
            .await
            .unwrap();
        let mut change = delta(name, 1, "Dark.City.1998.mp4", true, true);
        let mut upsert = p::FileUpsert::decode(change.records[0].payload.as_slice()).unwrap();
        upsert.files[0].oshash = hash;
        upsert.files[0].streams_json = json!({"container":"mp4","duration_ms":600000}).to_string();
        change.records[0].payload = upsert.encode_to_vec();
        store.apply_catalogue("host", &change).await.unwrap();
        collections.push(collection);
        changes.push(change);
    }
    let library = store
        .create_library("Films", MediaType::Movies, &collections)
        .await
        .unwrap();
    let item = store.collection_items(&collections[0]).await.unwrap()[0]
        .library_item_id
        .clone();
    let playback = store.playback_item(&library, &item).await.unwrap();
    assert_eq!(playback.renditions.len(), 2);
    let sources: Vec<_> = playback
        .renditions
        .iter()
        .map(|r| json!({"media_entry_id":r.entry.id,"source_version":r.source_version()}))
        .collect();
    let route = format!("/api/v1/catalogue/libraries/{library}/items/{item}");
    let file = f.dir.path().join("video.mp4");
    std::fs::write(&file, b"0123456789").unwrap();
    f.sessions.set_local_source("host", move |_, _, path| {
        assert!(!path.starts_with("mediadb-download:"));
        Ok(file.clone())
    });
    f.registry
        .connected("host", "mediahost", "Fixture", "fixture-cert", "test");
    let (status, result) = f
        .request(
            "POST",
            &format!("{route}/subtitles/search"),
            json!({"source":sources[0],"languages":["eng"]}),
            Some(&f.token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(
        *provider.hashes.lock().unwrap(),
        vec![playback.renditions[0].files[0].oshash]
    );
    let body = json!({"source":sources[0],"file_id":format,"language":"eng"});
    let download_route = format!("{route}/subtitles/download");
    let (first, second) = tokio::join!(
        f.request("POST", &download_route, body.clone(), Some(&f.token)),
        f.request("POST", &download_route, body.clone(), Some(&f.token))
    );
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    assert_eq!(second.0, StatusCode::OK, "{}", second.1);
    let track = first.1["track_id"].as_i64().unwrap();
    assert!(track < 0);
    assert_eq!(second.1["track_id"], track);
    assert_eq!(
        provider.downloads.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    for (i, source) in sources.iter().enumerate() {
        let (status, preview) = f
            .request(
                "QUERY",
                &route,
                json!({"mode":"direct","media_entry_id":source["media_entry_id"]}),
                Some(&f.token),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{preview}");
        assert_eq!(preview["subtitle_source"], *source);
        assert_eq!(
            preview["negotiated"]["subtitles"].as_array().unwrap().len(),
            usize::from(i == 0),
            "{preview}"
        );
    }
    let (status, session) = f.request("POST", "/api/v1/playback/sessions", json!({"library_id":library,"item_id":item,"media_entry_id":sources[0]["media_entry_id"],"mode":"direct","subtitle_track":track}), Some(&f.token)).await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    let sid = session["session_id"].as_str().unwrap();
    let (status, error) = f
        .request(
            "POST",
            &format!("/api/v1/playback/sessions/{sid}/seek"),
            json!({"position_ms":0,"subtitle_track":-999999}),
            Some(&f.token),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    if format == "ass" {
        let response = f
            .api
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/v1/playback/sessions/{sid}/subtitles/{track}.ass"
                    ))
                    .header("authorization", format!("Bearer {}", f.token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("Dialogue:"));
    }
    // A different release cannot remove the acquired asset.
    let (_, removed) = f
        .request(
            "DELETE",
            &format!("{route}/subtitles/{track}"),
            sources[1].clone(),
            Some(&f.token),
        )
        .await;
    assert_eq!(removed["removed"], false);
    let rendition = &playback.renditions[0];
    assert!(
        !store
            .remove_downloaded_subtitle(
                -track,
                &rendition.entry.id,
                &rendition.source_version(),
                "someone-else",
                false
            )
            .await
            .unwrap()
    );
    // Reopening the database preserves the asset and its original source binding.
    let reopened = kahawai_hub::db::open_catalogue(f.dir.path()).await.unwrap();
    let read = reopened.playback_item(&library, &item).await.unwrap();
    assert_eq!(
        read.renditions
            .iter()
            .map(|r| r.downloaded_subtitles.len())
            .sum::<usize>(),
        1
    );
    drop(reopened);
    // Correcting metadata moves the copy, not its source-owned subtitle.
    let record = store
        .put_provider_record(&kahawai_mediadb::ProviderRecord {
            children: None,
            provider: "fixture".into(),
            namespace: "movie".into(),
            external_id: "matrix".into(),
            language: "en".into(),
            media_type: MediaType::Movies,
            title: "The Matrix".into(),
            year: Some(1999),
            description: Default::default(),
        })
        .await
        .unwrap();
    store
        .assign_metadata(&rendition.entry.item_id, Some(&record))
        .await
        .unwrap();
    let moved = store
        .enrichment_input(&rendition.entry.item_id)
        .await
        .unwrap()
        .library_item_id;
    assert_ne!(moved, item);
    let corrected = store.playback_item(&library, &moved).await.unwrap();
    assert_eq!(corrected.renditions[0].downloaded_subtitles[0].id, -track);
    store
        .assign_metadata(&rendition.entry.item_id, None)
        .await
        .unwrap();
    // Replacing the source invalidates even a search result that was already open.
    let index = playback.renditions[0].collection_id.clone();
    let n = changes
        .iter()
        .position(|c| c.collection_id == index)
        .unwrap();
    let mut change = changes[n].clone();
    change.snapshot = false;
    change.through_version = 2;
    change.records[0].version = 2;
    let mut upsert = p::FileUpsert::decode(change.records[0].payload.as_slice()).unwrap();
    upsert.files[0].mtime_unix += 1;
    change.records[0].payload = upsert.encode_to_vec();
    store.apply_catalogue("host", &change).await.unwrap();
    let (status, result) = f
        .request("POST", &download_route, body, Some(&f.token))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{result}");
    assert_eq!(
        provider.downloads.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert!(
        store
            .playback_item(&library, &item)
            .await
            .unwrap()
            .renditions
            .iter()
            .all(|r| r.downloaded_subtitles.is_empty())
    );
    assert!(
        store
            .remove_downloaded_subtitle(
                -track,
                &rendition.entry.id,
                &rendition.source_version(),
                "admin",
                true
            )
            .await
            .unwrap()
    );
    // Session bytes are captured: deletion/replacement cannot switch its subtitles.
    let response = f
        .api
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/playback/sessions/{}/subtitles/{track}.vtt",
                    session["session_id"].as_str().unwrap()
                ))
                .header("authorization", format!("Bearer {}", f.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("Only this release"));
}

#[tokio::test]
async fn library_rescan_uses_committed_membership_and_negotiated_deep_support() {
    let f = Fixture::new().await;
    let store = f.registry.catalogue();
    let mut members = Vec::new();
    for host in ["host", "offline", "older"] {
        store
            .offer_catalogue(
                host,
                host,
                &p::CatalogOffer {
                    collections: vec![offer("movies", 0), offer("excluded", 0)],
                },
            )
            .await
            .unwrap();
        members.push(
            store
                .collections(host)
                .await
                .unwrap()
                .into_iter()
                .find(|c| c.remote_id == "movies")
                .unwrap()
                .id,
        );
    }
    let library = store
        .create_library("Rescan", MediaType::Movies, &members)
        .await
        .unwrap();
    let empty = store
        .create_library("Empty", MediaType::Movies, &[])
        .await
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let (old_tx, mut old_rx) = tokio::sync::mpsc::channel(8);
    f.registry
        .register_link("host", tx, kahawai_proto::PROTOCOL_MINOR, 0);
    f.registry.register_link("older", old_tx, 1, 0);
    let path = format!("/admin/v1/catalogue/libraries/{library}/refresh");
    assert_eq!(
        f.request("POST", &path, json!({}), None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert!(rx.try_recv().is_err());
    for deep in [false, true] {
        let (status, body) = f
            .request(
                "POST",
                &format!("{path}?deep={deep}"),
                json!({}),
                Some(&f.token),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"asked": if deep {1} else {2}, "offline":1,"unsupported":usize::from(deep)})
        );
        let p::hub_to_host::Msg::RescanRequest(request) = receive(&mut rx).await else {
            panic!("expected rescan")
        };
        assert_eq!(request.collection_id, "movies");
        assert_eq!(request.deep, deep);
        assert!(
            rx.try_recv().is_err(),
            "excluded collection was not scanned"
        );
        if deep {
            assert!(
                old_rx.try_recv().is_err(),
                "old host must not silently downgrade deep intent"
            );
        } else {
            let p::hub_to_host::Msg::RescanRequest(request) = receive(&mut old_rx).await else {
                panic!("expected rescan")
            };
            assert!(!request.deep);
        }
    }
    assert_eq!(
        f.request(
            "POST",
            &format!("/admin/v1/catalogue/libraries/{empty}/refresh"),
            json!({}),
            Some(&f.token)
        )
        .await
        .1,
        json!({"asked":0,"offline":0,"unsupported":0})
    );
    assert_eq!(
        f.request(
            "POST",
            "/admin/v1/catalogue/libraries/missing/refresh",
            json!({}),
            Some(&f.token)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn segment_admin_reports_live_sources_without_a_manual_trigger() {
    let f = Fixture::new().await;
    let store = f.registry.catalogue();
    for host in ["host", "offline"] {
        let mut shows = offer("shows", 0);
        shows.media_type = "series".into();
        let mut anime = offer("anime", 0);
        anime.media_type = "anime".into();
        store
            .offer_catalogue(
                host,
                host,
                &p::CatalogOffer {
                    collections: vec![shows, anime, offer("movies", 0)],
                },
            )
            .await
            .unwrap();
    }
    f.registry
        .connected("host", "mediahost", "Fixture", "fixture-cert", "test");
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let (generation, _) = f
        .registry
        .register_link("host", tx, kahawai_proto::PROTOCOL_MINOR, 0);
    let path = "/admin/v1/segments";
    assert_eq!(
        f.request("GET", path, json!({}), None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.request("POST", path, json!({}), None).await.0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    let (code, status) = f.request("GET", path, json!({}), Some(&f.token)).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        status["collections"].as_array().unwrap().len(),
        6,
        "movies included for loudness"
    );
    assert!(
        status["collections"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["pending_sources"].is_null())
    );
    f.registry.report_discovery(
        "host",
        generation,
        p::DiscoveryStatus {
            collection_id: "shows".into(),
            pending_segments: 7,
            segments_enabled: Some(true),
            ..Default::default()
        },
    );
    f.registry.report_discovery(
        "host",
        generation,
        p::DiscoveryStatus {
            collection_id: "anime".into(),
            pending_segments: 3,
            ..Default::default()
        },
    );
    let (_, status) = f.request("GET", path, json!({}), Some(&f.token)).await;
    let rows = status["collections"].as_array().unwrap();
    assert!(
        rows.iter()
            .any(|c| c["name"] == "shows" && c["pending_sources"] == 7 && c["enabled"] == true)
    );
    assert!(
        rows.iter()
            .any(|c| c["name"] == "anime" && c["pending_sources"] == 3 && c["enabled"].is_null())
    );
    assert_eq!(
        f.request("POST", path, json!({}), Some(&f.token)).await.0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert!(
        rx.try_recv().is_err(),
        "status reads never trigger discovery"
    );
    let (new_tx, _new_rx) = tokio::sync::mpsc::channel(8);
    f.registry
        .register_link("host", new_tx, kahawai_proto::PROTOCOL_MINOR, 0);
    f.registry.report_discovery(
        "host",
        generation,
        p::DiscoveryStatus {
            collection_id: "shows".into(),
            pending_segments: 99,
            ..Default::default()
        },
    );
    assert!(
        f.registry.discovery_status("host", "shows").is_none(),
        "old report cannot cross a reconnect"
    );
}

#[tokio::test]
async fn matching_search_aggregates_anime_movie_results_and_provider_failures() {
    use kahawai_mediadb as m;
    let f = Fixture::new().await;
    let store = f.registry.catalogue();
    let mut collection = offer("anime", 1);
    collection.media_type = "anime".into();
    store
        .offer_catalogue(
            "host",
            "Fixture",
            &p::CatalogOffer {
                collections: vec![collection],
            },
        )
        .await
        .unwrap();
    store
        .apply_catalogue(
            "host",
            &delta("anime", 1, "Batman.Gotham.Night.mkv", true, true),
        )
        .await
        .unwrap();
    let collection = store.collections("host").await.unwrap()[0].id.clone();
    let (status, _) = f
        .request(
            "POST",
            "/admin/v1/catalogue/libraries",
            json!({"name":"Animost","media_type":"anime","collection_ids":[collection]}),
            Some(&f.token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let input = store
        .enrichment_items(None, None, false, "Batman", 0, 10)
        .await
        .unwrap();
    let input = store.enrichment_input(&input[0].id).await.unwrap();
    let entries = store.media_entries(&input.item_id).await.unwrap();
    assert!(matches!(entries[0].data.kind, m::EntryKind::Movie));
    store
        .set_provider_order(
            m::MediaType::Anime,
            &["tmdb".into(), "tvdb".into(), "anidb".into()],
        )
        .await
        .unwrap();
    for provider in ["tmdb", "tvdb"] {
        f.registry
            .credentials()
            .unwrap()
            .set_provider(
                kahawai_hub::secrets::HUB,
                provider,
                &std::collections::BTreeMap::from([("api_key", "fixture")]),
            )
            .await
            .unwrap();
    }
    let title = "Batman: Gotham Knight";
    let old_question = serde_json::to_string(&json!([
        entries.iter().map(|e| &e.data.kind).collect::<Vec<_>>(),
        [],
        "anime",
        title,
        null,
        input.artist,
        null,
        []
    ]))
    .unwrap();
    let question = serde_json::to_string(&("movie", &old_question)).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let answer = m::EnrichmentAnswer {
        candidates: vec![m::EnrichmentCandidate {
            record: m::ProviderRecord {
                children: None,
                provider: "tmdb".into(),
                namespace: "movie".into(),
                external_id: "13851".into(),
                language: "en".into(),
                media_type: m::MediaType::Movies,
                title: title.into(),
                year: Some(2008),
                description: Default::default(),
            },
            strength: 0,
            complete: false,
            links: vec![],
        }],
        ..Default::default()
    };
    store
        .put_cache_answer(
            "tmdb",
            &old_question,
            &serde_json::to_string(&m::EnrichmentAnswer::default()).unwrap(),
            now,
        )
        .await
        .unwrap();
    store
        .put_cache_answer(
            "tmdb",
            &question,
            &serde_json::to_string(&answer).unwrap(),
            now,
        )
        .await
        .unwrap();
    store
        .put_cache_answer("tvdb", &question, "broken fixture answer", now)
        .await
        .unwrap();
    let path = format!("/admin/v1/enrich/items/{}/candidates", input.item_id);
    let body = json!({"revision":input.revision,"query":title});
    assert_eq!(
        f.request("POST", &path, body.clone(), None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.request(
            "POST",
            &path,
            json!({"revision":-1,"query":title}),
            Some(&f.token)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (status, result) = f.request("POST", &path, body, Some(&f.token)).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(
        result["detail"]["candidates"][0]["record"]["namespace"], "movie",
        "{result}"
    );
    assert_eq!(result["detail"]["candidates"][0]["record"]["title"], title);
    assert!(result["identities"].is_array());
    assert_eq!(result["errors"].as_object().unwrap().len(), 1, "{result}");
    assert!(result["errors"]["tvdb"].is_string());
}
