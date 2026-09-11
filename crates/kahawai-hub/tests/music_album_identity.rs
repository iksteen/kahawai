//! Metadata-only Album Artist convergence preserves durable human decisions.
use kahawai_hub::registry::{FileUpsertRecord, Registry};

fn track(number: i64, corrected: bool) -> FileUpsertRecord {
    let mut tags = serde_json::json!({"title":format!("Song {number}"),"album":"Compilation",
        "artist":format!("Guest {number}"),"track_number":number.to_string()});
    if corrected {
        tags["album_artist"] = "Various Artists".into();
    }
    FileUpsertRecord {
        root_token: kahawai_core::media::root_token(std::path::Path::new("/music")),
        path_rel: format!("loose-{number}.flac"),
        size: 100 + number as u64,
        mtime_unix: 1,
        head_xxh3: number as u64,
        tail_xxh3: number as u64 + 1,
        oshash: 0,
        streams_json: serde_json::json!({"tags":tags}).to_string(),
    }
}

async fn fixture() -> (
    kahawai_hub::library::Database,
    Registry,
    Vec<String>,
    Vec<String>,
) {
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    let registry = Registry::new(db.clone(), Default::default());
    registry
        .record_satellite("host", "mediahost", "host", "fp")
        .await
        .unwrap();
    registry
        .announce_collection("host", "music", "music", &["/music".into()])
        .await
        .unwrap();
    registry
        .upsert_files("host", "music", vec![track(1, false), track(2, false)])
        .await
        .unwrap();
    let copies: Vec<String> = sqlx::query_scalar(
        "SELECT parent_id FROM collection_items WHERE kind='track' ORDER BY episode",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    let mut targets = Vec::new();
    let mut tx = db.begin().await.unwrap();
    for title in ["Chosen first", "Chosen second", "Rejected album"] {
        targets.push(
            kahawai_hub::library::create(
                &mut tx,
                kahawai_hub::library::NewItem {
                    kind: "album".into(),
                    title: title.into(),
                    year: Some(2000),
                    artist: Some("Various Artists".into()),
                    parent_id: None,
                    season: None,
                    episode: None,
                    edition: None,
                },
            )
            .await
            .unwrap(),
        );
    }
    tx.commit().await.unwrap();
    (db, registry, copies, targets)
}

async fn assign(db: &kahawai_hub::library::Database, copy: &str, target: &str) {
    let mut tx = db.begin().await.unwrap();
    kahawai_hub::library::assign(&mut tx, copy, &[target.to_owned()])
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn refresh(registry: &Registry) {
    registry
        .upsert_files("host", "music", vec![track(1, true), track(2, true)])
        .await
        .unwrap();
}

#[tokio::test]
async fn regrouping_transfers_explicit_assignment_rejections_and_pinned_metadata() {
    let (db, registry, copies, targets) = fixture().await;
    assign(&db, &copies[1], &targets[1]).await;
    sqlx::query("INSERT INTO rejected_library_matches VALUES(?,?)")
        .bind(&copies[1])
        .bind(&targets[2])
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO provider_metadata(item_id,provider,provider_id,title,poster_path,provider_artist_id,confidence,updated_at)
        VALUES(?,'musicbrainz','chosen-release','Chosen second','chosen.jpg','chosen-artist','manual',1)").bind(&copies[1]).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO manual_match(item_id,provider,provider_id,pinned_at) VALUES(?,'musicbrainz','chosen-release',1)")
        .bind(&copies[1]).execute(&db).await.unwrap();
    refresh(&registry).await;
    let assignment: (String, bool, Option<String>) = sqlx::query_as("SELECT a.library_item_id,c.assignment_manual,c.match_conflict FROM collection_items c JOIN collection_item_library_items a ON a.collection_item_id=c.id WHERE c.kind='album'")
        .fetch_one(&db).await.unwrap();
    assert_eq!(assignment, (targets[1].clone(), true, None));
    let rejected: Vec<String> = sqlx::query_scalar(
        "SELECT library_item_id FROM rejected_library_matches WHERE collection_item_id=?",
    )
    .bind(&copies[0])
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(rejected, vec![targets[2].clone()]);
    let answer: (String, String, String) = sqlx::query_as("SELECT p.provider_id,p.poster_path,p.provider_artist_id FROM provider_metadata p JOIN manual_match m ON m.item_id=p.item_id AND m.provider=p.provider WHERE p.item_id=?")
        .bind(&copies[0]).fetch_one(&db).await.unwrap();
    assert_eq!(
        answer,
        (
            "chosen-release".into(),
            "chosen.jpg".into(),
            "chosen-artist".into()
        )
    );
    refresh(&registry).await;
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id=?"
        )
        .bind(&copies[0])
        .fetch_one(&db)
        .await
        .unwrap(),
        targets[1]
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM files")
            .fetch_one(&db)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn regrouping_transfers_a_library_choice_without_a_provider_pin() {
    let (db, registry, copies, targets) = fixture().await;
    assign(&db, &copies[1], &targets[1]).await;
    sqlx::query("INSERT INTO rejected_library_matches VALUES(?,?)")
        .bind(&copies[1])
        .bind(&targets[2])
        .execute(&db)
        .await
        .unwrap();
    refresh(&registry).await;
    let assignment: (String, bool) = sqlx::query_as("SELECT a.library_item_id,c.assignment_manual FROM collection_items c JOIN collection_item_library_items a ON a.collection_item_id=c.id WHERE c.kind='album'")
        .fetch_one(&db).await.unwrap();
    assert_eq!(assignment, (targets[1].clone(), true));
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM rejected_library_matches WHERE collection_item_id=? AND library_item_id=?")
        .bind(&copies[0]).bind(&targets[2]).fetch_one(&db).await.unwrap(), 1);
}

#[tokio::test]
async fn regrouping_retains_conflicting_explicit_choices_until_a_human_resolves_them() {
    let (db, registry, copies, targets) = fixture().await;
    for n in 0..2 {
        assign(&db, &copies[n], &targets[n]).await;
        sqlx::query("INSERT INTO rejected_library_matches VALUES(?,?)")
            .bind(&copies[n])
            .bind(&targets[2])
            .execute(&db)
            .await
            .unwrap();
    }
    for _ in 0..2 {
        refresh(&registry).await;
        let assignments: Vec<(String, String, bool, Option<String>)> = sqlx::query_as("SELECT c.id,a.library_item_id,c.assignment_manual,c.match_conflict FROM collection_items c JOIN collection_item_library_items a ON a.collection_item_id=c.id WHERE c.kind='album' ORDER BY c.id")
            .fetch_all(&db).await.unwrap();
        assert_eq!(
            assignments.len(),
            2,
            "conflicting human choices must keep both physical albums"
        );
        for (copy, target, manual, conflict) in assignments {
            let n = copies.iter().position(|id| id == &copy).unwrap();
            assert_eq!(target, targets[n]);
            assert!(manual);
            assert!(
                conflict.is_some(),
                "conflict must be visible after unchanged refreshes"
            );
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM rejected_library_matches WHERE library_item_id=?"
            )
            .bind(&targets[2])
            .fetch_one(&db)
            .await
            .unwrap(),
            2
        );
    }
    assign(&db, &copies[1], &targets[0]).await;
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM collection_items WHERE kind='album' AND match_conflict IS NOT NULL")
        .fetch_one(&db).await.unwrap(), 0, "normal assignment resolves both peers' conflict");
    refresh(&registry).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM collection_items WHERE kind='album'")
            .fetch_one(&db)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn regrouping_retains_a_rejection_that_conflicts_with_the_other_albums_choice() {
    let (db, registry, copies, targets) = fixture().await;
    assign(&db, &copies[0], &targets[0]).await;
    sqlx::query("INSERT INTO rejected_library_matches VALUES(?,?)")
        .bind(&copies[1])
        .bind(&targets[0])
        .execute(&db)
        .await
        .unwrap();
    for _ in 0..2 {
        refresh(&registry).await;
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM collection_items WHERE kind='album'"
            )
            .fetch_one(&db)
            .await
            .unwrap(),
            2
        );
        assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM collection_items WHERE kind='album' AND match_conflict IS NOT NULL").fetch_one(&db).await.unwrap(), 2);
        assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM rejected_library_matches WHERE collection_item_id=? AND library_item_id=?")
            .bind(&copies[1]).bind(&targets[0]).fetch_one(&db).await.unwrap(), 1);
    }
    assign(&db, &copies[1], &targets[0]).await;
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM collection_items WHERE kind='album' AND match_conflict IS NOT NULL").fetch_one(&db).await.unwrap(), 0);
    refresh(&registry).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM collection_items WHERE kind='album'")
            .fetch_one(&db)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn changed_album_credit_clears_the_former_peers_conflict() {
    let (db, registry, copies, targets) = fixture().await;
    for n in 0..2 {
        assign(&db, &copies[n], &targets[n]).await;
    }
    refresh(&registry).await;
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM collection_items WHERE kind='album' AND match_conflict IS NOT NULL").fetch_one(&db).await.unwrap(), 2);
    let mut changed = track(2, true);
    let mut info: serde_json::Value = serde_json::from_str(&changed.streams_json).unwrap();
    info["tags"]["album_artist"] = "Different Artist".into();
    changed.streams_json = info.to_string();
    registry
        .upsert_files("host", "music", vec![changed])
        .await
        .unwrap();
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM collection_items WHERE kind='album' AND match_conflict IS NOT NULL").fetch_one(&db).await.unwrap(), 0,
        "metadata that separates the albums must clear both former peers");
}
