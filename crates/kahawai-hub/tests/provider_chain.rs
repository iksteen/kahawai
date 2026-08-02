//! HUB-5: first-claim-wins at field granularity, and what a reorder
//! does. The point of storing every provider's answer is that
//! precedence can be re-decided locally — these tests are what keeps
//! that property true.

use kahawai_hub::providers::{Fields, chain_in_force, media_type_of_item, set_chain, store_answer};
use sqlx::Row;
use sqlx::SqlitePool;

async fn item(db: &SqlitePool, id: &str) {
    sqlx::query("INSERT INTO items (id, kind, title, norm_title) VALUES (?, 'movie', ?, ?)")
        .bind(id)
        .bind(id)
        .bind(id)
        .execute(db)
        .await
        .unwrap();
}

async fn merged(
    db: &SqlitePool,
    id: &str,
) -> (String, Option<String>, Option<String>, Option<f64>) {
    let r = sqlx::query(
        "SELECT provider, title, overview, rating FROM resolved_metadata WHERE item_id = ?",
    )
    .bind(id)
    .fetch_one(db)
    .await
    .unwrap();
    (
        r.get("provider"),
        r.get("title"),
        r.get("overview"),
        r.get("rating"),
    )
}

fn answer(title: &str, overview: Option<&str>, rating: Option<f64>) -> Fields {
    Fields {
        title: Some(title.into()),
        overview: overview.map(str::to_string),
        rating,
        ..Default::default()
    }
}

#[tokio::test]
async fn earlier_provider_wins_a_field_and_later_ones_fill_the_holes() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    let chain = chain_in_force(&db, "movies").await;
    assert_eq!(chain, vec!["tmdb", "tvdb"]);

    // TMDB matched, but has no synopsis and no rating for this one.
    store_answer(
        &db,
        "i1",
        "tmdb",
        "550",
        "auto",
        answer("Fight Club", None, None),
    )
    .await
    .unwrap();
    // TVDB, ranked below, has a title AND the synopsis TMDB lacked.
    store_answer(
        &db,
        "i1",
        "tvdb",
        "77",
        "auto",
        answer(
            "Fight Club (1999)",
            Some("A ticking-time-bomb insomniac..."),
            Some(8.4),
        ),
    )
    .await
    .unwrap();

    let (provider, title, overview, rating) = merged(&db, "i1").await;
    assert_eq!(provider, "tmdb", "identity belongs to the first to match");
    assert_eq!(title.as_deref(), Some("Fight Club"), "TMDB's title stands");
    assert!(overview.is_some(), "TVDB filled the gap TMDB left");
    assert_eq!(rating, Some(8.4), "and the rating too");
}

#[tokio::test]
async fn reordering_re_decides_ownership_without_asking_anyone() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    store_answer(
        &db,
        "i1",
        "tmdb",
        "550",
        "auto",
        answer("TMDB title", Some("tmdb"), None),
    )
    .await
    .unwrap();
    store_answer(
        &db,
        "i1",
        "tvdb",
        "77",
        "auto",
        answer("TVDB title", Some("tvdb"), None),
    )
    .await
    .unwrap();
    assert_eq!(merged(&db, "i1").await.1.as_deref(), Some("TMDB title"));

    // Rank TVDB above TMDB. No provider is contacted: the answers are
    // already on disk, which is the whole reason this is affordable.
    set_chain(&db, "movies", &["tvdb".into(), "tmdb".into()])
        .await
        .unwrap();
    let (provider, title, overview, _) = merged(&db, "i1").await;
    assert_eq!(
        title.as_deref(),
        Some("TVDB title"),
        "the reorder took effect"
    );
    assert_eq!(overview.as_deref(), Some("tvdb"));
    assert_eq!(provider, "tvdb", "identity follows the order too");

    // And back again — nothing was lost in the first merge.
    set_chain(&db, "movies", &["tmdb".into(), "tvdb".into()])
        .await
        .unwrap();
    assert_eq!(merged(&db, "i1").await.1.as_deref(), Some("TMDB title"));
}

#[tokio::test]
async fn a_manual_pick_outranks_every_provider_and_survives_reorders() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    store_answer(
        &db,
        "i1",
        "tmdb",
        "550",
        "auto",
        answer("Robot Wars", None, None),
    )
    .await
    .unwrap();
    // A human corrected it to the TVDB record.
    kahawai_hub::providers::assign_manual(
        &db,
        "i1",
        "tvdb",
        "77",
        answer("Robot Wars (1993)", None, None),
    )
    .await
    .unwrap();
    assert_eq!(
        merged(&db, "i1").await.1.as_deref(),
        Some("Robot Wars (1993)")
    );

    // TMDB is ranked first and re-runs — the human's answer still wins.
    set_chain(&db, "movies", &["tmdb".into(), "tvdb".into()])
        .await
        .unwrap();
    let (provider, title, ..) = merged(&db, "i1").await;
    assert_eq!(
        title.as_deref(),
        Some("Robot Wars (1993)"),
        "a reorder cannot undo a human"
    );
    assert_eq!(
        provider, "tvdb",
        "and it stays attributed to the real service"
    );
}

#[tokio::test]
async fn a_stored_order_that_is_not_a_permutation_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    // Dropping a provider would silently disable it; adding an unknown
    // one would silently do nothing.
    assert!(set_chain(&db, "movies", &["tmdb".into()]).await.is_err());
    assert!(
        set_chain(&db, "movies", &["tmdb".into(), "imdb".into()])
            .await
            .is_err()
    );
    assert_eq!(chain_in_force(&db, "movies").await, vec!["tmdb", "tvdb"]);
}

#[tokio::test]
async fn an_items_media_type_comes_from_the_collection_it_lives_in() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    // No sources yet: everything enriches as movies/series by default.
    assert_eq!(media_type_of_item(&db, "i1").await, "movies");

    sqlx::query(
        "INSERT INTO satellites (module_id, module_type, name, cert_fingerprint)
         VALUES ('mh', 'mediahost', 'mh', 'fp')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO collections (module_id, collection_id, media_type)
         VALUES ('mh', 'c1', 'anime')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO item_sources (item_id, module_id, collection_id, path_rel)
         VALUES ('i1', 'mh', 'c1', 'a.mkv')",
    )
    .execute(&db)
    .await
    .unwrap();
    assert_eq!(media_type_of_item(&db, "i1").await, "anime");
    assert_eq!(
        chain_in_force(&db, "anime").await,
        vec!["anime", "tmdb", "tvdb"]
    );
}

/// The anime composite stores its half under `anilist`; it must still
/// rank where `anime` sits, or the tail would outrank the very provider
/// that identified the item.
#[tokio::test]
async fn the_anime_composites_answer_ranks_as_the_chain_entry() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    // The chain follows the item's own collection now, so the fixture has
    // to live in an anime one — passing a chain in is no longer possible,
    // which is the point: the caller cannot pick the wrong order.
    sqlx::query(
        "INSERT INTO satellites (module_id, module_type, name, cert_fingerprint)
         VALUES ('mh', 'mediahost', 'mh', 'fp')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO collections (module_id, collection_id, media_type)
         VALUES ('mh', 'c1', 'anime')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO item_sources (item_id, module_id, collection_id, path_rel)
         VALUES ('i1', 'mh', 'c1', 'a.mkv')",
    )
    .execute(&db)
    .await
    .unwrap();
    store_answer(
        &db,
        "i1",
        "anilist",
        "9253",
        "auto",
        answer("Steins;Gate", None, None),
    )
    .await
    .unwrap();
    store_answer(
        &db,
        "i1",
        "tmdb",
        "42509",
        "auto",
        answer("Steins Gate", Some("A rag-tag group..."), Some(8.5)),
    )
    .await
    .unwrap();

    let (provider, title, overview, rating) = merged(&db, "i1").await;
    assert_eq!(
        title.as_deref(),
        Some("Steins;Gate"),
        "AniList title outranks TMDB's"
    );
    assert_eq!(provider, "anilist", "identity stays with the anime chain");
    assert!(
        overview.is_some(),
        "but TMDB supplied the synopsis AniList lacked"
    );
    assert_eq!(rating, Some(8.5));
}

/// A provider that could not be reached is not a hole in the data: it
/// comes back due, with backoff, so a ban or a 429 costs a delay rather
/// than a permanently unenriched item.
#[tokio::test]
async fn an_unreachable_provider_is_rescheduled_not_dropped() {
    use kahawai_hub::providers::{due_items, reschedule, settled};
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;

    reschedule(&db, "i1", "anime", "anidb banned us").await;
    let row = sqlx::query(
        "SELECT attempts, due_at - unixepoch() AS in_s, reason
         FROM enrichment_queue WHERE item_id = 'i1' AND provider = 'anime'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("attempts"), 1);
    assert!(row.get::<String, _>("reason").contains("banned"));
    let first: i64 = row.get("in_s");
    assert!(
        first > 0 && first <= 900,
        "first retry is soon-ish: {first}s"
    );
    assert!(due_items(&db, 10).await.is_empty(), "not due yet");

    // Repeated refusals back off further, never faster.
    reschedule(&db, "i1", "anime", "anidb banned us").await;
    let second: i64 = sqlx::query_scalar(
        "SELECT due_at - unixepoch() FROM enrichment_queue WHERE item_id = 'i1'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(second > first, "backoff grew: {first} -> {second}");

    // Once it answers, the debt is cleared.
    settled(&db, "i1", "anime").await;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM enrichment_queue")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

/// Work that is due shows up for the next run to pick up.
#[tokio::test]
async fn due_work_is_offered_to_the_next_run() {
    use kahawai_hub::providers::due_items;
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    sqlx::query(
        "INSERT INTO enrichment_queue (item_id, provider, due_at, reason)
         VALUES ('i1', 'tvdb', unixepoch() - 60, 'backfill')",
    )
    .execute(&db)
    .await
    .unwrap();
    assert_eq!(due_items(&db, 10).await, vec!["i1".to_string()]);
}

/// Only the kinds the chain walks may be queued. An episode's
/// description comes from its show's episode pass, so a queue row for
/// one is work nothing picks up — it would sit permanently due.
#[tokio::test]
async fn episodes_are_never_left_queued() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    sqlx::query(
        "INSERT INTO items (id, kind, title, norm_title) VALUES ('ep', 'episode', 'e', 'e')",
    )
    .execute(&db)
    .await
    .unwrap();
    item(&db, "show1").await;
    for id in ["ep", "show1"] {
        sqlx::query(
            "INSERT INTO enrichment_queue (item_id, provider, due_at) VALUES (?, 'tmdb', unixepoch())",
        )
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
    }
    // The migration that closes this runs at open(); re-assert its rule
    // the way the schema does, so a future backfill cannot reintroduce it.
    sqlx::query(
        "DELETE FROM enrichment_queue
         WHERE item_id IN (SELECT id FROM items WHERE kind NOT IN ('movie','show','album'))",
    )
    .execute(&db)
    .await
    .unwrap();
    let left: Vec<String> = sqlx::query_scalar("SELECT item_id FROM enrichment_queue")
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(
        left,
        vec!["show1".to_string()],
        "episode work must not linger"
    );
}

/// Movies and series share a DEFAULT order, not a chain. Ranking TVDB
/// first for series must leave films untouched — the grouping in the
/// requirement is about the defaults being equal, nothing more.
#[tokio::test]
async fn series_has_its_own_chain_independent_of_movies() {
    use kahawai_hub::providers::{MEDIA_TYPES, chain_for, media_type_key};
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    assert!(MEDIA_TYPES.contains(&"series"));
    assert_eq!(media_type_key("series"), "series");
    assert_eq!(chain_for("series"), chain_for("movies"), "same default");
    // An unknown media type still enriches rather than having no chain.
    assert_eq!(media_type_key("podcasts"), "movies");

    // Seeded separately, and moving one leaves the other alone.
    assert_eq!(chain_in_force(&db, "series").await, vec!["tmdb", "tvdb"]);
    set_chain(&db, "series", &["tvdb".into(), "tmdb".into()])
        .await
        .unwrap();
    assert_eq!(chain_in_force(&db, "series").await, vec!["tvdb", "tmdb"]);
    assert_eq!(
        chain_in_force(&db, "movies").await,
        vec!["tmdb", "tvdb"],
        "reordering series must not touch films"
    );
}

/// Every provider answers, whatever the order — that is what makes
/// ranking a preference rather than a lottery over who was asked. A
/// complete row is no longer a reason to stop.
#[tokio::test]
async fn a_complete_row_does_not_stop_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    // TMDB answered completely.
    store_answer(
        &db,
        "i1",
        "tmdb",
        "550",
        "auto",
        Fields {
            title: Some("Fight Club".into()),
            overview: Some("tmdb synopsis".into()),
            poster_path: Some("/p.jpg".into()),
            rating: Some(8.4),
            premiered: Some("1999-10-15".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // TVDB is asked anyway and its answer is kept, ready to win if the
    // order changes later.
    store_answer(
        &db,
        "i1",
        "tvdb",
        "77",
        "auto",
        answer("TVDB title", Some("tvdb"), None),
    )
    .await
    .unwrap();
    assert_eq!(merged(&db, "i1").await.1.as_deref(), Some("Fight Club"));

    set_chain(&db, "movies", &["tvdb".into(), "tmdb".into()])
        .await
        .unwrap();
    let (provider, title, overview, rating) = merged(&db, "i1").await;
    assert_eq!(
        title.as_deref(),
        Some("TVDB title"),
        "the reorder now has data to pick"
    );
    assert_eq!(overview.as_deref(), Some("tvdb"));
    assert_eq!(provider, "tvdb");
    assert_eq!(rating, Some(8.4), "and TMDB still supplies what TVDB lacks");
}

/// A provider that matched only weakly may describe the item it
/// identified, but must not donate fields to somebody else's item: an
/// uncertain match filling a synopsis for the wrong film is worse than
/// no synopsis. Now that every provider is asked, this is the guard.
#[tokio::test]
async fn a_weak_non_owner_does_not_donate_fields() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    store_answer(
        &db,
        "i1",
        "tmdb",
        "550",
        "auto",
        answer("Solaris", None, None),
    )
    .await
    .unwrap();
    // TVDB matched something it is not sure about.
    store_answer(
        &db,
        "i1",
        "tvdb",
        "999",
        "weak",
        answer("Solaris (remake)", Some("wrong film's synopsis"), Some(3.0)),
    )
    .await
    .unwrap();
    let (provider, title, overview, rating) = merged(&db, "i1").await;
    assert_eq!(provider, "tmdb");
    assert_eq!(title.as_deref(), Some("Solaris"));
    assert_eq!(overview, None, "a weak stranger's synopsis stays out");
    assert_eq!(rating, None, "and its rating too");

    // Ranking it first does NOT hand it the item: rank breaks ties
    // between comparable matches, it does not promote a guess over a
    // certainty.
    set_chain(&db, "movies", &["tvdb".into(), "tmdb".into()])
        .await
        .unwrap();
    let (provider, _, overview, _) = merged(&db, "i1").await;
    assert_eq!(provider, "tmdb", "the confident match keeps the item");
    assert_eq!(
        overview, None,
        "and the weak stranger still donates nothing"
    );

    // It does own an item nobody else could identify, and then its own
    // data is all there is.
    item(&db, "i2").await;
    store_answer(&db, "i2", "tmdb", "", "miss", Fields::default())
        .await
        .unwrap();
    store_answer(
        &db,
        "i2",
        "tvdb",
        "999",
        "weak",
        answer("Obscure", Some("only guess"), None),
    )
    .await
    .unwrap();
    let (provider, _, overview, _) = merged(&db, "i2").await;
    assert_eq!(provider, "tvdb");
    assert_eq!(overview.as_deref(), Some("only guess"));
}

/// Rank decides between equals; it does not let a guess outrank a
/// certainty. Once every provider is asked, the top-ranked one will
/// sometimes have only a weak match for an item another identified
/// confidently — the confident one must keep the item.
#[tokio::test]
async fn a_confident_match_outranks_a_weak_one_whatever_the_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    store_answer(
        &db,
        "i1",
        "tmdb",
        "111",
        "weak",
        answer("Being Human?", None, None),
    )
    .await
    .unwrap();
    store_answer(
        &db,
        "i1",
        "tvdb",
        "222",
        "auto",
        answer("Being Human", Some("bbc"), None),
    )
    .await
    .unwrap();

    let (provider, title, overview, _) = merged(&db, "i1").await;
    assert_eq!(provider, "tvdb", "the certain match keeps the item");
    assert_eq!(title.as_deref(), Some("Being Human"));
    assert_eq!(overview.as_deref(), Some("bbc"));

    // A human's correction beats both, from either provider.
    kahawai_hub::providers::assign_manual(
        &db,
        "i1",
        "tmdb",
        "333",
        answer("Being Human (US)", None, None),
    )
    .await
    .unwrap();
    assert_eq!(merged(&db, "i1").await.0, "tmdb");
    assert_eq!(
        merged(&db, "i1").await.1.as_deref(),
        Some("Being Human (US)")
    );

    // A confident non-owner still fills what the manual pick lacks —
    // a hand-matched item should not lose the poster TVDB has.
    store_answer(
        &db,
        "i1",
        "tvdb",
        "222",
        "auto",
        Fields {
            poster_path: Some("/art.jpg".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let poster: Option<String> =
        sqlx::query_scalar("SELECT poster_path FROM resolved_metadata WHERE item_id = 'i1'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(poster.as_deref(), Some("/art.jpg"));
}

/// A confirmed year beats a silent one. TMDB carries a 1952 series
/// called "The Continental" with no air date at all, and it won an
/// exact-title match over "The Continental: From the World of John Wick"
/// (2023) — which is what the file plainly meant.
#[test]
fn a_stated_year_beats_an_exact_title_with_no_year() {
    use kahawai_hub::enrich::{Candidate, pick_candidate};
    let old_show = Candidate::for_test(19069, "The Continental", None);
    let right = Candidate::for_test(
        72710,
        "The Continental: From the World of John Wick",
        Some("2023-09-22"),
    );
    // TMDB returns the relevant one first, the exact-title one second.
    let cands = vec![right, old_show];
    let (picked, conf) = pick_candidate(&cands, "The Continental", Some(2023)).unwrap();
    assert_eq!(picked.id(), 72710, "the year the file states must decide");
    assert_eq!(conf, "auto");

    // With no year to go on, the exact title is still the best signal.
    let (picked, _) = pick_candidate(&cands, "The Continental", None).unwrap();
    assert_eq!(picked.id(), 19069);

    // A subtitle only counts at a separator: no swallowing neighbours.
    // Two candidates, so the lone-plausible-hit tier stays out of it.
    let officer = Candidate::for_test(1, "The Officer", Some("2023-01-01"));
    let unrelated = Candidate::for_test(2, "Something Else", Some("2023-01-01"));
    assert!(pick_candidate(&[officer, unrelated], "The Office", Some(2023)).is_none());
}

/// Two manual picks cannot coexist: the assignment is one row, so the
/// second choice replaces the first rather than tying with it. (The merge
/// this replaced tied on insertion order, so hand-picking the top-ranked
/// provider appeared to do nothing.)
#[tokio::test]
async fn a_second_manual_pick_replaces_the_first() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    kahawai_hub::providers::assign_manual(
        &db,
        "i1",
        "tvdb",
        "414734",
        answer("The Continental (2023)", None, None),
    )
    .await
    .unwrap();
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tvdb".into(), "414734".into(), true))
    );

    kahawai_hub::providers::assign_manual(
        &db,
        "i1",
        "tmdb",
        "72710",
        answer("The Continental: From the World of John Wick", None, None),
    )
    .await
    .unwrap();
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tmdb".into(), "72710".into(), true))
    );
    assert_eq!(
        merged(&db, "i1").await.1.as_deref(),
        Some("The Continental: From the World of John Wick")
    );
}

// ---------- HUB-5 take three: assignment, not merge ----------

/// (provider, provider_id, manual) of an item's assignment, if it has one.
async fn assigned(db: &SqlitePool, id: &str) -> Option<(String, String, bool)> {
    sqlx::query("SELECT provider, provider_id, manual FROM item_match WHERE item_id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .unwrap()
        .map(|r| {
            (
                r.get("provider"),
                r.get("provider_id"),
                r.get::<i64, _>("manual") != 0,
            )
        })
}

async fn answer_row(db: &SqlitePool, id: &str, provider: &str, pid: &str, strength: &str) {
    sqlx::query(
        "INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence, updated_at)
         VALUES (?, ?, ?, ?, ?, unixepoch())
         ON CONFLICT (item_id, provider) DO UPDATE SET
           provider_id = excluded.provider_id, confidence = excluded.confidence",
    )
    .bind(id)
    .bind(provider)
    .bind(pid)
    .bind(format!("{provider} says"))
    .bind(strength)
    .execute(db)
    .await
    .unwrap();
}

/// A strong match beats a weak one whatever the preference order says —
/// the order breaks ties between comparable matches, it does not promote a
/// guess over a certainty.
#[tokio::test]
async fn strong_beats_weak_across_the_preference_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    answer_row(&db, "i1", "tmdb", "111", "weak").await; // ranks FIRST for movies
    answer_row(&db, "i1", "tvdb", "222", "auto").await;
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tvdb".into(), "222".into(), false))
    );

    // Equal strength: now the order decides, and a reorder moves it.
    answer_row(&db, "i1", "tmdb", "111", "auto").await;
    assert_eq!(assigned(&db, "i1").await.unwrap().0, "tmdb");
    set_chain(&db, "movies", &["tvdb".into(), "tmdb".into()])
        .await
        .unwrap();
    assert_eq!(assigned(&db, "i1").await.unwrap().0, "tvdb");
}

/// The owner's rule, directly: a more preferred provider that gains info
/// later replaces the earlier automatic match.
#[tokio::test]
async fn a_later_preferred_answer_replaces_an_automatic_match() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    answer_row(&db, "i1", "tvdb", "222", "auto").await;
    assert_eq!(assigned(&db, "i1").await.unwrap().0, "tvdb");

    answer_row(&db, "i1", "tmdb", "111", "auto").await;
    assert_eq!(
        assigned(&db, "i1").await.unwrap().0,
        "tmdb",
        "the preferred provider takes over"
    );
}

/// Only misses, or nothing at all, means NO assignment row — absence is
/// the unmatched state, not a sentinel.
#[tokio::test]
async fn misses_leave_the_item_unassigned() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    assert_eq!(assigned(&db, "i1").await, None, "never asked");

    answer_row(&db, "i1", "tmdb", "", "miss").await;
    answer_row(&db, "i1", "tvdb", "", "miss").await;
    assert_eq!(assigned(&db, "i1").await, None, "asked, nothing found");
}

/// An answer that was only reachable through somebody else's mapped id
/// describes the item but may never own it — otherwise ranking TMDB first
/// would hand an anime item to a record reached *because* AniDB identified
/// it.
#[tokio::test]
async fn a_bridged_answer_never_owns_the_item() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    answer_row(&db, "i1", "anilist", "9253", "auto").await;
    answer_row(&db, "i1", "tmdb", "42509", "bridged").await;
    // Rank TMDB above the anime composite; the bridge still cannot win.
    set_chain(
        &db,
        "anime",
        &["tmdb".into(), "anime".into(), "tvdb".into()],
    )
    .await
    .unwrap();
    assert_eq!(assigned(&db, "i1").await.unwrap().0, "anilist");

    // And a bridged answer alone leaves the item unassigned.
    item(&db, "i2").await;
    answer_row(&db, "i2", "tmdb", "42509", "bridged").await;
    assert_eq!(assigned(&db, "i2").await, None);
}

/// A refused record is not a candidate; an item where everything has been
/// refused stays unassigned, and a NEW record is picked up automatically —
/// which is what "try again when something new pops up" means.
#[tokio::test]
async fn refused_records_are_skipped_until_something_new_appears() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    answer_row(&db, "i1", "tmdb", "19069", "auto").await;
    assert_eq!(assigned(&db, "i1").await.unwrap().1, "19069");

    sqlx::query(
        "INSERT INTO rejected_matches (item_id, provider, provider_id, rejected_at)
         VALUES ('i1', 'tmdb', '19069', unixepoch())",
    )
    .execute(&db)
    .await
    .unwrap();
    assert_eq!(
        assigned(&db, "i1").await,
        None,
        "the refused record is dropped"
    );
    // The answers themselves survive a rejection.
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM provider_metadata WHERE item_id='i1'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(kept, 1);

    // A different record from the same provider is fair game.
    answer_row(&db, "i1", "tmdb", "72710", "auto").await;
    assert_eq!(assigned(&db, "i1").await.unwrap().1, "72710");
}

/// Episodes and tracks never get an assignment: they follow their parent.
#[tokio::test]
async fn episodes_are_never_assigned() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    sqlx::query("INSERT INTO items (id, kind, title, norm_title) VALUES ('ep','episode','e','e')")
        .execute(&db)
        .await
        .unwrap();
    answer_row(&db, "ep", "tvdb", "999", "auto").await;
    assert_eq!(assigned(&db, "ep").await, None);
}

/// An assignment whose backing answer is downgraded to a miss is dropped,
/// not left pointing at nothing.
#[tokio::test]
async fn an_assignment_whose_answer_decays_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    answer_row(&db, "i1", "tmdb", "111", "auto").await;
    assert!(assigned(&db, "i1").await.is_some());

    answer_row(&db, "i1", "tmdb", "", "miss").await;
    assert_eq!(assigned(&db, "i1").await, None);
}

/// A human's pin outlives both a later answer from a more preferred
/// provider and a reorder.
#[tokio::test]
async fn a_manual_assignment_is_never_recomputed() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    kahawai_hub::providers::assign_manual(
        &db,
        "i1",
        "tvdb",
        "414734",
        Fields {
            title: Some("hand picked".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tvdb".into(), "414734".into(), true))
    );

    // TMDB ranks first and answers strongly: the pin still holds.
    answer_row(&db, "i1", "tmdb", "111", "auto").await;
    set_chain(&db, "movies", &["tmdb".into(), "tvdb".into()])
        .await
        .unwrap();
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tvdb".into(), "414734".into(), true))
    );
}

/// "Yes, that one" on an automatic match. It must survive exactly what a
/// pin survives — the point of confirming is that the guessing stops —
/// and it had no test at all, which is how the promotion could have been
/// left half-applied without anything noticing.
#[tokio::test]
async fn confirming_an_automatic_match_makes_it_as_durable_as_a_pin() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    answer_row(&db, "i1", "tvdb", "414734", "auto").await;
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tvdb".into(), "414734".into(), false))
    );

    kahawai_hub::providers::confirm_assignment(&db, "i1")
        .await
        .unwrap();
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tvdb".into(), "414734".into(), true))
    );
    // Confirming records the same pin a hand-picked match does — it is
    // the input the assignment is then derived from.
    assert_eq!(
        sqlx::query_as::<_, (String, String)>(
            "SELECT provider, provider_id FROM manual_match WHERE item_id = 'i1'"
        )
        .fetch_optional(&db)
        .await
        .unwrap(),
        Some(("tvdb".into(), "414734".into()))
    );

    // A better-ranked strong answer arrives afterwards. Before confirming
    // it would have taken the item; now it may only describe it.
    answer_row(&db, "i1", "tmdb", "111", "auto").await;
    set_chain(&db, "movies", &["tmdb".into(), "tvdb".into()])
        .await
        .unwrap();
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tvdb".into(), "414734".into(), true)),
        "a confirmed match is a human decision, not a strong guess"
    );
}

/// Rejecting keeps every answer on file — deleting them would make the
/// next run re-ask every provider, AniDB included, for one UI click — and
/// schedules the refused providers to be looked at again later.
#[tokio::test]
async fn rejecting_keeps_the_answers_and_schedules_another_look() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    answer_row(&db, "i1", "tmdb", "19069", "auto").await;
    answer_row(&db, "i1", "tvdb", "", "miss").await;
    assert!(assigned(&db, "i1").await.is_some());

    kahawai_hub::providers::reject_matches(&db, "i1")
        .await
        .unwrap();
    assert_eq!(
        assigned(&db, "i1").await,
        None,
        "no assignment after a rejection"
    );
    let answers: i64 =
        sqlx::query_scalar("SELECT count(*) FROM provider_metadata WHERE item_id = 'i1'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(answers, 2, "the answers, and the recorded miss, survive");
    let refused: i64 =
        sqlx::query_scalar("SELECT count(*) FROM rejected_matches WHERE item_id = 'i1'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(refused, 1, "only records that exist can be refused");
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM enrichment_queue WHERE item_id = 'i1' AND due_at > unixepoch()",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        queued, 1,
        "the refused provider gets another look after a cooldown"
    );

    // Re-running the pick does not resurrect the refused record.
    assert_eq!(assigned(&db, "i1").await, None);
}

/// Two answers landing at once must leave exactly one assignment, and the
/// one the rule dictates — no lost update, no SQLITE_BUSY.
#[tokio::test]
async fn concurrent_answers_leave_one_correct_assignment() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    for n in 0..12 {
        let id = format!("i{n}");
        item(&db, &id).await;
        let (a, b) = (db.clone(), db.clone());
        let (i1, i2) = (id.clone(), id.clone());
        let tmdb = async move {
            store_answer(&a, &i1, "tmdb", "111", "auto", answer("tmdb", None, None)).await
        };
        let tvdb = async move {
            store_answer(&b, &i2, "tvdb", "222", "auto", answer("tvdb", None, None)).await
        };
        // Both orders, so neither writer is systematically first.
        if n % 2 == 0 {
            let (x, y) = tokio::join!(tmdb, tvdb);
            x.unwrap();
            y.unwrap();
        } else {
            let (y, x) = tokio::join!(tvdb, tmdb);
            x.unwrap();
            y.unwrap();
        }
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM item_match WHERE item_id = ?")
            .bind(&id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(rows, 1, "{id}: exactly one assignment");
        assert_eq!(
            assigned(&db, &id).await.unwrap().0,
            "tmdb",
            "{id}: the preferred provider"
        );
    }
}

/// The view resolves an item's fields: the assigned provider first, then
/// the preference order fills what it left empty.
#[tokio::test]
async fn the_view_side_fills_from_the_preference_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    // TMDB is assigned but has no synopsis; TVDB has one.
    store_answer(
        &db,
        "i1",
        "tmdb",
        "550",
        "auto",
        answer("Fight Club", None, Some(8.4)),
    )
    .await
    .unwrap();
    store_answer(
        &db,
        "i1",
        "tvdb",
        "77",
        "auto",
        answer("FC", Some("a synopsis"), None),
    )
    .await
    .unwrap();
    let r = sqlx::query(
        "SELECT provider, title, overview, rating, confidence FROM resolved_metadata
          WHERE item_id = 'i1'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(r.get::<String, _>("provider"), "tmdb");
    assert_eq!(
        r.get::<String, _>("title"),
        "Fight Club",
        "the assigned provider's title"
    );
    assert_eq!(
        r.get::<String, _>("overview"),
        "a synopsis",
        "side-filled from TVDB"
    );
    assert_eq!(r.get::<f64, _>("rating"), 8.4);
    assert_eq!(r.get::<String, _>("confidence"), "auto");
}

/// An episode has no assignment: it renders through its show's.
#[tokio::test]
async fn the_view_resolves_an_episode_through_its_show() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "show1").await;
    sqlx::query(
        "INSERT INTO items (id, kind, title, norm_title, parent_id, season, episode)
         VALUES ('ep1', 'episode', 'e', 'e', 'show1', 1, 1)",
    )
    .execute(&db)
    .await
    .unwrap();
    // The show is assigned to TVDB (TMDB missed it).
    store_answer(&db, "show1", "tmdb", "", "miss", Fields::default())
        .await
        .unwrap();
    store_answer(
        &db,
        "show1",
        "tvdb",
        "9",
        "auto",
        answer("The Show", None, None),
    )
    .await
    .unwrap();
    // Both providers answered for the episode itself.
    store_answer(
        &db,
        "ep1",
        "tmdb",
        "1",
        "auto",
        answer("tmdb episode", None, None),
    )
    .await
    .unwrap();
    store_answer(
        &db,
        "ep1",
        "tvdb",
        "2",
        "auto",
        answer("tvdb episode", Some("t"), None),
    )
    .await
    .unwrap();

    let title: String =
        sqlx::query_scalar("SELECT title FROM resolved_metadata WHERE item_id = 'ep1'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        title, "tvdb episode",
        "the episode follows the show's provider, not the rank"
    );
    assert_eq!(
        assigned(&db, "ep1").await,
        None,
        "and still has no assignment of its own"
    );
}

/// The flattening rule from the module doc, as a runnable check: a
/// per-item read of the view must not materialise it. If someone tidies
/// the scalar subqueries into joins this fails, which is the only warning
/// there is — the results stay correct and get 70x slower.
#[tokio::test]
async fn the_view_stays_flattenable() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    let plan: Vec<String> = sqlx::query(
        "EXPLAIN QUERY PLAN
         SELECT i.id, v.title FROM items i
         LEFT JOIN resolved_metadata v ON v.item_id = i.id
          WHERE i.parent_id = 'i1'",
    )
    .fetch_all(&db)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get::<String, _>("detail"))
    .collect();
    let plan = plan.join("\n");
    assert!(
        !plan.to_uppercase().contains("MATERIALIZE"),
        "the view materialised instead of flattening:\n{plan}"
    );
}

/// HUB-9: local is not in any chain and leads anyway. A .nfo wins the
/// fields it states; the search results fill in the rest.
#[tokio::test]
async fn local_metadata_leads_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    assert_eq!(chain_in_force(&db, "movies").await, vec!["tmdb", "tvdb"]);
    assert_eq!(
        chain_in_force(&db, "anime").await,
        vec!["anime", "tmdb", "tvdb"]
    );

    item(&db, "i1").await;
    store_answer(
        &db,
        "i1",
        "tmdb",
        "593",
        "auto",
        answer("Solaris (1972)", Some("tmdb plot"), Some(8.0)),
    )
    .await
    .unwrap();
    store_answer(
        &db,
        "i1",
        "local",
        "nfo",
        "auto",
        Fields {
            title: Some("Solyaris".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // The .nfo owns the item and its title; TMDB still supplies what the
    // human did not write down.
    assert_eq!(assigned(&db, "i1").await.unwrap().0, "local");
    let (provider, title, overview, rating) = merged(&db, "i1").await;
    assert_eq!(provider, "local");
    assert_eq!(title.as_deref(), Some("Solyaris"));
    assert_eq!(overview.as_deref(), Some("tmdb plot"), "side-filled");
    assert_eq!(rating, Some(8.0));

    // It is not orderable: there is no position to move it to, and a
    // chain naming it is refused rather than quietly accepted.
    assert!(
        set_chain(
            &db,
            "movies",
            &["tmdb".into(), "local".into(), "tvdb".into()]
        )
        .await
        .is_err()
    );
    // Reordering what IS in the chain leaves local in front.
    set_chain(&db, "movies", &["tvdb".into(), "tmdb".into()])
        .await
        .unwrap();
    assert_eq!(assigned(&db, "i1").await.unwrap().0, "local");
    assert_eq!(merged(&db, "i1").await.1.as_deref(), Some("Solyaris"));
}

/// The one thing that displaces local: the owner contradicting it. A pin
/// elsewhere means the .nfo is wrong about which work this is, so local
/// steps aside wholesale rather than keeping the fields it stated.
#[tokio::test]
async fn a_pin_elsewhere_displaces_the_nfo() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    store_answer(
        &db,
        "i1",
        "tmdb",
        "593",
        "auto",
        answer("Solaris", Some("tmdb plot"), None),
    )
    .await
    .unwrap();
    store_answer(
        &db,
        "i1",
        "local",
        "nfo",
        "auto",
        Fields {
            title: Some("Solyaris".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(merged(&db, "i1").await.1.as_deref(), Some("Solyaris"));

    // The pick carries the record's own fields, as the API does when a
    // human chooses one out of the search results.
    kahawai_hub::providers::assign_manual(
        &db,
        "i1",
        "tmdb",
        "593",
        answer("Solaris", Some("tmdb plot"), None),
    )
    .await
    .unwrap();
    let (provider, title, _, _) = merged(&db, "i1").await;
    assert_eq!(provider, "tmdb");
    assert_eq!(
        title.as_deref(),
        Some("Solaris"),
        "the pin is not overridden by the .nfo"
    );

    // A cover claims no record, so a pin about identity never displaces
    // it — that is the whole reason it answers with an empty id.
    store_answer(
        &db,
        "i1",
        "local",
        "",
        "auto",
        Fields {
            poster_path: Some("local://cover.jpg".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let p: Option<String> =
        sqlx::query_scalar("SELECT poster_path FROM resolved_metadata WHERE item_id = 'i1'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(p.as_deref(), Some("local://cover.jpg"));
}

/// HUB-9: the cover next to the file beats a provider's own poster
/// without becoming the answer to "what work is this?", which a picture
/// cannot say — and without holding a rank to beat it with.
#[tokio::test]
async fn local_artwork_supplies_the_poster_but_never_the_identity() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;

    store_answer(
        &db,
        "i1",
        "tmdb",
        "550",
        "auto",
        Fields {
            title: Some("Fight Club".into()),
            poster_path: Some("/tmdb.jpg".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // Empty provider_id: a scanned cover, no claim of identity.
    store_answer(
        &db,
        "i1",
        "local",
        "",
        "auto",
        Fields {
            poster_path: Some("local://cover.jpg".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let r = sqlx::query(
        "SELECT provider, title, poster_path FROM resolved_metadata WHERE item_id = 'i1'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        r.get::<Option<String>, _>("poster_path").as_deref(),
        Some("local://cover.jpg")
    );
    assert_eq!(r.get::<String, _>("provider"), "tmdb");
    assert_eq!(
        r.get::<Option<String>, _>("title").as_deref(),
        Some("Fight Club")
    );

    // And the assignment itself is TMDB's, not local's.
    let m: Option<String> =
        sqlx::query_scalar("SELECT provider FROM item_match WHERE item_id = 'i1'")
            .fetch_optional(&db)
            .await
            .unwrap();
    assert_eq!(m.as_deref(), Some("tmdb"));

    // local holds no rank at all — the cover wins on being local.
    let ranked: Vec<String> =
        sqlx::query_scalar("SELECT provider FROM provider_ranks WHERE provider = 'local'")
            .fetch_all(&db)
            .await
            .unwrap();
    assert!(ranked.is_empty(), "local must not be rankable");
}

// ---------- a declined chain is not a miss ----------

/// A provider that declines every item — which is exactly what every
/// real provider does on a restart, when its questions are already on
/// file and no network request is due.
struct DecliningProvider(&'static str);

#[async_trait::async_trait]
impl kahawai_hub::providers::Provider for DecliningProvider {
    fn name(&self) -> &'static str {
        self.0
    }
    async fn enrich(
        &self,
        _db: &SqlitePool,
        _item: &kahawai_hub::providers::ItemRef,
    ) -> anyhow::Result<kahawai_hub::providers::Outcome> {
        Ok(kahawai_hub::providers::Outcome::Declined)
    }
}

fn item_ref(id: &str) -> kahawai_hub::providers::ItemRef {
    kahawai_hub::providers::ItemRef {
        id: id.into(),
        kind: "movie".into(),
        title: id.into(),
        norm_title: id.into(),
        year: None,
        artist: None,
        norm_artist: None,
        alt: None,
        existing: None,
        manual: false,
        known_aid: None,
        identified: false,
        owner: None,
    }
}

/// The restart wipe of 2026-08-01: every question already recorded, so
/// every provider declines, the chain returns None — and the standing
/// answers must come through untouched. The unconditional miss upsert
/// this guards against erased the whole catalogue's matches (and their
/// posters) on every hub restart.
#[tokio::test]
async fn a_fully_declined_chain_never_wipes_a_standing_answer() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    store_answer(
        &db,
        "i1",
        "tmdb",
        "550",
        "auto",
        Fields {
            title: Some("Fight Club".into()),
            poster_path: Some("/p.jpg".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(assigned(&db, "i1").await.unwrap().0, "tmdb");

    let mut set = kahawai_hub::providers::ProviderSet::default();
    set.add(Box::new(DecliningProvider("tmdb")));
    let out = set.run_chain("movies", &db, &item_ref("i1")).await;
    assert!(out.is_none(), "everyone declined");

    let (conf, poster): (String, Option<String>) = sqlx::query_as(
        "SELECT confidence, poster_path FROM provider_metadata
          WHERE item_id = 'i1' AND provider = 'tmdb'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(conf, "auto", "the answer survived the declined chain");
    assert_eq!(poster.as_deref(), Some("/p.jpg"));
    assert_eq!(
        assigned(&db, "i1").await.unwrap().0,
        "tmdb",
        "and the assignment with it"
    );
}

/// The walker still records the miss for an item nobody has anything on
/// — "consulted, nothing found" is presentation the UI needs.
#[tokio::test]
async fn a_declined_chain_records_a_miss_only_where_nothing_stands() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;

    let mut set = kahawai_hub::providers::ProviderSet::default();
    set.add(Box::new(DecliningProvider("tmdb")));
    assert!(set.run_chain("movies", &db, &item_ref("i1")).await.is_none());

    let (pid, conf): (String, String) = sqlx::query_as(
        "SELECT provider_id, confidence FROM provider_metadata
          WHERE item_id = 'i1' AND provider = 'tmdb'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!((pid.as_str(), conf.as_str()), ("", "miss"));
    assert_eq!(assigned(&db, "i1").await, None);
}

/// The selection must ask about exactly the providers the pass bound —
/// no more, no less. json_each(?2) makes that a runtime fact rather
/// than a hard-coded list: a provider outside the bound set never
/// counts as owing work, whatever its name is. TVDB is one instance of
/// this (an unconfigured searcher never asks and never records its
/// question, so counting it as owed would re-select every matched item
/// on every run — which is what kept feeding the restart wipe above),
/// but the rule is general, which this test proves by naming a provider
/// that isn't TVDB at all.
#[tokio::test]
async fn a_provider_outside_the_bound_set_owes_no_work() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    store_answer(
        &db,
        "i1",
        "tmdb",
        "550",
        "auto",
        answer("Fight Club", None, None),
    )
    .await
    .unwrap();

    let due = |providers: &'static str| {
        let db = db.clone();
        async move {
            sqlx::query(kahawai_hub::enrich::GENERIC_SELECTION_SQL)
                .bind(kahawai_hub::providers::QUERY_REV)
                .bind(providers)
                .fetch_all(&db)
                .await
                .unwrap()
                .len()
        }
    };
    assert_eq!(
        due(r#"["tmdb"]"#).await,
        0,
        "the only bound provider is already answered"
    );
    assert_eq!(
        due(r#"["tmdb","tvdb"]"#).await,
        1,
        "tvdb is bound this time and still owes its look"
    );
    assert_eq!(
        due(r#"["ghost"]"#).await,
        1,
        "a provider outside tmdb's answer still owes work once it's bound — \
         nothing about the rule is tvdb-specific"
    );
}

// ---------- the caller path: a restart must not erase a match ----------

/// The regression itself, through the real entry point rather than
/// `run_chain` directly. A fully-matched item can still get re-selected
/// — here because an .nfo appeared that `local` hasn't read yet, which
/// is independent of tmdb's already-answered state — and when it is,
/// every provider it already holds a real answer for gets skipped
/// rather than re-asked (`has_real_answer`). `run_chain` then returns
/// `None`, not because anyone found a miss but because nothing needed
/// doing. `enrich.rs`'s caller used to treat that `None` as "record a
/// miss", overwriting the standing tmdb match on every such restart.
/// This drives `Enricher::run_once` itself, so unlike the two tests
/// above — which pin `run_chain`'s own (already-correct) guard — it
/// fails if the regression creeps back in anywhere between `run_chain`
/// and the database.
#[tokio::test]
async fn a_restart_that_re_selects_a_matched_item_does_not_erase_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = kahawai_hub::registry::Registry::new(db.clone(), Default::default());
    // Never dialled: the item's tmdb answer already stands, so
    // `has_real_answer` skips the provider before it would place a
    // request with this key.
    registry
        .set_setting("tmdb_api_key", "unused-in-this-test")
        .await
        .unwrap();

    item(&db, "i1").await;
    store_answer(
        &db,
        "i1",
        "tmdb",
        "550",
        "auto",
        Fields {
            title: Some("Fight Club".into()),
            poster_path: Some("/p.jpg".into()),
            // Filled so the details-backfill pass (a later, unrelated
            // step of the same run) has nothing left to fetch either.
            original_language: Some("en".into()),
            genres: Some("[\"Drama\"]".into()),
            cast_json: Some("[]".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(assigned(&db, "i1").await.unwrap().0, "tmdb");

    // An .nfo shows up that `local` has not read yet: on its own enough
    // to re-select the item for the generic pass, per the SQL's local
    // OR-branch.
    sqlx::query(
        "INSERT INTO files (module_id, collection_id, path_rel, size, mtime_unix,
                            head_xxh3, tail_xxh3, oshash, streams_json)
         VALUES ('m0', 'c0', 'Fight Club (1999).mkv', 1000, 1, 0, 0, 0,
                 '{\"nfo\":\"Fight Club.nfo\"}')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO item_sources (module_id, collection_id, path_rel, item_id)
         VALUES ('m0', 'c0', 'Fight Club (1999).mkv', 'i1')",
    )
    .execute(&db)
    .await
    .unwrap();

    let rows = sqlx::query(kahawai_hub::enrich::GENERIC_SELECTION_SQL)
        .bind(kahawai_hub::providers::QUERY_REV)
        .bind(r#"["tmdb"]"#)
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the fresh .nfo owes a local answer, so the item is selected \
         despite tmdb already matching"
    );

    let enricher = std::sync::Arc::new(kahawai_hub::enrich::Enricher::new(
        dir.path().to_path_buf(),
    ));
    let registry = std::sync::Arc::new(registry);
    enricher.run_once(&registry).await.unwrap();

    let (pid, conf, poster): (String, String, Option<String>) = sqlx::query_as(
        "SELECT provider_id, confidence, poster_path FROM provider_metadata
          WHERE item_id = 'i1' AND provider = 'tmdb'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        (pid.as_str(), conf.as_str()),
        ("550", "auto"),
        "the match survived a run that re-selected but could not re-answer it"
    );
    assert_eq!(poster.as_deref(), Some("/p.jpg"));
    assert_eq!(
        assigned(&db, "i1").await.unwrap().0,
        "tmdb",
        "and the assignment with it"
    );
}
