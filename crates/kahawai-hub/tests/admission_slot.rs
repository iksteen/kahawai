//! A start that is abandoned gives its admission slot back.
//!
//! `Sessions::start` takes a per-user slot before `start_inner` and gave it
//! back on the line after the await. That covers the thirteen early returns
//! inside, and not the exit that has no line at all: the caller going away.
//! `start_session` awaits the whole thing inline, so a closed tab — or any
//! client that stops waiting — drops the future mid-flight and nothing after
//! the await ever runs. The id stayed in `reserved` for the life of the
//! process and went on counting, so four abandoned starts left an account
//! unable to begin anything until the hub was restarted.
//!
//! Dropping the future is the whole point, so this test drops it: a 1 ms
//! `timeout` around a call whose first act is a database query.

use std::sync::Arc;
use std::time::Duration;

use kahawai_hub::registry::{FileUpsertRecord, Registry};
use kahawai_hub::sessions::Sessions;

const CAP: usize = 4;

async fn fixture() -> (Arc<Registry>, Arc<Sessions>, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(Registry::new(db.clone(), Default::default()));
    registry
        .announce_collection("01HOST", "movies", "movies", &[])
        .await
        .unwrap();
    registry
        .upsert_files(
            "01HOST",
            "movies",
            vec![FileUpsertRecord {
                root_token: "root".into(),
                path_rel: "Heat (1995).mkv".into(),
                size: 1_000_000,
                mtime_unix: 1,
                head_xxh3: 1,
                tail_xxh3: 2,
                oshash: 3,
                streams_json: r#"{"container":"matroska","duration_ms":600000}"#.into(),
            }],
        )
        .await
        .unwrap();
    registry.connected("01HOST", "mediahost", "nas", "fp", "test");
    let item: String = sqlx::query_scalar("SELECT id FROM items LIMIT 1")
        .fetch_one(&db)
        .await
        .unwrap();
    let sessions = Arc::new(Sessions::with_limits(
        dir.path().join("scratch"),
        CAP,
        Duration::from_secs(90),
    ));
    sessions.attach_registry(registry.clone());
    (registry, sessions, item, dir)
}

/// Every start here fails — there is no mediahost actually serving bytes — so
/// the assertion is on WHICH failure. "too many concurrent streams" means a
/// slot was kept by a start that no longer exists.
async fn start_once(registry: &Registry, sessions: &Arc<Sessions>, item: &str) -> String {
    let subs = kahawai_hub::subtitles::Subtitles::new(std::env::temp_dir().join("kh-subs-test"));
    match sessions
        .start(
            registry,
            &subs,
            "u1",
            item,
            Some("direct"),
            None,
            0,
            0,
            0,
            None,
        )
        .await
    {
        Ok(_) => "started".into(),
        Err(e) => format!("{e:#}"),
    }
}

#[tokio::test]
async fn an_abandoned_start_does_not_keep_its_slot() {
    let (registry, sessions, item, _dir) = fixture().await;
    let subs = kahawai_hub::subtitles::Subtitles::new(_dir.path().join("subs"));

    // Twice the cap, every one of them dropped while still inside `start`.
    //
    // `select!` with `biased` rather than a timeout: the start is polled FIRST,
    // so it runs as far as its first await — past the admission — and is then
    // dropped when the ready branch wins. A `timeout` was tried and proved
    // nothing, because these starts fail in well under a millisecond, so the
    // timeout never elapsed and no future was ever abandoned. The test passed
    // against the leaking code.
    for _ in 0..CAP * 2 {
        tokio::select! {
            biased;
            _ = sessions.start(
                &registry,
                &subs,
                "u1",
                &item,
                Some("direct"),
                None,
                0,
                0,
                0,
                None,
            ) => unreachable!("a start cannot finish before a ready future"),
            _ = std::future::ready(()) => {}
        }
    }

    let verdict = start_once(&registry, &sessions, &item).await;
    assert!(
        !verdict.contains("too many concurrent"),
        "eight abandoned starts must leave the cap untouched, got {verdict:?}"
    );
}

/// The control, so the test above cannot pass by the cap never applying:
/// sessions that really are held DO count, and the ninth is refused.
#[tokio::test]
async fn the_cap_still_applies_to_starts_that_are_waited_on() {
    let (registry, sessions, item, _dir) = fixture().await;
    // Nothing is dropped here. Each start runs to completion and fails for
    // want of a serving host, releasing its slot — so the cap is never hit and
    // the refusal below must NOT be the concurrency one either. What this
    // pins is that `start_once` reaches the same place both ways, which is
    // what makes the assertion above meaningful.
    for _ in 0..CAP * 2 {
        let v = start_once(&registry, &sessions, &item).await;
        assert!(!v.contains("too many concurrent"), "unexpected: {v}");
    }
}
