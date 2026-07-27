//! NFR-1/NFR-2: measure the hub against its stated targets rather than
//! asserting it feels quick.
//!
//! Run by hand — seeding a quarter of a million files takes longer than
//! anyone wants in CI:
//!
//! ```text
//! cargo test --release -p kahawai-hub --test scale_bench -- --ignored --nocapture
//! ```
//!
//! Release matters: a debug build measures rustc's bounds checks, not the
//! hub. The numbers below are wall-clock through the real router, so they
//! include SQL, the resolved-metadata view and JSON serialisation — which
//! is what a client actually waits for.
//!
//! The targets, from the requirements:
//!   NFR-1  browse responses <= 200 ms at 50k items
//!   NFR-2  >= 250k files across all collections, >= 10 mediahosts

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

const MEDIAHOSTS: usize = 10;

struct Bench {
    api: axum::Router,
    token: String,
    library: String,
    db: sqlx::SqlitePool,
}

/// Seed `items` top-level movies spread over `MEDIAHOSTS` collections,
/// one file each, every one matched and described — the expensive shape,
/// not an empty catalogue.
async fn seed(dir: &std::path::Path, items: usize) -> Bench {
    let db = kahawai_hub::db::open(dir).await.unwrap();
    let registry = Arc::new(kahawai_hub::registry::Registry::new(db.clone(), Default::default()));
    let auth = Arc::new(kahawai_hub::auth::Auth::new(db.clone(), dir).await.unwrap());

    let library = "01BENCHLIBRARY0000000000".to_string();
    sqlx::query("INSERT INTO libraries (id, name, media_type) VALUES (?, 'bench', 'movies')")
        .bind(&library)
        .execute(&db)
        .await
        .unwrap();

    let t0 = Instant::now();
    let mut tx = db.begin().await.unwrap();
    for m in 0..MEDIAHOSTS {
        let module = format!("01BENCHMODULE{m:011}");
        let collection = format!("c{m}");
        sqlx::query(
            "INSERT INTO satellites (module_id, module_type, name, cert_fingerprint,
                                     enrolled_at, disabled)
             VALUES (?, 'mediahost', ?, '', unixepoch(), 0)",
        )
        .bind(&module)
        .bind(format!("host{m}"))
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO collections (module_id, collection_id, media_type, roots_json, sync_version)
             VALUES (?, ?, 'movies', '[\"/m\"]', 1)",
        )
        .bind(&module)
        .bind(&collection)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO library_collections (library_id, module_id, collection_id)
             VALUES (?, ?, ?)",
        )
        .bind(&library)
        .bind(&module)
        .bind(&collection)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    for n in 0..items {
        let id = format!("01BENCHITEM{n:015}");
        let m = n % MEDIAHOSTS;
        let module = format!("01BENCHMODULE{m:011}");
        let collection = format!("c{m}");
        let path = format!("Film {n} (2020).mkv");
        sqlx::query(
            "INSERT INTO items (id, kind, title, norm_title, year) VALUES (?, 'movie', ?, ?, 2020)",
        )
        .bind(&id)
        .bind(format!("Film {n}"))
        .bind(format!("film {n}"))
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO files (module_id, collection_id, path_rel, size, mtime_unix,
                                head_xxh3, tail_xxh3, oshash, streams_json, subs_extracted)
             VALUES (?, ?, ?, 1000000, 1, 0, 0, 0, '{}', 0)",
        )
        .bind(&module)
        .bind(&collection)
        .bind(&path)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO item_sources (item_id, module_id, collection_id, path_rel)
             VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&module)
        .bind(&collection)
        .bind(&path)
        .execute(&mut *tx)
        .await
        .unwrap();
        // Two providers each, so the resolved view has something to
        // choose between — a catalogue with one answer per item would
        // flatter the read path.
        for (provider, pid) in [("tmdb", n), ("tvdb", n + 1_000_000)] {
            sqlx::query(
                "INSERT INTO provider_metadata
                   (item_id, provider, provider_id, title, overview, poster_path, rating,
                    premiered, genres, confidence, updated_at)
                 VALUES (?, ?, ?, ?, 'A synopsis long enough to be worth serialising.',
                         '/poster.jpg', 7.5, '2020-01-01', '[\"Drama\"]', 'auto', unixepoch())",
            )
            .bind(&id)
            .bind(provider)
            .bind(pid.to_string())
            .bind(format!("Film {n}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO item_match (item_id, provider, provider_id, media_type, manual, updated_at)
             VALUES (?, 'tmdb', ?, 'movies', 0, unixepoch())",
        )
        .bind(&id)
        .bind(n.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    eprintln!("  seeded {items} items over {MEDIAHOSTS} collections in {:?}", t0.elapsed());

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
    let enricher = Arc::new(kahawai_hub::enrich::Enricher::new(dir.to_path_buf()));
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
        .complete_setup(&auth.setup_token().unwrap(), "bench", "hunter22222hunter")
        .await
        .unwrap()
        .access_token;
    Bench { api, token, library, db }
}

impl Bench {
    /// Best of `runs`, so one scheduler hiccup does not decide a verdict.
    async fn time(&self, uri: &str, runs: usize) -> (std::time::Duration, usize) {
        let mut best = std::time::Duration::MAX;
        let mut bytes = 0;
        for _ in 0..runs {
            let t = Instant::now();
            let resp = self
                .api
                .clone()
                .oneshot(
                    Request::get(uri)
                        .header("authorization", format!("Bearer {}", self.token))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(resp.status().is_success(), "{uri} -> {}", resp.status());
            let b = axum::body::to_bytes(resp.into_body(), 1 << 30).await.unwrap();
            best = best.min(t.elapsed());
            bytes = b.len();
        }
        (best, bytes)
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "seeds a quarter-million rows; run by hand"]
async fn browse_latency_and_scale() {
    for items in [50_000usize, 250_000] {
        let dir = tempfile::tempdir().unwrap();
        eprintln!("\n=== {items} items");
        let b = seed(dir.path(), items).await;

        let files: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM files").fetch_one(&b.db).await.unwrap();
        let (whole, bytes) = b.time("/api/v1/items", 3).await;
        let (scoped, _) =
            b.time(&format!("/api/v1/items?library={}", b.library), 3).await;
        let (detail, _) =
            b.time(&format!("/api/v1/items/01BENCHITEM{:015}", items / 2), 5).await;

        eprintln!("  files             {files}");
        eprintln!("  GET /items        {:>8.1} ms  ({:.1} MB)", whole.as_secs_f64() * 1e3,
                  bytes as f64 / 1e6);
        eprintln!("  GET /items?library{:>8.1} ms", scoped.as_secs_f64() * 1e3);
        eprintln!("  GET /items/{{id}}   {:>8.1} ms", detail.as_secs_f64() * 1e3);
        let t = Instant::now();
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM items i JOIN resolved_metadata m ON m.item_id = i.id",
        )
        .fetch_one(&b.db)
        .await
        .unwrap();
        eprintln!("  view over {n:>6}    {:>8.1} ms  (SQL only, no serialisation)",
                  t.elapsed().as_secs_f64() * 1e3);

        // NFR-1 states the browse target at 50k. Recorded at 250k too,
        // because NFR-2 asks the shape to hold there, and a number that
        // is merely printed is a number nobody notices moving.
        if items == 50_000 && std::env::var("BENCH_REPORT_ONLY").is_err() {
            assert!(
                scoped.as_millis() <= 200,
                "NFR-1: browse at 50k took {:.1} ms, target 200 ms",
                scoped.as_secs_f64() * 1e3
            );
        }
        assert!(detail.as_millis() <= 200, "item detail should not scale with catalogue size");
    }
}
