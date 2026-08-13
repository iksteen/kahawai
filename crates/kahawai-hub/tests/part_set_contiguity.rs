//! A multi-part source is played whole or not at all.
//!
//! `base_ms` accumulates over the parts a session is built from, so a set with
//! a hole becomes a shorter film whose timeline starts at zero — CD2 rebased to
//! nothing, the seekbar reading 20:00 of a 45-minute film, and 20 minutes in
//! really an hour and twenty. Since a lost mediahost now ends its sessions and
//! the client restarts by itself, nothing human is watching for that.
//!
//! The check this pins replaced a count of connected parts against a count of
//! part rows. Both were built from the same query, so it only ever caught a
//! disconnected host: a part with no row at all — renamed out of the fold,
//! reconciled off disk — was absent from both sides and passed.

use std::sync::Arc;

use kahawai_hub::registry::{FileUpsertRecord, Registry};
use kahawai_hub::sessions::{Sessions, SourceOffline};

fn rec(path: &str) -> FileUpsertRecord {
    FileUpsertRecord {
        root_token: "root".into(),
        path_rel: path.into(),
        size: 1_000_000,
        mtime_unix: 1,
        head_xxh3: 1,
        tail_xxh3: 2,
        oshash: 3,
        // A duration per part, because the assembly sums them into `base_ms`.
        streams_json: r#"{"container":"matroska","duration_ms":600000}"#.into(),
    }
}

/// An item built from `files`, each `(host, filename)`. Only hosts named in
/// `present` are marked connected.
async fn item_with(
    files: Vec<(&str, &str)>,
    present: &[&str],
) -> (Arc<Registry>, Arc<Sessions>, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(Registry::new(db.clone(), Default::default()));
    let hosts: std::collections::BTreeSet<&str> = files.iter().map(|(h, _)| *h).collect();
    for host in &hosts {
        registry
            .announce_collection(host, "movies", "movies", &[])
            .await
            .unwrap();
        registry
            .upsert_files(
                host,
                "movies",
                files
                    .iter()
                    .filter(|(h, _)| h == host)
                    .map(|(_, f)| rec(f))
                    .collect(),
            )
            .await
            .unwrap();
    }
    // No transport here: the only thing the source assembly asks about a host is
    // whether it is present.
    for host in present {
        registry.connected(host, "mediahost", "nas", "fp", "test");
    }
    // Asserted, not assumed: if this ever stopped taking, the gap case would go
    // on passing through the `by_part.is_empty()` bail instead — the right
    // answer for the wrong reason.
    for host in present {
        assert!(registry.is_connected(host), "{host} must count as present");
    }
    let items: Vec<String> = sqlx::query_scalar("SELECT id FROM items WHERE kind = 'movie'")
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(items.len(), 1, "the fixture must fold to exactly one item");
    let item = items.into_iter().next().unwrap();
    let sources: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM item_sources WHERE item_id = ?")
        .bind(&item)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(
        sources as usize,
        files.len(),
        "every file must be a source row"
    );
    let sessions = Arc::new(Sessions::new(dir.path().join("scratch")));
    sessions.attach_registry(registry.clone());
    (registry, sessions, item, dir)
}

/// `Sessions::start`, in whichever mode. FORCED goes straight at the source
/// assembly; NEGOTIATED goes through `candidate_sources`, which is the path the
/// web player uses for every video session because it sends no mode.
///
/// That distinction is the whole reason this signature has a mode: the first
/// version of this file forced `direct` everywhere, so all five cases exercised
/// `source_parts` directly and none of them touched the code a viewer reaches.
/// `candidate_sources` was swallowing the refusal below with `let Ok(..)`, and
/// the tests could not see it.
async fn refusal_in(mode: Option<&str>, files: Vec<(&str, &str)>, present: &[&str]) -> String {
    let (registry, sessions, item, _dir) = item_with(files, present).await;
    let subs = kahawai_hub::subtitles::Subtitles::new(_dir.path().join("subs"));
    let started = sessions
        .start(&registry, &subs, "u1", &item, mode, None, 0, 0, 0, None)
        .await;
    // `Session` has no Debug, so unwrap the error by hand rather than expect_err.
    let err = match started {
        Ok(_) => panic!("no real mediahost is serving, so this cannot succeed"),
        Err(e) => e,
    };
    // `SourceOffline` is the stand-by signal: the item is fine, wait for a host.
    // Anything else is a different refusal, and the message says which.
    if err.downcast_ref::<SourceOffline>().is_some() {
        "source-offline".into()
    } else {
        format!("{err:#}")
    }
}

/// The forced path, which is what the music player uses.
async fn refusal(files: Vec<(&str, &str)>, present: &[&str]) -> String {
    refusal_in(Some("direct"), files, present).await
}

/// The negotiated path: no mode, which is every video session the web player
/// starts.
async fn negotiated(files: Vec<(&str, &str)>, present: &[&str]) -> String {
    refusal_in(None, files, present).await
}

#[tokio::test]
async fn a_hole_is_permanent_on_the_path_the_player_uses() {
    // The regression fence for the swallowed error. A 503 here is the stand-by
    // signal, and the client retries that on a five-second interval with no
    // ceiling — so a film with a missing middle disc stood by for ever, which
    // is the exact failure the contiguity check was added to prevent.
    let verdict = negotiated(
        vec![
            ("01HOST", "Gappy (1999) - CD1.avi"),
            ("01HOST", "Gappy (1999) - CD3.avi"),
        ],
        &["01HOST"],
    )
    .await;
    assert!(
        verdict.contains("incomplete"),
        "a hole must refuse permanently when negotiating too, got {verdict:?}"
    );
}

#[tokio::test]
async fn a_whole_part_set_negotiates() {
    // The control for the test above: the same path, nothing missing, must NOT
    // be refused — otherwise the assertion could pass on any old failure.
    let verdict = negotiated(
        vec![
            ("01HOST", "Whole (1999) - CD1.avi"),
            ("01HOST", "Whole (1999) - CD2.avi"),
        ],
        &["01HOST"],
    )
    .await;
    assert!(
        !verdict.contains("incomplete"),
        "a complete set must get past the source assembly, got {verdict:?}"
    );
}

#[tokio::test]
async fn a_part_whose_host_is_away_still_stands_by_when_negotiating() {
    // And the transient case must stay transient on this path: a part behind an
    // absent host is a wait, not a refusal.
    let verdict = negotiated(
        vec![
            ("01AWAY", "Split (1999) - CD1.avi"),
            ("02HERE", "Split (1999) - CD2.avi"),
        ],
        &["02HERE"],
    )
    .await;
    assert_eq!(verdict, "source-offline", "an absent host is still a wait");
}

#[tokio::test]
async fn a_part_whose_host_is_away_stands_by() {
    // CD1 on a host that is gone, CD2 on one that is present. Transient: the
    // item is whole, so the answer is to wait.
    //
    // This is the regression fence for the check I nearly deleted. A contiguity
    // test alone sees {2}, a perfectly contiguous run of one, and would have
    // played CD2 rebased to zero.
    let verdict = refusal(
        vec![
            ("01AWAY", "Split (1999) - CD1.avi"),
            ("02HERE", "Split (1999) - CD2.avi"),
        ],
        &["02HERE"],
    )
    .await;
    assert_eq!(
        verdict, "source-offline",
        "a part behind an absent host is a wait"
    );
}

#[tokio::test]
async fn a_hole_with_every_host_present_is_permanent() {
    // CD1 and CD3 present and connected, CD2 never existed. Nothing to wait
    // for, so this must NOT be the stand-by signal — the client retries that
    // on an interval with no ceiling.
    let verdict = refusal(
        vec![
            ("01HOST", "Gappy (1999) - CD1.avi"),
            ("01HOST", "Gappy (1999) - CD3.avi"),
        ],
        &["01HOST"],
    )
    .await;
    assert!(
        verdict.contains("incomplete"),
        "a hole is a permanent refusal, got {verdict:?}"
    );
}

#[tokio::test]
async fn a_missing_beginning_is_permanent() {
    // CD2 and CD3, no CD1 anywhere. Contiguous among themselves, so only the
    // `first > 1` half catches this — and without it two thirds of the film
    // would play rebased to zero.
    let verdict = refusal(
        vec![
            ("01HOST", "Headless (1999) - CD2.avi"),
            ("01HOST", "Headless (1999) - CD3.avi"),
        ],
        &["01HOST"],
    )
    .await;
    assert!(
        verdict.contains("incomplete"),
        "a set that does not start at the beginning is incomplete, got {verdict:?}"
    );
}

#[tokio::test]
async fn a_whole_part_set_is_assembled() {
    // The control: it gets past the source assembly and fails further on, for
    // want of a mediahost actually serving bytes.
    let verdict = refusal(
        vec![
            ("01HOST", "Whole (1999) - CD1.avi"),
            ("01HOST", "Whole (1999) - CD2.avi"),
        ],
        &["01HOST"],
    )
    .await;
    assert!(
        !verdict.contains("incomplete") && verdict != "source-offline",
        "a complete set must reach the lease, got {verdict:?}"
    );
}

#[tokio::test]
async fn a_lone_part_is_a_playable_film() {
    // A CD1 with no CD2 is one file that happens to carry a part number. An
    // earlier guard required more than one part and dropped it entirely, which
    // left the item with no candidates and answered stand-by for ever.
    let verdict = refusal(vec![("01HOST", "Solo (1999) - CD1.avi")], &["01HOST"]).await;
    assert!(
        !verdict.contains("incomplete") && verdict != "source-offline",
        "one part is a complete run of one, got {verdict:?}"
    );
}
