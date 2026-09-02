//! Series resolution (M4): episode files land under show items, the
//! hierarchy survives reconciliation, and childless shows are swept.

use kahawai_hub::registry::{FileUpsertRecord, Registry};
use sqlx::Row;

const TEST_ROOT: &str = "/kahawai-test-root";

fn rec(path: &str, size: u64) -> FileUpsertRecord {
    FileUpsertRecord {
        root_token: kahawai_core::media::root_token(std::path::Path::new(TEST_ROOT)),
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
    registry
        .record_satellite("01HOST", "mediahost", "nas", "fp")
        .await
        .unwrap();
    registry
        .announce_collection("01HOST", "series", "series", &[TEST_ROOT.into()])
        .await
        .unwrap();

    registry
        .upsert_files(
            "01HOST",
            "series",
            vec![
                rec("Andor/Season 1/Star Wars - Andor - S01E01 - Kassa.mkv", 100),
                rec(
                    "Andor/Season 1/Star Wars - Andor - S01E02 - That Would Be Me.mkv",
                    101,
                ),
                rec("Andor/Season 2/Andor.S02E01.1080p.mkv", 102),
                rec(
                    "The Wire (2002)/Season 3/The.Wire.S03E11.Middle.Ground.mkv",
                    103,
                ),
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
            vec![rec(
                "Andor/Season 1/Star Wars - Andor - S01E01 - Kassa.mkv",
                100,
            )],
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
    let keep: std::collections::HashSet<kahawai_hub::registry::SourcePath> = [
        "Andor/Season 1/Star Wars - Andor - S01E01 - Kassa.mkv",
        "Andor/Season 1/Star Wars - Andor - S01E02 - That Would Be Me.mkv",
        "Andor/Season 2/Andor.S02E01.1080p.mkv",
        "Andor/Season 1/behind-the-scenes.mkv",
    ]
    .into_iter()
    .map(|path| kahawai_hub::registry::SourcePath {
        root_token: kahawai_core::media::root_token(std::path::Path::new(TEST_ROOT)),
        path_rel: path.into(),
    })
    .collect();
    registry
        .reconcile_files("01HOST", "series", &keep)
        .await
        .unwrap();
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
    registry
        .record_satellite("01HOST", "mediahost", "nas", "fp")
        .await
        .unwrap();
    registry
        .announce_collection("01HOST", "movies", "movies", &[TEST_ROOT.into()])
        .await
        .unwrap();
    registry
        .announce_collection("01HOST", "anime", "anime", &[TEST_ROOT.into()])
        .await
        .unwrap();

    // One auto-library per collection, matching name + type.
    let libs = registry.libraries_overview().await.unwrap();
    assert_eq!(libs.len(), 2, "{libs:?}");
    let anime = libs.iter().find(|library| library.name == "anime").unwrap();
    assert_eq!(anime.media_type, "anime");
    assert_eq!(anime.collections.len(), 1);

    // Re-announce (reconnect) must not duplicate memberships.
    registry
        .announce_collection("01HOST", "movies", "movies", &[TEST_ROOT.into()])
        .await
        .unwrap();
    assert_eq!(registry.libraries_overview().await.unwrap().len(), 2);

    // Type enforcement: a movies collection cannot join an anime library.
    let anime_id = anime.id.as_str();
    let err = registry
        .attach_collection(anime_id, "01HOST", "movies")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("type mismatch"), "{err:#}");

    // Manual library of the right type accepts it.
    let extra = registry
        .create_library("everything-movies", "movies")
        .await
        .unwrap();
    registry
        .attach_collection(&extra, "01HOST", "movies")
        .await
        .unwrap();
    // Bad media type rejected outright.
    assert!(registry.create_library("nope", "podcasts").await.is_err());
    // Delete cascades memberships.
    assert!(registry.delete_library(&extra).await.unwrap());
}

#[tokio::test]
async fn resolves_music_into_albums_and_tracks() {
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    let registry = Registry::new(db.clone(), Default::default());
    registry
        .record_satellite("01HOST", "mediahost", "nas", "fp")
        .await
        .unwrap();
    registry
        .announce_collection("01HOST", "music", "music", &[TEST_ROOT.into()])
        .await
        .unwrap();

    // Tagged file: tags win over the filename.
    let tagged = FileUpsertRecord {
        root_token: kahawai_core::media::root_token(std::path::Path::new(TEST_ROOT)),
        streams_json: serde_json::json!({
            "tags": {
                "artist": "Rotting Christ",
                "album": "Khronos",
                "title": "Thou Art Blind",
                "track_number": "1",
            }
        })
        .to_string(),
        ..rec(
            "Rotting Christ/Khronos (2000)/Rotting Christ - Khronos - 01 - WRONG.flac",
            10,
        )
    };
    // Untagged: the Lidarr filename fallback fires.
    let untagged = rec(
        "Rotting Christ/Khronos (2000)/Rotting Christ - Khronos - 02 - If It Ends Tomorrow.flac",
        11,
    );
    // Same album name, different artist: must be a separate album.
    let other = FileUpsertRecord {
        root_token: kahawai_core::media::root_token(std::path::Path::new(TEST_ROOT)),
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

    let albums: Vec<(String, String, Option<i64>)> =
        sqlx::query("SELECT title, artist, year FROM items WHERE kind = 'album' ORDER BY artist")
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
            (
                "Khronos".to_string(),
                "Rotting Christ".to_string(),
                Some(2000)
            ),
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
        vec![
            (1, "Thou Art Blind".to_string()),
            (2, "If It Ends Tomorrow".to_string())
        ],
        "tags must beat the WRONG filename title: {tracks:?}"
    );

    // Reconcile away one artist: their album dies of childlessness.
    let keep: std::collections::HashSet<kahawai_hub::registry::SourcePath> = [
        "Rotting Christ/Khronos (2000)/Rotting Christ - Khronos - 01 - WRONG.flac",
        "Rotting Christ/Khronos (2000)/Rotting Christ - Khronos - 02 - If It Ends Tomorrow.flac",
        "Rotting Christ/Khronos (2000)/rip-log.flac",
    ]
    .into_iter()
    .map(|path| kahawai_hub::registry::SourcePath {
        root_token: kahawai_core::media::root_token(std::path::Path::new(TEST_ROOT)),
        path_rel: path.into(),
    })
    .collect();
    registry
        .reconcile_files("01HOST", "music", &keep)
        .await
        .unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE kind = 'album'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(n, 1, "childless album should be swept");
}

#[tokio::test]
async fn album_artist_groups_compilations_and_preserves_existing_identity_and_state() {
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    let registry = Registry::new(db.clone(), Default::default());
    registry
        .record_satellite("01HOST", "mediahost", "nas", "fp")
        .await
        .unwrap();
    registry
        .announce_collection("01HOST", "music", "music", &[TEST_ROOT.into()])
        .await
        .unwrap();

    let tagged = |path: &str, size: u64, artist: &str, track: u32, album_artist: Option<&str>| {
        let mut tags = serde_json::json!({
            "artist": artist,
            "album": "Creme de la Core",
            "title": format!("Song {track}"),
            "track_number": track.to_string(),
        });
        if let Some(album_artist) = album_artist {
            tags["album_artist"] = album_artist.into();
        }
        FileUpsertRecord {
            streams_json: serde_json::json!({"tags": tags}).to_string(),
            ..rec(path, size)
        }
    };

    // This is how an already indexed loose file looked before Album Artist
    // extraction: without a usable directory fallback, its song artist owned
    // the album.
    registry
        .upsert_files(
            "01HOST",
            "music",
            vec![
                tagged("loose-one.flac", 20, "Guest One", 1, None),
                tagged("loose-two.flac", 21, "Guest Two", 2, None),
            ],
        )
        .await
        .unwrap();
    let old_tracks: Vec<(String, String)> =
        sqlx::query_as("SELECT id,artist FROM items WHERE kind='track' ORDER BY episode")
            .fetch_all(&db)
            .await
            .unwrap();
    let retained_album: String = sqlx::query_scalar("SELECT parent_id FROM items WHERE id=?")
        .bind(&old_tracks[0].0)
        .fetch_one(&db)
        .await
        .unwrap();
    let pinned_album: String = sqlx::query_scalar("SELECT parent_id FROM items WHERE id=?")
        .bind(&old_tracks[1].0)
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO provider_metadata
           (item_id,provider,provider_id,title,poster_path,confidence,updated_at)
         VALUES(?,'musicbrainz','wrong-release','Wrong release','wrong.jpg','auto',1);
         INSERT INTO provider_queries(item_id,provider,query_type,query,rev,asked_at)
         VALUES(?,'musicbrainz','title','guest 1|creme de la core',1,1);
         INSERT INTO provider_metadata
           (item_id,provider,provider_id,title,poster_path,confidence,updated_at)
         VALUES(?,'musicbrainz','human-release','Human release','human.jpg','manual',1);
         INSERT INTO manual_match(item_id,provider,provider_id,pinned_at)
         VALUES(?,'musicbrainz','human-release',1)",
    )
    .bind(&retained_album)
    .bind(&retained_album)
    .bind(&pinned_album)
    .bind(&pinned_album)
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO users(id,username,password_hash) VALUES('listener','listener','x');
         INSERT INTO watch_state
           (user_id,item_id,position_ms,duration_ms,played,play_count,updated_at)
         VALUES('listener',?,42000,180000,0,3,123),
               ('listener',?,84000,180000,0,2,124)",
    )
    .bind(&old_tracks[0].0)
    .bind(&old_tracks[1].0)
    .execute(&db)
    .await
    .unwrap();

    // The refreshed source moves under the compilation. A second recording
    // has a different song artist but the same release credit.
    registry
        .upsert_files(
            "01HOST",
            "music",
            vec![
                tagged(
                    "loose-one.flac",
                    20,
                    "Guest One",
                    1,
                    Some("Various Artists"),
                ),
                tagged(
                    "loose-two.flac",
                    21,
                    "Guest Two",
                    2,
                    Some("Various Artists"),
                ),
            ],
        )
        .await
        .unwrap();

    let albums: Vec<(String, String)> =
        sqlx::query_as("SELECT title,artist FROM items WHERE kind='album'")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(
        albums,
        [("Creme de la Core".into(), "Various Artists".into())]
    );
    let tracks: Vec<(i64, String)> =
        sqlx::query_as("SELECT episode,artist FROM items WHERE kind='track' ORDER BY episode")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(tracks, [(1, "Guest One".into()), (2, "Guest Two".into())]);
    let state: Vec<(i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT i.episode,w.position_ms,w.play_count,w.updated_at FROM watch_state w
          JOIN items i ON i.id=w.item_id WHERE i.kind='track' ORDER BY i.episode",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(
        state,
        [(1, 42_000, 3, 123), (2, 84_000, 2, 124)],
        "identity correction lost state"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items WHERE id=?")
            .bind(&old_tracks[0].0)
            .fetch_one(&db)
            .await
            .unwrap(),
        1,
        "the first track identity should survive the correction"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items WHERE id=?")
            .bind(&old_tracks[1].0)
            .fetch_one(&db)
            .await
            .unwrap(),
        1,
        "a free target slot should reparent rather than replace the second track"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT id FROM items WHERE kind='album'")
            .fetch_one(&db)
            .await
            .unwrap(),
        retained_album,
        "correcting the release credit replaced the album identity"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT provider_id FROM provider_metadata
              WHERE item_id=? AND provider='musicbrainz'",
        )
        .bind(&retained_album)
        .fetch_one(&db)
        .await
        .unwrap(),
        "human-release",
        "the old automatic answer survived or the converging human pin was lost"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM provider_queries
              WHERE item_id=? AND provider='musicbrainz'",
        )
        .bind(&retained_album)
        .fetch_one(&db)
        .await
        .unwrap(),
        0,
        "an old question can also suppress a future correction back to that artist"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT provider_id FROM manual_match WHERE item_id=?",)
            .bind(&retained_album)
            .fetch_one(&db)
            .await
            .unwrap(),
        "human-release",
        "the explicit match on the merged-away album was not preserved"
    );
}

#[tokio::test]
async fn album_artist_correction_preserves_a_human_musicbrainz_pin() {
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    let registry = Registry::new(db.clone(), Default::default());
    registry
        .record_satellite("01HOST", "mediahost", "nas", "fp")
        .await
        .unwrap();
    registry
        .announce_collection("01HOST", "music", "music", &[TEST_ROOT.into()])
        .await
        .unwrap();
    let tagged = |album_artist: Option<&str>| {
        let mut tags = serde_json::json!({
            "artist": "Guest", "album": "A Compilation",
            "title": "A Song", "track_number": "1"
        });
        if let Some(album_artist) = album_artist {
            tags["album_artist"] = album_artist.into();
        }
        FileUpsertRecord {
            streams_json: serde_json::json!({"tags": tags}).to_string(),
            ..rec("loose.flac", 30)
        }
    };
    registry
        .upsert_files("01HOST", "music", vec![tagged(None)])
        .await
        .unwrap();
    let album: String = sqlx::query_scalar("SELECT id FROM items WHERE kind='album'")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO provider_metadata
           (item_id,provider,provider_id,title,confidence,updated_at)
         VALUES(?,'musicbrainz','human-release','Human choice','manual',1);
         INSERT INTO manual_match(item_id,provider,provider_id,pinned_at)
         VALUES(?,'musicbrainz','human-release',1);
         INSERT INTO provider_queries(item_id,provider,query_type,query,rev,asked_at)
         VALUES(?,'musicbrainz','title','guest|a compilation',1,1)",
    )
    .bind(&album)
    .bind(&album)
    .bind(&album)
    .execute(&db)
    .await
    .unwrap();

    registry
        .upsert_files("01HOST", "music", vec![tagged(Some("Various Artists"))])
        .await
        .unwrap();

    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT provider_id FROM provider_metadata
              WHERE item_id=? AND provider='musicbrainz'",
        )
        .bind(&album)
        .fetch_one(&db)
        .await
        .unwrap(),
        "human-release"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM manual_match WHERE item_id=?")
            .bind(&album)
            .fetch_one(&db)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn replacement_at_the_same_path_does_not_inherit_item_or_watch_state() {
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    let registry = Registry::new(db.clone(), Default::default());
    registry
        .record_satellite("01HOST", "mediahost", "nas", "fp")
        .await
        .unwrap();
    registry
        .announce_collection("01HOST", "music", "music", &[TEST_ROOT.into()])
        .await
        .unwrap();
    let tagged = |size: u64, artist: &str, album: &str| FileUpsertRecord {
        streams_json: serde_json::json!({"tags": {
            "artist": artist, "album": album, "title": "Track", "track_number": "1"
        }})
        .to_string(),
        ..rec("mutable.flac", size)
    };
    registry
        .upsert_files(
            "01HOST",
            "music",
            vec![tagged(40, "Old Artist", "Old Album")],
        )
        .await
        .unwrap();
    let old_track: String = sqlx::query_scalar("SELECT id FROM items WHERE kind='track'")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO users(id,username,password_hash) VALUES('listener','listener','x');
         INSERT INTO watch_state
           (user_id,item_id,position_ms,duration_ms,played,play_count,updated_at)
         VALUES('listener',?,42000,180000,0,3,123)",
    )
    .bind(&old_track)
    .execute(&db)
    .await
    .unwrap();

    registry
        .upsert_files(
            "01HOST",
            "music",
            vec![tagged(41, "New Artist", "New Album")],
        )
        .await
        .unwrap();
    let new_track: String = sqlx::query_scalar("SELECT id FROM items WHERE kind='track'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_ne!(new_track, old_track);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM watch_state WHERE item_id=?")
            .bind(&new_track)
            .fetch_one(&db)
            .await
            .unwrap(),
        0,
        "different bytes inherited the displaced recording's state"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM watch_state_archive
              WHERE size=40 AND head_xxh3=40 AND tail_xxh3=41",
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        1,
        "the displaced content's state was not retained for a future return"
    );
}
