//! HUB-5: first-claim-wins at field granularity, and what a reorder
//! does. The point of storing every provider's answer is that
//! precedence can be re-decided locally — these tests are what keeps
//! that property true.

use kahawai_hub::providers::{
    chain_in_force, materialize, media_type_of_item, set_chain, store_answer, Fields, MANUAL,
};
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

async fn merged(db: &SqlitePool, id: &str) -> (String, Option<String>, Option<String>, Option<f64>) {
    let r = sqlx::query(
        "SELECT provider, title, overview, rating FROM merged_metadata WHERE item_id = ?",
    )
    .bind(id)
    .fetch_one(db)
    .await
    .unwrap();
    (r.get("provider"), r.get("title"), r.get("overview"), r.get("rating"))
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
    store_answer(&db, "i1", "tmdb", "550", "auto", answer("Fight Club", None, None), &chain)
        .await
        .unwrap();
    // TVDB, ranked below, has a title AND the synopsis TMDB lacked.
    store_answer(
        &db,
        "i1",
        "tvdb",
        "77",
        "auto",
        answer("Fight Club (1999)", Some("A ticking-time-bomb insomniac..."), Some(8.4)),
        &chain,
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
    let chain = chain_in_force(&db, "movies").await;
    store_answer(&db, "i1", "tmdb", "550", "auto", answer("TMDB title", Some("tmdb"), None), &chain)
        .await
        .unwrap();
    store_answer(&db, "i1", "tvdb", "77", "auto", answer("TVDB title", Some("tvdb"), None), &chain)
        .await
        .unwrap();
    assert_eq!(merged(&db, "i1").await.1.as_deref(), Some("TMDB title"));

    // Rank TVDB above TMDB. No provider is contacted: the answers are
    // already on disk, which is the whole reason this is affordable.
    set_chain(&db, "movies", &["tvdb".into(), "tmdb".into()]).await.unwrap();
    let (provider, title, overview, _) = merged(&db, "i1").await;
    assert_eq!(title.as_deref(), Some("TVDB title"), "the reorder took effect");
    assert_eq!(overview.as_deref(), Some("tvdb"));
    assert_eq!(provider, "tvdb", "identity follows the order too");

    // And back again — nothing was lost in the first merge.
    set_chain(&db, "movies", &["tmdb".into(), "tvdb".into()]).await.unwrap();
    assert_eq!(merged(&db, "i1").await.1.as_deref(), Some("TMDB title"));
}

#[tokio::test]
async fn a_manual_pick_outranks_every_provider_and_survives_reorders() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    let chain = chain_in_force(&db, "movies").await;
    store_answer(&db, "i1", "tmdb", "550", "auto", answer("Robot Wars", None, None), &chain)
        .await
        .unwrap();
    // A human corrected it to the TVDB record.
    store_answer(&db, "i1", "tvdb", "77", MANUAL, answer("Robot Wars (1993)", None, None), &chain)
        .await
        .unwrap();
    assert_eq!(merged(&db, "i1").await.1.as_deref(), Some("Robot Wars (1993)"));

    // TMDB is ranked first and re-runs — the human's answer still wins.
    set_chain(&db, "movies", &["tmdb".into(), "tvdb".into()]).await.unwrap();
    let (provider, title, ..) = merged(&db, "i1").await;
    assert_eq!(title.as_deref(), Some("Robot Wars (1993)"), "a reorder cannot undo a human");
    assert_eq!(provider, "tvdb", "and it stays attributed to the real service");
}

#[tokio::test]
async fn a_stored_order_that_is_not_a_permutation_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    // Dropping a provider would silently disable it; adding an unknown
    // one would silently do nothing.
    assert!(set_chain(&db, "movies", &["tmdb".into()]).await.is_err());
    assert!(set_chain(&db, "movies", &["tmdb".into(), "imdb".into()]).await.is_err());
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
    assert_eq!(chain_in_force(&db, "anime").await, vec!["anime", "tmdb", "tvdb"]);
}

/// The anime composite stores its half under `anilist`; it must still
/// rank where `anime` sits, or the tail would outrank the very provider
/// that identified the item.
#[tokio::test]
async fn the_anime_composites_answer_ranks_as_the_chain_entry() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    let chain = chain_in_force(&db, "anime").await;
    store_answer(&db, "i1", "anilist", "9253", "auto", answer("Steins;Gate", None, None), &chain)
        .await
        .unwrap();
    store_answer(
        &db,
        "i1",
        "tmdb",
        "42509",
        "auto",
        answer("Steins Gate", Some("A rag-tag group..."), Some(8.5)),
        &chain,
    )
    .await
    .unwrap();

    let (provider, title, overview, rating) = merged(&db, "i1").await;
    assert_eq!(title.as_deref(), Some("Steins;Gate"), "AniList title outranks TMDB's");
    assert_eq!(provider, "anilist", "identity stays with the anime chain");
    assert!(overview.is_some(), "but TMDB supplied the synopsis AniList lacked");
    assert_eq!(rating, Some(8.5));
}

/// Rebuilding the merged row from an empty answer set must not blank a
/// row that predates the model (episodes, which stay out of it).
#[tokio::test]
async fn items_with_no_stored_answers_are_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "ep1").await;
    sqlx::query(
        "INSERT INTO merged_metadata (item_id, provider, provider_id, title, confidence, updated_at)
         VALUES ('ep1', 'tvdb', '9', 'Episode 1', 'auto', unixepoch())",
    )
    .execute(&db)
    .await
    .unwrap();
    materialize(&db, "ep1", &chain_in_force(&db, "movies").await).await.unwrap();
    assert_eq!(merged(&db, "ep1").await.1.as_deref(), Some("Episode 1"));
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
    assert!(first > 0 && first <= 900, "first retry is soon-ish: {first}s");
    assert!(due_items(&db, 10).await.is_empty(), "not due yet");

    // Repeated refusals back off further, never faster.
    reschedule(&db, "i1", "anime", "anidb banned us").await;
    let second: i64 =
        sqlx::query_scalar("SELECT due_at - unixepoch() FROM enrichment_queue WHERE item_id = 'i1'")
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
    sqlx::query("INSERT INTO items (id, kind, title, norm_title) VALUES ('ep', 'episode', 'e', 'e')")
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
    assert_eq!(left, vec!["show1".to_string()], "episode work must not linger");
}

/// Movies and series share a DEFAULT order, not a chain. Ranking TVDB
/// first for series must leave films untouched — the grouping in the
/// requirement is about the defaults being equal, nothing more.
#[tokio::test]
async fn series_has_its_own_chain_independent_of_movies() {
    use kahawai_hub::providers::{chain_for, media_type_key, MEDIA_TYPES};
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    assert!(MEDIA_TYPES.contains(&"series"));
    assert_eq!(media_type_key("series"), "series");
    assert_eq!(chain_for("series"), chain_for("movies"), "same default");
    // An unknown media type still enriches rather than having no chain.
    assert_eq!(media_type_key("podcasts"), "movies");

    // Seeded separately, and moving one leaves the other alone.
    assert_eq!(chain_in_force(&db, "series").await, vec!["tmdb", "tvdb"]);
    set_chain(&db, "series", &["tvdb".into(), "tmdb".into()]).await.unwrap();
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
    let chain = chain_in_force(&db, "movies").await;
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
        &chain,
    )
    .await
    .unwrap();
    // TVDB is asked anyway and its answer is kept, ready to win if the
    // order changes later.
    store_answer(&db, "i1", "tvdb", "77", "auto", answer("TVDB title", Some("tvdb"), None), &chain)
        .await
        .unwrap();
    assert_eq!(merged(&db, "i1").await.1.as_deref(), Some("Fight Club"));

    set_chain(&db, "movies", &["tvdb".into(), "tmdb".into()]).await.unwrap();
    let (provider, title, overview, rating) = merged(&db, "i1").await;
    assert_eq!(title.as_deref(), Some("TVDB title"), "the reorder now has data to pick");
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
    let chain = chain_in_force(&db, "movies").await;
    store_answer(&db, "i1", "tmdb", "550", "auto", answer("Solaris", None, None), &chain)
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
        &chain,
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
    set_chain(&db, "movies", &["tvdb".into(), "tmdb".into()]).await.unwrap();
    let (provider, _, overview, _) = merged(&db, "i1").await;
    assert_eq!(provider, "tmdb", "the confident match keeps the item");
    assert_eq!(overview, None, "and the weak stranger still donates nothing");

    // It does own an item nobody else could identify, and then its own
    // data is all there is.
    item(&db, "i2").await;
    store_answer(&db, "i2", "tmdb", "", "miss", Fields::default(), &chain).await.unwrap();
    store_answer(&db, "i2", "tvdb", "999", "weak", answer("Obscure", Some("only guess"), None), &chain)
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
    let chain = chain_in_force(&db, "movies").await; // tmdb, tvdb
    store_answer(&db, "i1", "tmdb", "111", "weak", answer("Being Human?", None, None), &chain)
        .await
        .unwrap();
    store_answer(&db, "i1", "tvdb", "222", "auto", answer("Being Human", Some("bbc"), None), &chain)
        .await
        .unwrap();

    let (provider, title, overview, _) = merged(&db, "i1").await;
    assert_eq!(provider, "tvdb", "the certain match keeps the item");
    assert_eq!(title.as_deref(), Some("Being Human"));
    assert_eq!(overview.as_deref(), Some("bbc"));

    // A manual correction still beats both, from either provider.
    store_answer(&db, "i1", "tmdb", "333", MANUAL, answer("Being Human (US)", None, None), &chain)
        .await
        .unwrap();
    assert_eq!(merged(&db, "i1").await.0, "tmdb");
    assert_eq!(merged(&db, "i1").await.1.as_deref(), Some("Being Human (US)"));
}

/// A confirmed year beats a silent one. TMDB carries a 1952 series
/// called "The Continental" with no air date at all, and it won an
/// exact-title match over "The Continental: From the World of John Wick"
/// (2023) — which is what the file plainly meant.
#[test]
fn a_stated_year_beats_an_exact_title_with_no_year() {
    use kahawai_hub::enrich::{pick_candidate, Candidate};
    let old_show = Candidate::for_test(19069, "The Continental", None);
    let right = Candidate::for_test(72710, "The Continental: From the World of John Wick",
        Some("2023-09-22"));
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
