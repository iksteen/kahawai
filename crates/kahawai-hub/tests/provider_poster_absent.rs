//! A provider with no artwork is an answer, not a failure.
//!
//! Cover Art Archive 404s for any release group nobody has uploaded a
//! sleeve for, which is the ordinary case for obscure records. That 404
//! used to travel as an `Err`, so the client got a 500 whose body quoted
//! the upstream URL: a server error for a record with no cover, and the
//! provider's own address handed to whoever asked (SEC-WEB-7).
//!
//! What must stay an error is anything that might not be true a minute
//! later — a 5xx, a timeout, a refused connection. Losing that
//! distinction would be worse than the bug: every provider outage would
//! silently mark the whole library as having no artwork.

use kahawai_hub::enrich::Enricher;

/// A stand-in provider: one path with no image, one that is broken, one
/// that answers.
async fn provider() -> String {
    provider_counting(std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0))).await
}

/// The same, with the number of requests for `absent.jpg` visible to the
/// caller — the only way to see whether a miss was remembered.
async fn provider_counting(hits: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> String {
    let app = axum::Router::new()
        .route(
            "/absent.jpg",
            axum::routing::get(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (axum::http::StatusCode::NOT_FOUND, "not found")
                }
            }),
        )
        .route(
            "/gone.jpg",
            axum::routing::get(|| async { (axum::http::StatusCode::GONE, "gone") }),
        )
        .route(
            "/broken.jpg",
            axum::routing::get(|| async {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "upstream is having a day",
                )
            }),
        )
        .route(
            "/there.jpg",
            axum::routing::get(|| async { [1u8, 2, 3, 4].to_vec() }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

#[tokio::test]
async fn no_poster_is_none_and_a_broken_provider_is_still_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let enricher = Enricher::new(dir.path().to_path_buf());
    let base = provider().await;

    // The bug: this was an Err carrying the URL, and became a 500.
    assert!(
        enricher
            .fetch_poster(&format!("{base}/absent.jpg"))
            .await
            .expect("a provider with no image is not a failure")
            .is_none()
    );
    // Gone means the same thing as far as a poster is concerned.
    assert!(
        enricher
            .fetch_poster(&format!("{base}/gone.jpg"))
            .await
            .expect("410 is also just 'no image here'")
            .is_none()
    );

    // Still an error, and this is the half that matters: if a provider
    // outage read as "no artwork", every poster in the library would
    // quietly disappear until someone noticed.
    let err = enricher
        .fetch_poster(&format!("{base}/broken.jpg"))
        .await
        .expect_err("a 500 from a provider is our problem, not an answer");
    assert!(
        err.to_string().contains("500"),
        "the log still gets the detail: {err}"
    );

    assert_eq!(
        enricher
            .fetch_poster(&format!("{base}/there.jpg"))
            .await
            .unwrap(),
        Some(vec![1u8, 2, 3, 4]),
        "and a poster that exists still comes back"
    );
}

/// A remembered miss is not asked for again.
///
/// Nothing was written for a provider that has no poster, so every request for
/// a coverless release was an outbound fetch — and those pass through the
/// per-host gate one at a time, seconds apart. A shelf of coverless records,
/// re-rendered on every scroll-back and doubled by the srcset, could hold that
/// queue at saturation for as long as somebody kept browsing: enrichment for
/// the whole hub waits behind it, and the stricter providers start counting
/// towards a ban.
#[tokio::test]
async fn a_provider_miss_is_remembered_for_an_hour() {
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let base = provider_counting(hits.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let art = kahawai_hub::artwork::Artwork::new(
        dir.path().to_path_buf(),
        std::sync::Arc::new(Enricher::new(dir.path().join("enrich"))),
    );
    let poster = format!("{base}/absent.jpg");

    // Ten renders of the same coverless card, the way a shelf produces them.
    for _ in 0..10 {
        assert!(
            art.remote_poster_for_test(&poster).await.unwrap().is_none(),
            "no poster is still no poster"
        );
    }
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the provider must be asked once, not once per render"
    );
}
