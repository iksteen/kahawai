//! Series resolution (M4): episode files land under show items, the
//! hierarchy survives reconciliation, and childless shows are swept.

use kahawai_hub::registry::{FileUpsertRecord, Registry};
use sqlx::Row;

fn rec(path: &str, size: u64) -> FileUpsertRecord {
    FileUpsertRecord {
        path_rel: path.into(),
        size,
        mtime_unix: 1,
        head_xxh3: size,
        tail_xxh3: size + 1,
        oshash: size + 2,
        streams_json: "{}".into(),
    }
}

#[tokio::test]
async fn resolves_series_into_shows_and_episodes() {
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    let registry = Registry::new(db.clone(), Default::default());
    registry.record_satellite("01HOST", "mediahost", "nas", "fp").await.unwrap();
    registry
        .announce_collection("01HOST", "series", "series", &["/srv/series".into()])
        .await
        .unwrap();

    registry
        .upsert_files(
            "01HOST",
            "series",
            vec![
                rec("Andor/Season 1/Star Wars - Andor - S01E01 - Kassa.mkv", 100),
                rec("Andor/Season 1/Star Wars - Andor - S01E02 - That Would Be Me.mkv", 101),
                rec("Andor/Season 2/Andor.S02E01.1080p.mkv", 102),
                rec("The Wire (2002)/Season 3/The.Wire.S03E11.Middle.Ground.mkv", 103),
                // Unparseable: stored as a file, resolved to nothing.
                rec("Andor/Season 1/behind-the-scenes.mkv", 104),
            ],
        )
        .await
        .unwrap();

    let shows: Vec<(String, String)> =
        sqlx::query("SELECT id, title FROM items WHERE kind = 'show' ORDER BY title")
            .fetch_all(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.get("id"), r.get("title")))
            .collect();
    assert_eq!(
        shows.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(),
        ["Andor", "The Wire"],
        "{shows:?}"
    );

    let andor = &shows[0].0;
    let eps: Vec<(i64, i64, String)> = sqlx::query(
        "SELECT season, episode, title FROM items
         WHERE kind = 'episode' AND parent_id = ? ORDER BY season, episode",
    )
    .bind(andor)
    .fetch_all(&db)
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.get("season"), r.get("episode"), r.get("title")))
    .collect();
    assert_eq!(
        eps,
        vec![
            (1, 1, "Kassa".to_string()),
            (1, 2, "That Would Be Me".to_string()),
            (2, 1, "1080p".to_string()), // release junk as a title beats no title
        ],
        "{eps:?}"
    );

    // Re-upsert (rescan) must not duplicate anything.
    registry
        .upsert_files(
            "01HOST",
            "series",
            vec![rec("Andor/Season 1/Star Wars - Andor - S01E01 - Kassa.mkv", 100)],
        )
        .await
        .unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE kind = 'episode'")
        .fetch_all(&db)
        .await
        .unwrap()[0];
    assert_eq!(n, 4);

    // Reconcile away every Wire file: its episode goes, and the
    // childless show is swept with it.
    let keep: std::collections::HashSet<String> = [
        "Andor/Season 1/Star Wars - Andor - S01E01 - Kassa.mkv",
        "Andor/Season 1/Star Wars - Andor - S01E02 - That Would Be Me.mkv",
        "Andor/Season 2/Andor.S02E01.1080p.mkv",
        "Andor/Season 1/behind-the-scenes.mkv",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    registry.reconcile_files("01HOST", "series", &keep).await.unwrap();
    let titles: Vec<String> =
        sqlx::query_scalar("SELECT title FROM items WHERE kind = 'show' ORDER BY title")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(titles, ["Andor"], "childless show should be swept");
}

#[tokio::test]
async fn libraries_auto_provision_and_enforce_types() {
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    let registry = Registry::new(db.clone(), Default::default());
    registry.record_satellite("01HOST", "mediahost", "nas", "fp").await.unwrap();
    registry
        .announce_collection("01HOST", "movies", "movies", &["/srv/movies".into()])
        .await
        .unwrap();
    registry
        .announce_collection("01HOST", "anime", "anime", &["/srv/anime".into()])
        .await
        .unwrap();

    // One auto-library per collection, matching name + type.
    let libs = registry.libraries_overview().await.unwrap();
    assert_eq!(libs.len(), 2, "{libs:?}");
    let anime = libs.iter().find(|l| l["name"] == "anime").unwrap();
    assert_eq!(anime["media_type"], "anime");
    assert_eq!(anime["collections"].as_array().unwrap().len(), 1);

    // Re-announce (reconnect) must not duplicate memberships.
    registry
        .announce_collection("01HOST", "movies", "movies", &["/srv/movies".into()])
        .await
        .unwrap();
    assert_eq!(registry.libraries_overview().await.unwrap().len(), 2);

    // Type enforcement: a movies collection cannot join an anime library.
    let anime_id = anime["id"].as_str().unwrap();
    let err = registry
        .attach_collection(anime_id, "01HOST", "movies")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("type mismatch"), "{err:#}");

    // Manual library of the right type accepts it.
    let extra = registry.create_library("everything-movies", "movies").await.unwrap();
    registry.attach_collection(&extra, "01HOST", "movies").await.unwrap();
    // Bad media type rejected outright.
    assert!(registry.create_library("nope", "podcasts").await.is_err());
    // Delete cascades memberships.
    assert!(registry.delete_library(&extra).await.unwrap());
}

#[tokio::test]
async fn resolves_music_into_albums_and_tracks() {
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    let registry = Registry::new(db.clone(), Default::default());
    registry.record_satellite("01HOST", "mediahost", "nas", "fp").await.unwrap();
    registry
        .announce_collection("01HOST", "music", "music", &["/srv/music".into()])
        .await
        .unwrap();

    // Tagged file: tags win over the filename.
    let tagged = FileUpsertRecord {
        streams_json: serde_json::json!({
            "tags": {
                "artist": "Rotting Christ",
                "album": "Khronos",
                "title": "Thou Art Blind",
                "track_number": "1",
            }
        })
        .to_string(),
        ..rec("Rotting Christ/Khronos (2000)/Rotting Christ - Khronos - 01 - WRONG.flac", 10)
    };
    // Untagged: the Lidarr filename fallback fires.
    let untagged =
        rec("Rotting Christ/Khronos (2000)/Rotting Christ - Khronos - 02 - If It Ends Tomorrow.flac", 11);
    // Same album name, different artist: must be a separate album.
    let other = FileUpsertRecord {
        streams_json: serde_json::json!({
            "tags": {"artist": "Other Band", "album": "Khronos", "title": "Song", "track_number": "1"}
        })
        .to_string(),
        ..rec("Other Band/Khronos/01 - Song.flac", 12)
    };
    // Junk that parses to nothing stays a bare file.
    let junk = rec("Rotting Christ/Khronos (2000)/rip-log.flac", 13);
    registry
        .upsert_files("01HOST", "music", vec![tagged, untagged, other, junk])
        .await
        .unwrap();

    let albums: Vec<(String, String, Option<i64>)> = sqlx::query(
        "SELECT title, artist, year FROM items WHERE kind = 'album' ORDER BY artist",
    )
    .fetch_all(&db)
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.get("title"), r.get("artist"), r.get("year")))
    .collect();
    assert_eq!(
        albums,
        vec![
            ("Khronos".to_string(), "Other Band".to_string(), None),
            ("Khronos".to_string(), "Rotting Christ".to_string(), Some(2000)),
        ],
        "{albums:?}"
    );

    let tracks: Vec<(i64, String)> = sqlx::query(
        "SELECT i.episode, i.title FROM items i
         JOIN items a ON a.id = i.parent_id
         WHERE i.kind = 'track' AND a.artist = 'Rotting Christ'
         ORDER BY i.episode",
    )
    .fetch_all(&db)
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.get("episode"), r.get("title")))
    .collect();
    assert_eq!(
        tracks,
        vec![(1, "Thou Art Blind".to_string()), (2, "If It Ends Tomorrow".to_string())],
        "tags must beat the WRONG filename title: {tracks:?}"
    );

    // Reconcile away one artist: their album dies of childlessness.
    let keep: std::collections::HashSet<String> = [
        "Rotting Christ/Khronos (2000)/Rotting Christ - Khronos - 01 - WRONG.flac",
        "Rotting Christ/Khronos (2000)/Rotting Christ - Khronos - 02 - If It Ends Tomorrow.flac",
        "Rotting Christ/Khronos (2000)/rip-log.flac",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    registry.reconcile_files("01HOST", "music", &keep).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE kind = 'album'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(n, 1, "childless album should be swept");
}
