//! Album slot collisions preserve human recording choices on unchanged files.
use kahawai_hub::registry::{FileUpsertRecord, Registry};

fn file(number: u64, corrected: bool) -> FileUpsertRecord {
    let mut tags = serde_json::json!({"title":format!("Recording {number}"),"album":"Compilation",
        "artist":format!("Guest {number}"),"track_number":"1","disc_number":"1"});
    if corrected {
        tags["album_artist"] = "Various Artists".into();
    }
    FileUpsertRecord {
        root_token: kahawai_core::media::root_token(std::path::Path::new("/music")),
        path_rel: format!("loose-{number}.flac"),
        size: 100 + number,
        mtime_unix: 1,
        head_xxh3: number,
        tail_xxh3: number + 1,
        oshash: 0,
        streams_json: serde_json::json!({"tags":tags}).to_string(),
    }
}

async fn setup() -> (
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
        .upsert_files("host", "music", vec![file(1, false), file(2, false)])
        .await
        .unwrap();
    let copies: Vec<String> = sqlx::query_scalar("SELECT ps.item_id FROM files f JOIN playable_source_parts p ON p.file_id=f.id JOIN playable_sources ps ON ps.id=p.playable_source_id ORDER BY f.path_rel")
        .fetch_all(&db).await.unwrap();
    let parent: String = sqlx::query_scalar("SELECT a.library_item_id FROM collection_items ci JOIN collection_item_library_items a ON a.collection_item_id=ci.parent_id WHERE ci.id=?")
        .bind(&copies[0]).fetch_one(&db).await.unwrap();
    let mut targets = Vec::new();
    let mut tx = db.begin().await.unwrap();
    for (track, title) in [
        (1, "Chosen first"),
        (2, "Chosen second"),
        (3, "Rejected song"),
    ] {
        targets.push(
            kahawai_hub::library::create(
                &mut tx,
                kahawai_hub::library::NewItem {
                    kind: "song".into(),
                    title: title.into(),
                    year: None,
                    artist: None,
                    parent_id: Some(parent.clone()),
                    season: Some(1),
                    episode: Some(track),
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

async fn choose(db: &kahawai_hub::library::Database, copy: &str, target: &str) {
    let mut tx = db.begin().await.unwrap();
    kahawai_hub::library::assign(&mut tx, copy, &[target.to_owned()])
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn refresh(registry: &Registry) {
    registry
        .upsert_files("host", "music", vec![file(1, true), file(2, true)])
        .await
        .unwrap();
}

async fn choices(
    db: &kahawai_hub::library::Database,
) -> Vec<(String, String, bool, Option<String>)> {
    sqlx::query_as("SELECT c.id,a.library_item_id,c.assignment_manual,c.match_conflict FROM collection_items c JOIN collection_item_library_items a ON a.collection_item_id=c.id WHERE c.kind='track' ORDER BY c.id")
        .fetch_all(db).await.unwrap()
}

async fn file_copies(db: &kahawai_hub::library::Database) -> Vec<String> {
    sqlx::query_scalar("SELECT ps.item_id FROM files f JOIN playable_source_parts p ON p.file_id=f.id JOIN playable_sources ps ON ps.id=p.playable_source_id ORDER BY f.path_rel")
        .fetch_all(db).await.unwrap()
}

async fn rejections(db: &kahawai_hub::library::Database) -> Vec<(String, String)> {
    sqlx::query_as("SELECT collection_item_id,library_item_id FROM rejected_library_matches ORDER BY collection_item_id,library_item_id")
        .fetch_all(db).await.unwrap()
}

async fn reject(db: &kahawai_hub::library::Database, copy: &str, target: &str) {
    sqlx::query("INSERT INTO rejected_library_matches VALUES(?,?)")
        .bind(copy)
        .bind(target)
        .execute(db)
        .await
        .unwrap();
}

#[tokio::test]
async fn song_slot_collision_preserves_the_only_explicit_choice_and_rejections() {
    let (db, registry, copies, targets) = setup().await;
    choose(&db, &copies[1], &targets[1]).await;
    reject(&db, &copies[1], &targets[2]).await;
    for _ in 0..2 {
        refresh(&registry).await;
        assert_eq!(
            choices(&db).await,
            vec![(copies[0].clone(), targets[1].clone(), true, None)]
        );
        assert_eq!(
            rejections(&db).await,
            vec![(copies[0].clone(), targets[2].clone())]
        );
        assert_eq!(file_copies(&db).await, vec![copies[0].clone(); 2]);
    }
}

#[tokio::test]
async fn song_slot_collision_transfers_pinned_answer_and_provider_rejection() {
    let (db, registry, copies, targets) = setup().await;
    choose(&db, &copies[1], &targets[1]).await;
    reject(&db, &copies[1], &targets[2]).await;
    sqlx::query("INSERT INTO provider_metadata(item_id,provider,provider_id,title,poster_path,provider_artist_id,confidence,updated_at)
        VALUES(?,'musicbrainz','chosen-recording','Chosen second','chosen.jpg','chosen-artist','manual',1)")
        .bind(&copies[1]).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO manual_match(item_id,provider,provider_id,pinned_at) VALUES(?,'musicbrainz','chosen-recording',1)")
        .bind(&copies[1]).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO rejected_matches(item_id,provider,provider_id,rejected_at) VALUES(?,'musicbrainz','wrong-recording',1)")
        .bind(&copies[1]).execute(&db).await.unwrap();
    for _ in 0..2 {
        refresh(&registry).await;
        assert_eq!(
            choices(&db).await,
            vec![(copies[0].clone(), targets[1].clone(), true, None)]
        );
        let answer: (String, String, String) = sqlx::query_as("SELECT p.provider_id,p.poster_path,p.provider_artist_id FROM provider_metadata p JOIN manual_match m ON m.item_id=p.item_id AND m.provider=p.provider AND m.provider_id=p.provider_id WHERE p.item_id=?")
            .bind(&copies[0]).fetch_one(&db).await.unwrap();
        assert_eq!(
            answer,
            (
                "chosen-recording".into(),
                "chosen.jpg".into(),
                "chosen-artist".into()
            )
        );
        let rejected: String =
            sqlx::query_scalar("SELECT provider_id FROM rejected_matches WHERE item_id=?")
                .bind(&copies[0])
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(rejected, "wrong-recording");
        assert_eq!(
            rejections(&db).await,
            vec![(copies[0].clone(), targets[2].clone())]
        );
        assert_eq!(file_copies(&db).await, vec![copies[0].clone(); 2]);
    }
}

#[tokio::test]
async fn song_slot_collision_preserves_conflicting_positive_choices_until_resolved() {
    let (db, registry, copies, targets) = setup().await;
    for n in 0..2 {
        choose(&db, &copies[n], &targets[n]).await;
        reject(&db, &copies[n], &targets[2]).await;
    }
    for _ in 0..2 {
        refresh(&registry).await;
        let assignments = choices(&db).await;
        assert_eq!(
            assignments.len(),
            2,
            "a slot collision cannot discard a human choice between different recordings"
        );
        for (copy, target, manual, conflict) in assignments {
            let n = copies.iter().position(|id| id == &copy).unwrap();
            assert_eq!(target, targets[n]);
            assert!(manual);
            assert!(
                conflict.is_some(),
                "both peers must show the unresolved choice"
            );
        }
        assert_eq!(rejections(&db).await.len(), 2);
        assert_eq!(file_copies(&db).await, copies);
    }
    choose(&db, &copies[1], &targets[0]).await;
    assert!(
        choices(&db).await.iter().all(|row| row.3.is_none()),
        "resolution must refresh both peers"
    );
    for _ in 0..2 {
        refresh(&registry).await;
        assert_eq!(
            choices(&db).await,
            vec![(copies[0].clone(), targets[0].clone(), true, None)]
        );
        assert_eq!(
            rejections(&db).await,
            vec![(copies[0].clone(), targets[2].clone())]
        );
        assert_eq!(file_copies(&db).await, vec![copies[0].clone(); 2]);
    }
}

#[tokio::test]
async fn song_slot_collision_preserves_a_conflicting_rejection_until_resolved() {
    let (db, registry, copies, targets) = setup().await;
    choose(&db, &copies[0], &targets[0]).await;
    reject(&db, &copies[1], &targets[0]).await;
    for _ in 0..2 {
        refresh(&registry).await;
        assert_eq!(
            rejections(&db).await,
            vec![(copies[1].clone(), targets[0].clone())]
        );
        assert_eq!(file_copies(&db).await, copies);
        let assignments = choices(&db).await;
        assert_eq!(assignments.len(), 2);
        assert!(assignments.iter().all(|row| row.3.is_some()));
    }
    choose(&db, &copies[1], &targets[0]).await;
    assert!(choices(&db).await.iter().all(|row| row.3.is_none()));
    refresh(&registry).await;
    assert_eq!(
        choices(&db).await,
        vec![(copies[0].clone(), targets[0].clone(), true, None)]
    );
    assert!(rejections(&db).await.is_empty());
}

#[tokio::test]
async fn correcting_song_position_clears_the_former_peers_conflict() {
    let (db, registry, copies, targets) = setup().await;
    for n in 0..2 {
        choose(&db, &copies[n], &targets[n]).await;
    }
    refresh(&registry).await;
    assert!(choices(&db).await.iter().all(|row| row.3.is_some()));
    let mut moved = file(2, true);
    let mut info: serde_json::Value = serde_json::from_str(&moved.streams_json).unwrap();
    info["tags"]["track_number"] = "2".into();
    moved.streams_json = info.to_string();
    registry
        .upsert_files("host", "music", vec![moved])
        .await
        .unwrap();
    assert_eq!(file_copies(&db).await, copies);
    assert_eq!(choices(&db).await.len(), 2);
    assert!(choices(&db).await.iter().all(|row| row.3.is_none()));
}

#[tokio::test]
async fn song_slot_conflicts_follow_rejected_aliases_and_clear_on_explicit_correction() {
    let (db, registry, copies, targets) = setup().await;
    // The first copy's unidentified library ID starts as its physical ID.
    reject(&db, &copies[1], &copies[0]).await;
    choose(&db, &copies[0], &targets[0]).await;
    assert_eq!(
        kahawai_hub::library::resolve_id(&db, &copies[0])
            .await
            .unwrap(),
        targets[0]
    );
    for _ in 0..2 {
        refresh(&registry).await;
        assert_eq!(file_copies(&db).await, copies);
        let assignments = choices(&db).await;
        assert_eq!(assignments.len(), 2);
        assert!(assignments.iter().all(|row| row.3.is_some()));
        assert_eq!(
            rejections(&db).await,
            vec![(copies[1].clone(), copies[0].clone())]
        );
    }
    choose(&db, &copies[1], &targets[0]).await;
    assert!(rejections(&db).await.is_empty());
    assert!(choices(&db).await.iter().all(|row| row.3.is_none()));
    refresh(&registry).await;
    assert_eq!(
        choices(&db).await,
        vec![(copies[0].clone(), targets[0].clone(), true, None)]
    );
}
