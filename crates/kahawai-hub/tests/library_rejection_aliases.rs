//! Permanent first-identification aliases preserve the meaning of refusals.
use kahawai_hub::{
    db,
    library::{self, Database},
    providers::{self, Fields},
};

async fn fixture() -> Database {
    let db = db::open_in_memory().await.unwrap();
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','movies','movies');
        INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES
          ('origin','movie','Unknown origin','unknown origin',NULL,'host','movies'),
          ('bridge','movie','Unknown bridge','unknown bridge',NULL,'host','movies'),
          ('refuser','movie','Unknown refusal','unknown refusal',NULL,'host','movies'),
          ('known','movie','Known','known',2000,'host','movies'),
          ('other','movie','Other','other',2001,'host','movies');")
        .execute(&db).await.unwrap();
    db
}

async fn choose(db: &Database, copy: &str, target: &str) {
    let mut tx = db.begin().await.unwrap();
    library::assign(&mut tx, copy, &[target.to_owned()])
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn reject(db: &Database, target: &str) {
    sqlx::query("INSERT INTO rejected_library_matches VALUES('refuser',?)")
        .bind(target)
        .execute(db)
        .await
        .unwrap();
}

async fn enrich(db: &Database) {
    providers::store_answer(
        db,
        "refuser",
        "tmdb",
        "known-record",
        "auto",
        Fields {
            title: Some("Known".into()),
            premiered: Some("2000-01-01".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

async fn assigned(db: &Database) -> String {
    sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='refuser'")
        .fetch_one(db).await.unwrap()
}

async fn rejections(db: &Database) -> Vec<String> {
    sqlx::query_scalar("SELECT library_item_id FROM rejected_library_matches WHERE collection_item_id='refuser' ORDER BY library_item_id")
        .fetch_all(db).await.unwrap()
}

#[tokio::test]
async fn enrichment_respects_a_rejection_through_multiple_permanent_aliases() {
    let db = fixture().await;
    reject(&db, "origin").await;
    reject(&db, "other").await;
    choose(&db, "origin", "bridge").await;
    choose(&db, "bridge", "known").await;
    assert_eq!(library::resolve_id(&db, "origin").await.unwrap(), "known");
    enrich(&db).await;
    assert_ne!(
        assigned(&db).await,
        "known",
        "identifying a rejected work cannot make it acceptable again"
    );
    assert_eq!(
        rejections(&db).await,
        vec!["origin", "other"],
        "retain the original refusal rows"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT match_mode FROM collection_items WHERE id='refuser'"
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        "unmatched"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT provider_id FROM provider_metadata WHERE item_id='refuser' AND provider='tmdb'"
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        "known-record",
        "refusal must not destroy enrichment evidence"
    );
}

#[tokio::test]
async fn explicit_correction_clears_only_rejections_of_the_same_canonical_work() {
    let db = fixture().await;
    reject(&db, "origin").await;
    reject(&db, "bridge").await;
    reject(&db, "other").await;
    choose(&db, "origin", "bridge").await;
    choose(&db, "bridge", "known").await;
    choose(&db, "refuser", "known").await;
    assert_eq!(assigned(&db).await, "known");
    assert_eq!(
        rejections(&db).await,
        vec!["other"],
        "a new choice supersedes equivalent refusals, not unrelated decisions"
    );
    enrich(&db).await;
    assert_eq!(assigned(&db).await, "known");
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT assignment_manual FROM collection_items WHERE id='refuser'"
        )
        .fetch_one(&db)
        .await
        .unwrap()
    );
}

#[tokio::test]
async fn promotion_invalidates_an_existing_automatic_match_without_waiting_for_enrichment() {
    let db = fixture().await;
    enrich(&db).await;
    assert_eq!(assigned(&db).await, "known");
    reject(&db, "origin").await;
    choose(&db, "origin", "bridge").await;
    assert_eq!(assigned(&db).await, "known");
    choose(&db, "bridge", "known").await;
    assert_ne!(
        assigned(&db).await,
        "known",
        "promotion changes the refusal's meaning in the same transaction"
    );
    assert_eq!(rejections(&db).await, vec!["origin"]);
}

#[tokio::test]
async fn migration_revisits_existing_alias_refusals() {
    let db = fixture().await;
    reject(&db, "origin").await;
    choose(&db, "origin", "bridge").await;
    choose(&db, "bridge", "known").await;
    enrich(&db).await;
    // Reproduce the stored assignment left behind by version 85.
    sqlx::query("UPDATE collection_item_library_items SET library_item_id='known' WHERE collection_item_id='refuser'")
        .execute(&db).await.unwrap();
    assert_eq!(assigned(&db).await, "known");
    let mut tx = db.begin().await.unwrap();
    sqlx::raw_sql("DROP INDEX library_alias_target; DROP INDEX library_rejection_target;")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../migrations/0084_library_rejection_aliases.sql"
    ))
    .execute(&mut *tx)
    .await
    .unwrap();
    let queued: Vec<String> = sqlx::query_scalar(
        "SELECT collection_item_id FROM library_pending ORDER BY collection_item_id",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(queued, vec!["refuser"]);
    tx.commit().await.unwrap();
    assert_ne!(assigned(&db).await, "known");
    assert_eq!(rejections(&db).await, vec!["origin"]);
}
