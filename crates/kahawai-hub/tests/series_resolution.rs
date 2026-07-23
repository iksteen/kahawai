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
