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
    let registry = Arc::new(kahawai_hub::registry::Registry::new(
        db.clone(),
        Default::default(),
    ));
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
            "INSERT INTO items(id,kind,title,norm_title,year,module_id,collection_id)
             VALUES(?,'movie',?,?,2020,?,?)",
        )
        .bind(&id)
        .bind(format!("Film {n}"))
        .bind(format!("film {n}"))
        .bind(&module)
        .bind(&collection)
        .execute(&mut *tx)
        .await
        .unwrap();
        let file_id: i64 = sqlx::query_scalar(
            "INSERT INTO files(module_id,collection_id,path_rel,size,mtime_unix,
                                head_xxh3,tail_xxh3,oshash,streams_json,subs_extracted)
             VALUES(?,?,?,1000000,1,0,0,0,'{}',0) RETURNING id",
        )
        .bind(&module)
        .bind(&collection)
        .bind(&path)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        kahawai_hub::registry::bind_file_to_item(&mut tx, file_id, &id)
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
        // No item_match insert: the answers above are the input, and the
        // assignment derives itself. Seeding it by hand now collides
        // with what the triggers already wrote.
    }
    tx.commit().await.unwrap();
    eprintln!(
        "  seeded {items} items over {MEDIAHOSTS} collections in {:?}",
        t0.elapsed()
    );

    let sessions = Arc::new(kahawai_hub::sessions::Sessions::new(
        tempfile::tempdir().unwrap().keep(),
    ));
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
        Arc::new(kahawai_hub::subtitles::Subtitles::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        Arc::new(kahawai_hub::artwork::Artwork::new(
            tempfile::tempdir().unwrap().keep(),
            enricher.clone(),
        )),
        enricher,
        kahawai_hub::api::NetOptions::default(),
    );
    auth.complete_setup("bench", "hunter22222hunter")
        .await
        .unwrap();
    let token = auth
        .login("bench", "hunter22222hunter")
        .await
        .unwrap()
        .access_token;
    Bench {
        api,
        token,
        library,
        db,
    }
}

impl Bench {
    /// WORST of `runs`, and every run is printed.
    ///
    /// This used to report the best "so one scheduler hiccup does not
    /// decide a verdict", and that is exactly how a real defect stayed
    /// hidden: the browse query was bimodal — 253 ms or 50 ms for the
    /// same statement, depending on which pooled connection served it —
    /// and best-of-N reported the good mode every time. A latency target
    /// is a promise about the slow case, so the slow case is what gets
    /// asserted.
    /// The same timing for `QUERY`, which is a different question with a
    /// different price: it parses every candidate source's `streams_json`
    /// and asks the fleet once per candidate. GET is a few indexed reads;
    /// this is negotiation. Measured beside it because QUERY became the
    /// item page's load path, so a regression here is felt on every
    /// detail view, not on a rare call.
    async fn time_query(
        &self,
        uri: &str,
        body: &str,
        runs: usize,
    ) -> (std::time::Duration, std::time::Duration) {
        let mut worst = std::time::Duration::ZERO;
        let mut best = std::time::Duration::MAX;
        let mut all = Vec::new();
        for _ in 0..runs {
            let t = Instant::now();
            let resp = self
                .api
                .clone()
                .oneshot(
                    Request::builder()
                        .method("QUERY")
                        .uri(uri)
                        .header("authorization", format!("Bearer {}", self.token))
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(resp.status().is_success(), "{uri} -> {}", resp.status());
            axum::body::to_bytes(resp.into_body(), 1 << 30)
                .await
                .unwrap();
            let took = t.elapsed();
            all.push(format!("{:.1}", took.as_secs_f64() * 1e3));
            worst = worst.max(took);
            best = best.min(took);
        }
        eprintln!("      QUERY {uri}  runs: {} ms", all.join(", "));
        (worst, best)
    }

    async fn time(&self, uri: &str, runs: usize) -> (std::time::Duration, usize) {
        let mut worst = std::time::Duration::ZERO;
        let mut all = Vec::new();
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
            let b = axum::body::to_bytes(resp.into_body(), 1 << 30)
                .await
                .unwrap();
            let took = t.elapsed();
            all.push(format!("{:.1}", took.as_secs_f64() * 1e3));
            worst = worst.max(took);
            bytes = b.len();
        }
        // Printed, not summarised: a spread between runs is itself a
        // finding, and averaging it away is what cost a day.
        eprintln!("      {uri}  runs: {} ms", all.join(", "));
        (worst, bytes)
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "seeds a quarter-million rows; run by hand"]
async fn browse_latency_and_scale() {
    let mut missed = Vec::new();
    for items in [50_000usize, 250_000] {
        // KEEP_BENCH_DB=/path leaves the seeded database behind so a plan
        // can be read against the real thing rather than a smaller stand-in.
        let keep = std::env::var("KEEP_BENCH_DB")
            .ok()
            .map(|p| format!("{p}-{items}"));
        let dir = tempfile::tempdir().unwrap();
        if let Some(k) = &keep {
            std::fs::create_dir_all(k).unwrap();
        }
        eprintln!("\n=== {items} items");
        let path: &std::path::Path = keep
            .as_ref()
            .map(std::path::Path::new)
            .unwrap_or(dir.path());
        let b = seed(path, items).await;

        let files: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM files")
            .fetch_one(&b.db)
            .await
            .unwrap();
        let (whole, bytes) = b.time("/api/v1/items", 3).await;
        let (scoped, _) = b
            .time(&format!("/api/v1/items?library={}", b.library), 3)
            .await;
        // The page a user actually lands on, and one deep in the middle:
        // an OFFSET still walks the rows it skips, so the last page is
        // the honest worst case, not the first.
        let (deep, _) = b
            .time(
                &format!("/api/v1/items?library={}&offset={}", b.library, items - 200),
                3,
            )
            .await;
        let (search, _) = b
            .time(
                &format!("/api/v1/items?library={}&q=film+1234", b.library),
                3,
            )
            .await;
        // The adversarial search: every seeded title contains "film", so
        // this needle matches the entire catalogue — the page streams but
        // the count must still visit everything.
        let (search_dense, _) = b
            .time(&format!("/api/v1/items?library={}&q=film", b.library), 3)
            .await;

        let (detail, _) = b
            .time(&format!("/api/v1/items/01BENCHITEM{:015}", items / 2), 5)
            .await;
        let (neg_worst, neg_best) = b
            .time_query(
                &format!("/api/v1/items/01BENCHITEM{:015}", items / 2),
                r#"{"profile":{"containers":["mp4"],"video":[{"codec":"h264"}],
                    "audio":["aac"],"hdr":false,
                    "graphics_overlay":true,"ass_render":true,
                    "target_duration":{"mode":"accurate"}}}"#,
                5,
            )
            .await;

        eprintln!("  files             {files}");
        eprintln!(
            "  GET /items        {:>8.1} ms  ({:.1} MB)",
            whole.as_secs_f64() * 1e3,
            bytes as f64 / 1e6
        );
        eprintln!(
            "  GET /items?library{:>8.1} ms  (first page)",
            scoped.as_secs_f64() * 1e3
        );
        eprintln!("  ...last page      {:>8.1} ms", deep.as_secs_f64() * 1e3);
        eprintln!("  ...search         {:>8.1} ms", search.as_secs_f64() * 1e3);
        eprintln!(
            "  ...search (dense) {:>8.1} ms",
            search_dense.as_secs_f64() * 1e3
        );

        eprintln!(
            "  GET /items/{{id}}   {:>8.1} ms",
            detail.as_secs_f64() * 1e3
        );
        // Worst AND steady, because the spread is the whole finding: the
        // FIRST query in a process dry-run-probes the encoders through
        // GStreamer (`ass_burn_available`, ~1 s, memoized after), and
        // every later one is a handful of indexed reads plus a pure
        // `pick_transcoder`. Reporting either number alone lies — the
        // same measurement reads as "285x GET" or "0.1x GET".
        //
        // No ratio against the GET line above: that one is a worst-of-5
        // carrying its own warm-up, so dividing them compares a cold
        // number by a warm one. And these seeded items have ONE source
        // and no subtitle tracks, so this bounds the per-source work
        // rather than exercising it — a multi-source item pays
        // `candidate_sources` per candidate.
        eprintln!(
            "  QUERY /items/{{id}} {:>8.1} ms first / {:.1} ms steady",
            neg_worst.as_secs_f64() * 1e3,
            neg_best.as_secs_f64() * 1e3,
        );

        // The write side of the trigger-driven pick. A reorder recomputes
        // every item of the media type — accepted deliberately, so the
        // number belongs on the record rather than in an argument.
        let t = Instant::now();
        kahawai_hub::providers::set_chain(&b.db, "movies", &["tvdb".into(), "tmdb".into()])
            .await
            .unwrap();
        eprintln!(
            "  chain reorder     {:>8.1} ms  (re-picks every item)",
            t.elapsed().as_secs_f64() * 1e3
        );

        // And the hottest write path in the system: a rescan announcing
        // sources in bulk. The trigger's WHEN guard is what keeps this
        // from paying for a pick per row.
        let t = Instant::now();
        let mut tx = b.db.begin().await.unwrap();
        for n in 0..1000 {
            let file_id: i64 = sqlx::query_scalar(
                "INSERT INTO files(module_id,collection_id,path_rel,size,mtime_unix,
                                   head_xxh3,tail_xxh3,oshash,streams_json)
                 VALUES(?,?,?,1000000,1,0,0,0,'{}') RETURNING id",
            )
            .bind(format!("01BENCHMODULE{:011}", n % MEDIAHOSTS))
            .bind(format!("c{}", n % MEDIAHOSTS))
            .bind(format!("extra {n}.mkv"))
            .fetch_one(&mut *tx)
            .await
            .unwrap();
            kahawai_hub::registry::bind_file_to_item(
                &mut tx,
                file_id,
                &format!("01BENCHITEM{n:015}"),
            )
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();
        eprintln!(
            "  1000 explicit sources {:>8.1} ms  (scan path)",
            t.elapsed().as_secs_f64() * 1e3
        );

        let t = Instant::now();
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM items i JOIN resolved_metadata m ON m.item_id = i.id",
        )
        .fetch_one(&b.db)
        .await
        .unwrap();
        eprintln!(
            "  view over {n:>6}    {:>8.1} ms  (SQL only, no serialisation)",
            t.elapsed().as_secs_f64() * 1e3
        );

        // The enrichment pass's standing tax: the question-gated
        // selection at the top of every run (provider_queries, 0044).
        // Quiescent — every item holds real answers — it must decide
        // "nothing to do" near-free at catalogue scale.
        let mut sel_idle = std::time::Duration::ZERO;
        let mut runs_q = Vec::new();
        for _ in 0..5 {
            let t = Instant::now();
            let rows = sqlx::query(kahawai_hub::enrich::GENERIC_SELECTION_SQL)
                .bind(kahawai_hub::providers::QUERY_REV)
                // both searchers configured: the bench times the worst case
                .bind(r#"["tmdb","tvdb"]"#)
                .fetch_all(&b.db)
                .await
                .unwrap();
            let took = t.elapsed();
            assert!(
                rows.is_empty(),
                "quiescent selection must be empty, got {}",
                rows.len()
            );
            runs_q.push(format!("{:.1}", took.as_secs_f64() * 1e3));
            sel_idle = sel_idle.max(took);
        }
        eprintln!(
            "  selection (idle)  {:>8.1} ms  runs: {}",
            sel_idle.as_secs_f64() * 1e3,
            runs_q.join(", ")
        );

        // With work owed: strip 1000 items' answers — no question rows
        // exist for them, so exactly those must surface as due.
        sqlx::query(
            "DELETE FROM provider_metadata
              WHERE item_id < '01BENCHITEM000000000001000'",
        )
        .execute(&b.db)
        .await
        .unwrap();
        let mut sel_due = std::time::Duration::ZERO;
        let mut runs_d = Vec::new();
        for _ in 0..5 {
            let t = Instant::now();
            let rows = sqlx::query(kahawai_hub::enrich::GENERIC_SELECTION_SQL)
                .bind(kahawai_hub::providers::QUERY_REV)
                .bind(r#"["tmdb","tvdb"]"#)
                .fetch_all(&b.db)
                .await
                .unwrap();
            let took = t.elapsed();
            assert_eq!(rows.len(), 1000, "exactly the stripped items are due");
            runs_d.push(format!("{:.1}", took.as_secs_f64() * 1e3));
            sel_due = sel_due.max(took);
        }
        eprintln!(
            "  selection (1000 due){:>6.1} ms  runs: {}",
            sel_due.as_secs_f64() * 1e3,
            runs_d.join(", ")
        );

        // NFR-1 states the target at 50k; recorded at BOTH sizes, since
        // NFR-2 asks the shape to hold at 250k and a page should not care
        // how much is behind it.
        //
        // Collected, not asserted here. Failing inside the loop meant one
        // miss at 50k stopped the 250k run from happening at all — so a
        // benchmark whose whole job is producing numbers produced none
        // for the size that was hardest to reach.
        for (what, took) in [
            ("first page", scoped),
            ("last page", deep),
            ("search", search),
            ("dense search", search_dense),
            ("item detail", detail),
        ] {
            if took.as_millis() > 200 {
                missed.push(format!(
                    "NFR-1: {what} at {items} items took {:.1} ms, target 200 ms",
                    took.as_secs_f64() * 1e3
                ));
            }
        }
        // Selection is background work, so its target is a pathology
        // tripwire, not a latency promise. Measured 2026-07-28 (release,
        // worst of 5): 50k idle 496 ms, 250k idle 2377 ms — LINEAR in
        // catalogue size (point probes per item on the pm/q PKs, plan
        // verified), bimodal on pooled-connection page cache (~590 ms
        // warm at 250k). The tripwire sits ~2x above the measured worst:
        // a quadratic step at these sizes lands in the tens of seconds
        // and cannot hide under it. If the standing tax itself ever
        // matters, the lever is a partial index
        // (provider_metadata(provider, item_id) WHERE provider_id <> '')
        // to drive from the answered side — unbuilt, no need yet.
        let (idle_limit, due_limit) = if items > 100_000 {
            (5_000u128, 5_500)
        } else {
            (1_200, 1_300)
        };
        for (what, took, limit) in [
            ("selection idle", sel_idle, idle_limit),
            ("selection 1000 due", sel_due, due_limit),
        ] {
            if took.as_millis() > limit {
                missed.push(format!(
                    "{what} at {items} items took {:.1} ms, tripwire {limit} ms",
                    took.as_secs_f64() * 1e3
                ));
            }
        }
    }
    assert!(missed.is_empty(), "{}", missed.join("\n"));
}
