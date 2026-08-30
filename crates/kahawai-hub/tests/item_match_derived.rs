//! `item_match` — which provider record an item IS — is derived state,
//! and it is maintained by the database rather than by remembering to
//! call something.
//!
//! It used to be maintained by explicit `assign()` calls, which worked
//! until a path forgot. That is the same failure that killed
//! `merged_metadata`, so the guard is the same as `sort_title.rs`: an
//! independent re-derivation of the truth, asserted after every kind of
//! write — including RAW SQL, which is exactly how the old merge drifted.
//!
//! Nothing here calls the pick. If a single assertion needs one, the
//! design has a hole in it.

use kahawai_sqlite::Database as SqlitePool;

/// The assignment, re-derived from scratch: the winning candidate per
/// item under the same rules, compared against what is stored.
///
/// Written out longhand on purpose. Sharing the production SQL would make
/// this test agree with the pick by construction, including where the
/// pick is wrong.
const TRUTH: &str = "\
WITH truth AS (
  SELECT item_id, provider, provider_id, pinned FROM (
    SELECT i.id AS item_id, pm.provider, pm.provider_id,
           EXISTS (SELECT 1 FROM manual_match mm
                    WHERE mm.item_id = pm.item_id AND mm.provider = pm.provider
                      AND mm.provider_id = pm.provider_id) AS pinned,
           ROW_NUMBER() OVER (PARTITION BY i.id ORDER BY
               NOT EXISTS (SELECT 1 FROM manual_match mm
                            WHERE mm.item_id = pm.item_id AND mm.provider = pm.provider
                              AND mm.provider_id = pm.provider_id),
               CASE pm.confidence WHEN 'auto' THEN 0 WHEN 'weak' THEN 1 ELSE 2 END,
               pm.provider <> 'local',
               COALESCE((SELECT r.rank FROM provider_ranks r
                          WHERE r.provider = CASE pm.provider
                                                 WHEN 'anilist' THEN 'anime'
                                                 ELSE pm.provider END
                            AND r.media_type=COALESCE(
                                  (SELECT CASE WHEN c.media_type IN
                                                    ('movies','series','anime','music')
                                               THEN c.media_type ELSE 'movies' END
                                     FROM collections c
                                    WHERE (c.module_id,c.collection_id)=
                                          (i.module_id,i.collection_id)),
                                  'movies')),99),
               pm.provider) AS n
      FROM items i JOIN provider_metadata pm ON pm.item_id = i.id
     WHERE i.kind IN ('movie', 'show', 'album')
       AND pm.confidence IN ('auto', 'weak') AND pm.provider_id <> ''
       AND NOT EXISTS (SELECT 1 FROM rejected_matches rj
                        WHERE rj.item_id = pm.item_id AND rj.provider = pm.provider
                          AND rj.provider_id = pm.provider_id)
  ) WHERE n = 1
),
stored AS (SELECT item_id, provider, provider_id, manual FROM item_match)
-- Both directions: a row the pick should have produced and did not is
-- drift just as much as one it produced and should not have.
SELECT (SELECT COUNT(*) FROM (SELECT * FROM stored EXCEPT SELECT * FROM truth))
     + (SELECT COUNT(*) FROM (SELECT * FROM truth EXCEPT SELECT * FROM stored))";

async fn drifted(db: &SqlitePool) -> i64 {
    sqlx::query_scalar(TRUTH).fetch_one(db).await.unwrap()
}

async fn assigned(db: &SqlitePool, id: &str) -> Option<(String, String, bool)> {
    sqlx::query_as::<_, (String, String, i64)>(
        "SELECT provider, provider_id, manual FROM item_match WHERE item_id = ?",
    )
    .bind(id)
    .fetch_optional(db)
    .await
    .unwrap()
    .map(|(p, i, m)| (p, i, m != 0))
}

async fn item(db: &SqlitePool, id: &str) {
    sqlx::query(
        "INSERT OR IGNORE INTO satellites(module_id,module_type,name,cert_fingerprint)
                 VALUES('fixture','mediahost','fixture','fp')",
    )
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT OR IGNORE INTO collections(module_id,collection_id,media_type)
                 VALUES('fixture','default','movies')",
    )
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,module_id,collection_id)
                 VALUES(?,'movie',?,?,'fixture','default')",
    )
    .bind(id)
    .bind(id)
    .bind(id)
    .execute(db)
    .await
    .unwrap();
}

/// Raw SQL only. Not one call into the provider API, so every assignment
/// below is the database's own work.
async fn answer(db: &SqlitePool, id: &str, provider: &str, pid: &str, confidence: &str) {
    sqlx::query(
        "INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence, updated_at)
         VALUES (?, ?, ?, ?, ?, unixepoch())
         ON CONFLICT (item_id, provider) DO UPDATE SET
           provider_id = excluded.provider_id, confidence = excluded.confidence",
    )
    .bind(id)
    .bind(provider)
    .bind(pid)
    .bind(format!("{provider} title"))
    .bind(confidence)
    .execute(db)
    .await
    .unwrap();
}

#[tokio::test]
async fn the_assignment_follows_every_input_with_nothing_called() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();

    // An item nothing has answered for has no assignment. Absence is the
    // representation — there is no "unmatched" row.
    item(&db, "i1").await;
    assert_eq!(assigned(&db, "i1").await, None);
    assert_eq!(drifted(&db).await, 0);

    // A miss is an answer, and still not a match.
    answer(&db, "i1", "tvdb", "", "miss").await;
    assert_eq!(
        assigned(&db, "i1").await,
        None,
        "a recorded miss is not an assignment"
    );
    assert_eq!(drifted(&db).await, 0);

    // The first real answer takes it.
    answer(&db, "i1", "tvdb", "414734", "auto").await;
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tvdb".into(), "414734".into(), false))
    );
    assert_eq!(drifted(&db).await, 0);

    // TMDB ranks ahead of TVDB for movies, so its answer takes over.
    answer(&db, "i1", "tmdb", "63", "auto").await;
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tmdb".into(), "63".into(), false))
    );
    assert_eq!(drifted(&db).await, 0);

    // Reordering the chain moves the assignment, with no provider asked.
    sqlx::query("UPDATE provider_ranks SET rank = 1 WHERE media_type='movies' AND provider='tmdb'")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE provider_ranks SET rank = 0 WHERE media_type='movies' AND provider='tvdb'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tvdb".into(), "414734".into(), false))
    );
    assert_eq!(drifted(&db).await, 0);

    // Refusing the winner hands it to the runner-up.
    sqlx::query(
        "INSERT INTO rejected_matches (item_id, provider, provider_id, rejected_at)
         VALUES ('i1','tvdb','414734', unixepoch())",
    )
    .execute(&db)
    .await
    .unwrap();
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tmdb".into(), "63".into(), false))
    );
    assert_eq!(drifted(&db).await, 0);

    // A pin outranks the chain, the confidence order and local alike.
    sqlx::query(
        "INSERT INTO manual_match (item_id, provider, provider_id, pinned_at)
         VALUES ('i1','tvdb','414734', unixepoch())",
    )
    .execute(&db)
    .await
    .unwrap();
    // ...except this one is still refused, and a refused record is not a
    // candidate at all — so the pin has nothing to win with.
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tmdb".into(), "63".into(), false))
    );
    assert_eq!(drifted(&db).await, 0);

    sqlx::query("DELETE FROM rejected_matches WHERE item_id = 'i1'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tvdb".into(), "414734".into(), true)),
        "the pin takes effect the moment its record is a candidate again"
    );
    assert_eq!(drifted(&db).await, 0);

    // Local is unranked and beats every provider (HUB-9) — but not the
    // owner explicitly naming a different record.
    answer(&db, "i1", "local", "nfo-1", "auto").await;
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tvdb".into(), "414734".into(), true)),
        "a pin says the .nfo is wrong about this file"
    );
    assert_eq!(drifted(&db).await, 0);

    // Withdrawing the pin lets local through.
    sqlx::query("DELETE FROM manual_match WHERE item_id = 'i1'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("local".into(), "nfo-1".into(), false))
    );
    assert_eq!(drifted(&db).await, 0);

    // Downgrading the winning answer to a miss retires it.
    sqlx::query(
        "UPDATE provider_metadata SET provider_id = '', confidence = 'miss'
                  WHERE item_id = 'i1' AND provider = 'local'",
    )
    .execute(&db)
    .await
    .unwrap();
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tvdb".into(), "414734".into(), false))
    );
    assert_eq!(drifted(&db).await, 0);

    // Deleting every answer leaves no assignment at all.
    sqlx::query("DELETE FROM provider_metadata WHERE item_id = 'i1'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(assigned(&db, "i1").await, None);
    assert_eq!(drifted(&db).await, 0);

    // Finally: prove the detector can fail. A truth query that is
    // silently always 0 would make every assertion above worthless.
    sqlx::query(
        "INSERT INTO item_match (item_id, provider, provider_id, media_type, manual, updated_at)
         VALUES ('i1','tmdb','invented','movies',0, unixepoch())",
    )
    .execute(&db)
    .await
    .unwrap();
    assert_eq!(
        drifted(&db).await,
        1,
        "an assignment no answer backs must read as drift"
    );
}

/// A pin whose record is withdrawn cannot keep an assignment alive — the
/// table would be claiming a match no provider still offers. The pin
/// itself survives, so the answer returning restores it.
#[tokio::test]
async fn a_pin_whose_answer_disappears_does_not_strand_an_assignment() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    item(&db, "i1").await;
    answer(&db, "i1", "tvdb", "414734", "auto").await;
    sqlx::query(
        "INSERT INTO manual_match (item_id, provider, provider_id, pinned_at)
         VALUES ('i1','tvdb','414734', unixepoch())",
    )
    .execute(&db)
    .await
    .unwrap();
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tvdb".into(), "414734".into(), true))
    );

    sqlx::query("DELETE FROM provider_metadata WHERE item_id = 'i1' AND provider = 'tvdb'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(assigned(&db, "i1").await, None);
    assert_eq!(drifted(&db).await, 0);

    answer(&db, "i1", "tvdb", "414734", "auto").await;
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tvdb".into(), "414734".into(), true)),
        "the pin is stateless intent; every pick re-applies it"
    );
    assert_eq!(drifted(&db).await, 0);
}

/// The media type an item enriches as comes from the collection its files
/// live in, and it decides which chain ranks the candidates. Moving a
/// source between collections therefore moves the answer — nothing kept
/// that up to date before.
#[tokio::test]
async fn moving_a_source_between_collections_re_ranks_the_item() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    for (id, mt) in [("c-movies", "movies"), ("c-anime", "anime")] {
        sqlx::query(
            "INSERT INTO satellites (module_id, module_type, name, cert_fingerprint,
                                     enrolled_at, disabled)
             VALUES (?, 'mediahost', ?, '', unixepoch(), 0)",
        )
        .bind(id)
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO collections (module_id, collection_id, media_type, roots_json, sync_version)
             VALUES (?, 'c', ?, '[\"/m\"]', 1)",
        )
        .bind(id)
        .bind(mt)
        .execute(&db)
        .await
        .unwrap();
    }
    item(&db, "i1").await;
    sqlx::query("UPDATE items SET module_id='c-movies',collection_id='c' WHERE id='i1'")
        .execute(&db)
        .await
        .unwrap();
    // Both answer; the anime chain ranks `anime` first, movies has no
    // entry for it at all, so the media type alone decides the winner.
    answer(&db, "i1", "tmdb", "63", "auto").await;
    answer(&db, "i1", "anime", "17", "auto").await;
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tmdb".into(), "63".into(), false))
    );
    assert_eq!(drifted(&db).await, 0);

    sqlx::query("UPDATE items SET module_id='c-anime' WHERE id='i1'")
        .execute(&db)
        .await
        .unwrap();
    let m = assigned(&db, "i1").await.unwrap();
    assert_eq!(
        m.0, "anime",
        "the item is anime now, and the anime chain ranks anime first"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT media_type FROM item_match WHERE item_id = 'i1'")
            .fetch_one(&db)
            .await
            .unwrap(),
        "anime"
    );
    assert_eq!(drifted(&db).await, 0);

    // And the collection itself being re-announced as something else.
    sqlx::query("UPDATE collections SET media_type = 'movies' WHERE module_id = 'c-anime'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        assigned(&db, "i1").await,
        Some(("tmdb".into(), "63".into(), false))
    );
    assert_eq!(drifted(&db).await, 0);
}
