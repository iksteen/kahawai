mod common;
use common::*;
use kahawai_mediadb::*;
use kahawai_proto::v1 as p;
use sqlx::Connection;

#[tokio::test]
async fn corrections_archive_and_resurrect_without_restoring_deleted_assignments() {
    let (dir, s) = store().await;
    let a = collection(&s, "a", MediaType::Movies, &["The.Matrix.1999.mkv"]).await;
    let b = collection(&s, "b", MediaType::Movies, &["The.Matrix.1999.mkv"]).await;
    let library = s
        .create_library("All", MediaType::Movies, &[a.clone(), b.clone()])
        .await
        .unwrap();
    let first = s.collection_items(&a).await.unwrap().remove(0);
    let second = s.collection_items(&b).await.unwrap().remove(0);
    let matrix = first.library_item_id.clone();
    assert_eq!(matrix, second.library_item_id);
    let dark_record = s
        .put_provider_record(&record(
            "tmdb",
            "dark",
            "Dark City",
            Some(1998),
            MediaType::Movies,
        ))
        .await
        .unwrap();
    s.assign_metadata(&second.id, Some(&dark_record))
        .await
        .unwrap();
    let dark = s.collection_items(&b).await.unwrap()[0]
        .library_item_id
        .clone();
    assert_ne!(dark, matrix);
    assert_eq!(
        s.library_item(&library, &matrix).await.unwrap().copy_ids,
        std::slice::from_ref(&first.id)
    );
    s.assign_metadata(&first.id, Some(&dark_record))
        .await
        .unwrap();
    assert_eq!(
        s.collection_items(&a).await.unwrap()[0].library_item_id,
        dark
    );
    assert!(s.library_item_record(&matrix).await.unwrap().archived);
    assert!(s.library_item(&library, &matrix).await.is_err());
    s.assign_metadata(&first.id, None).await.unwrap();
    assert_eq!(
        s.collection_items(&a).await.unwrap()[0].library_item_id,
        matrix
    );
    assert!(!s.library_item_record(&matrix).await.unwrap().archived);
    s.set_library_collections(&library, &[]).await.unwrap();
    assert!(s.browse(&library, 0, 10).await.unwrap().is_empty());
    assert!(!s.library_item_record(&matrix).await.unwrap().archived);
    s.remove_library(&library).await.unwrap();
    s.remove_collection(&a).await.unwrap();
    s.remove_collection(&b).await.unwrap();
    assert!(s.library_item_record(&matrix).await.unwrap().archived);
    assert!(s.library_item_record(&dark).await.unwrap().archived);
    s.close().await;
    let s = Store::open(&dir.path().join("mediadb.db")).await.unwrap();
    let b = collection(&s, "b", MediaType::Movies, &["The.Matrix.1999.mkv"]).await;
    let returned = s.collection_items(&b).await.unwrap().remove(0);
    assert_ne!(returned.id, second.id);
    assert_eq!(returned.selected_record, None);
    assert_eq!(returned.library_item_id, matrix);
    assert!(s.library_item_record(&dark).await.unwrap().archived);
    s.assign_metadata(&returned.id, Some(&dark_record))
        .await
        .unwrap();
    assert_eq!(
        s.collection_items(&b).await.unwrap()[0].library_item_id,
        dark
    );
    s.remove_mediahost("host").await.unwrap();
    assert!(s.library_item_record(&dark).await.unwrap().archived);
    assert!(s.library_item_record(&matrix).await.unwrap().archived);
}

#[tokio::test]
async fn provider_identity_updates_only_primary_copies_and_preserves_library_scope() {
    let (_dir, s) = store().await;
    let a = collection(&s, "a", MediaType::Movies, &["Film.2000.mkv"]).await;
    let b = collection(&s, "b", MediaType::Movies, &["Film.2000.mkv"]).await;
    let c = collection(&s, "c", MediaType::Movies, &["Film.2000.mkv"]).await;
    let mut p = record("tmdb", "film", "Film", Some(2000), MediaType::Movies);
    let primary = s.put_provider_record(&p).await.unwrap();
    let copies = [
        s.collection_items(&a).await.unwrap().remove(0),
        s.collection_items(&b).await.unwrap().remove(0),
        s.collection_items(&c).await.unwrap().remove(0),
    ];
    let original = copies[0].library_item_id.clone();
    for copy in &copies[..2] {
        s.assign_metadata(&copy.id, Some(&primary)).await.unwrap();
    }
    let other = s
        .put_provider_record(&record(
            "tvdb",
            "film",
            "Film",
            Some(2000),
            MediaType::Movies,
        ))
        .await
        .unwrap();
    s.assign_metadata(&copies[2].id, Some(&other))
        .await
        .unwrap();
    s.set_supplements(&copies[2].id, std::slice::from_ref(&primary))
        .await
        .unwrap();
    let all = s
        .create_library("All", MediaType::Movies, &[a.clone(), b.clone(), c.clone()])
        .await
        .unwrap();
    let only_c = s
        .create_library("C", MediaType::Movies, std::slice::from_ref(&c))
        .await
        .unwrap();
    p.description.overview = Some("Updated description".into());
    p.title = " FILM ".into();
    s.put_provider_record(&p).await.unwrap();
    assert_eq!(
        s.collection_items(&a).await.unwrap()[0].library_item_id,
        original
    );
    p.title = "Different film".into();
    p.year = Some(2001);
    s.put_provider_record(&p).await.unwrap();
    let changed = s.collection_items(&a).await.unwrap()[0]
        .library_item_id
        .clone();
    assert_ne!(changed, original);
    assert_eq!(
        s.collection_items(&b).await.unwrap()[0].library_item_id,
        changed
    );
    assert_eq!(
        s.collection_items(&c).await.unwrap()[0].library_item_id,
        original
    );
    assert_eq!(
        s.library_item(&all, &changed).await.unwrap().copy_ids.len(),
        2
    );
    assert!(s.library_item(&only_c, &changed).await.is_err());
    assert_eq!(s.browse(&only_c, 0, 10).await.unwrap()[0].id, original);
    s.set_library_collections(&all, &[b.clone(), a, c])
        .await
        .unwrap();
    assert_eq!(
        s.library_item(&all, &changed)
            .await
            .unwrap()
            .representative_id,
        copies[1].id
    );
    p.title = "Film".into();
    p.year = Some(2000);
    s.put_provider_record(&p).await.unwrap();
    assert!(s.library_item_record(&changed).await.unwrap().archived);
    assert_eq!(
        s.library_item(&all, &original)
            .await
            .unwrap()
            .copy_ids
            .len(),
        3
    );
}

#[tokio::test]
async fn singletons_move_on_correction_but_new_occurrences_never_restore_them() {
    for (kind, paths) in [
        (
            MediaType::Music,
            [
                "Artist/Album (2001)/01 - Track.flac",
                "Other/Album (2001)/01 - Track.flac",
            ],
        ),
        (MediaType::Movies, ["Unknown-A.mkv", "Unknown-B.mkv"]),
    ] {
        let (_dir, s) = store().await;
        let c = collection(&s, "copies", kind, &paths).await;
        let copies = s.collection_items(&c).await.unwrap();
        assert_eq!(copies.len(), 2);
        let mut answer = record("provider", "same", "Corrected", None, kind);
        let p = s.put_provider_record(&answer).await.unwrap();
        for copy in &copies {
            s.assign_metadata(&copy.id, Some(&p)).await.unwrap();
        }
        let corrected = s.collection_items(&c).await.unwrap();
        assert_ne!(corrected[0].library_item_id, corrected[1].library_item_id);
        for (old, new) in copies.iter().zip(&corrected) {
            assert_ne!(old.library_item_id, new.library_item_id);
            assert!(
                s.library_item_record(&old.library_item_id)
                    .await
                    .unwrap()
                    .archived
            );
        }
        // Completing a year coalesces movies, but never physical albums.
        answer.year = Some(2002);
        s.put_provider_record(&answer).await.unwrap();
        let complete = s.collection_items(&c).await.unwrap();
        assert_eq!(
            complete[0].library_item_id == complete[1].library_item_id,
            kind == MediaType::Movies
        );
        answer.year = None;
        s.put_provider_record(&answer).await.unwrap();
        assert_eq!(
            s.collection_items(&c)
                .await
                .unwrap()
                .iter()
                .map(|i| &i.library_item_id)
                .collect::<Vec<_>>(),
            corrected
                .iter()
                .map(|i| &i.library_item_id)
                .collect::<Vec<_>>()
        );
        for copy in &copies {
            s.assign_metadata(&copy.id, None).await.unwrap();
        }
        assert_eq!(
            s.collection_items(&c)
                .await
                .unwrap()
                .iter()
                .map(|i| &i.library_item_id)
                .collect::<Vec<_>>(),
            copies
                .iter()
                .map(|i| &i.library_item_id)
                .collect::<Vec<_>>()
        );
        s.remove_collection(&c).await.unwrap();
        let c = collection(&s, "copies", kind, &paths).await;
        for new in s.collection_items(&c).await.unwrap() {
            assert!(
                copies
                    .iter()
                    .all(|old| old.library_item_id != new.library_item_id)
            );
            assert!(new.selected_record.is_none());
        }
    }
}

#[tokio::test]
async fn occurrence_edits_and_failed_mutations_keep_membership_atomic() {
    let (dir, s) = store().await;
    let c = collection(&s, "copies", MediaType::Movies, &["Film.2000.mkv"]).await;
    let copy = s.collection_items(&c).await.unwrap().remove(0);
    let entries = s
        .media_entries(&copy.id)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.data)
        .collect();
    let mut edit = NewOccurrence {
        collection_id: c.clone(),
        root_id: copy.root_id.clone(),
        occurrence: copy.occurrence.clone(),
        detected: copy.detected.clone(),
        entries,
    };
    edit.detected.title = "Corrected".into();
    s.put_occurrence(&edit).await.unwrap();
    let corrected = s.collection_items(&c).await.unwrap()[0]
        .library_item_id
        .clone();
    assert_ne!(corrected, copy.library_item_id);
    let primary = s
        .put_provider_record(&record(
            "tmdb",
            "p",
            "Selected",
            Some(2003),
            MediaType::Movies,
        ))
        .await
        .unwrap();
    s.assign_metadata(&copy.id, Some(&primary)).await.unwrap();
    let selected = s.collection_items(&c).await.unwrap()[0]
        .library_item_id
        .clone();
    edit.detected.title = "Changed detection".into();
    s.put_occurrence(&edit).await.unwrap();
    assert_eq!(
        s.collection_items(&c).await.unwrap()[0].library_item_id,
        selected
    );
    let mut db = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.path().join("mediadb.db"))
            .read_only(true),
    )
    .await
    .unwrap();
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM library_items")
        .fetch_one(&mut db)
        .await
        .unwrap();
    edit.occurrence = "invalid".into();
    edit.detected.title = "Must roll back".into();
    edit.entries[0].parts[0].file_id = "missing".into();
    assert!(s.put_occurrence(&edit).await.is_err());
    let mut bad = file(3, "Bad.2000.mkv");
    bad.kind = "unsupported".into();
    assert!(
        s.apply_catalogue(
            "host",
            &delta(
                "copies",
                false,
                true,
                3,
                vec![file(2, "Must.Roll.Back.2000.mkv"), bad]
            )
        )
        .await
        .is_err()
    );
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM library_items")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(s.catalogue_cursor(&c).await.unwrap().version, 1);
    assert_eq!(
        s.collection_items(&c).await.unwrap()[0].library_item_id,
        selected
    );
    assert_eq!(s.files(&c).await.unwrap().len(), 1);
    // Tombstoning the final source archives the selected item without an archive write.
    let tombstone = p::CatalogRecord {
        version: 2,
        kind: "file".into(),
        key: b"root\0Film.2000.mkv".to_vec(),
        deleted: true,
        ..Default::default()
    };
    s.apply_catalogue("host", &delta("copies", false, true, 2, vec![tombstone]))
        .await
        .unwrap();
    assert!(s.library_item_record(&selected).await.unwrap().archived);
}

#[tokio::test]
async fn concurrent_imports_share_one_identity_and_replay_keeps_it() {
    let (_dir, s) = store().await;
    let (a, b) = tokio::join!(
        collection(&s, "a", MediaType::Movies, &["Film.2000.mkv"]),
        collection(&s, "b", MediaType::Movies, &["Film.2000.mkv"])
    );
    let first = s.collection_items(&a).await.unwrap().remove(0);
    assert_eq!(
        s.collection_items(&b).await.unwrap()[0].library_item_id,
        first.library_item_id
    );
    s.apply_catalogue(
        "host",
        &delta("a", false, true, 1, vec![file(1, "Film.2000.mkv")]),
    )
    .await
    .unwrap();
    assert_eq!(
        s.collection_items(&a).await.unwrap()[0].library_item_id,
        first.library_item_id
    );
}

#[tokio::test]
async fn viewer_pages_count_identities_filter_sort_and_respect_membership() {
    let (_dir, s) = store().await;
    let a = collection(
        &s,
        "a",
        MediaType::Movies,
        &["Dark.City.1998.mkv", "The.Matrix.1999.mkv"],
    )
    .await;
    let b = collection(
        &s,
        "b",
        MediaType::Movies,
        &["Dark.City.1998.mkv", "Alien.1979.mkv"],
    )
    .await;
    let library = s
        .create_library("Films", MediaType::Movies, &[a.clone(), b])
        .await
        .unwrap();
    let (page, total) = s
        .browse_page(&library, 1, 1, "", "-year", None)
        .await
        .unwrap();
    assert_eq!(total, 3); // Two Dark City copies occupy one grid position.
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].year, Some(1998));
    assert_eq!(page[0].copy_ids.len(), 2);
    let (page, total) = s
        .browse_page(&library, 0, 10, "CITY", "title", None)
        .await
        .unwrap();
    assert_eq!(total, 1);
    assert_eq!(page.len(), 1);
    assert_eq!(
        s.browse_page(&library, 99, 10, "", "title", None)
            .await
            .unwrap()
            .1,
        3
    );
    s.set_library_collections(&library, &[a]).await.unwrap();
    let (page, total) = s
        .browse_page(&library, 0, 10, "", "title", None)
        .await
        .unwrap();
    assert_eq!(total, 2);
    assert_eq!(page[0].copy_ids.len(), 1);
    assert!(
        s.browse_page(&library, 0, 10, "", "title; DROP TABLE libraries", None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn artist_pages_keep_album_copies_separate_and_library_scoped() {
    let (_dir, s) = store().await;
    let mut collections = Vec::new();
    for remote in ["a", "b"] {
        let (collection, _) = s
            .offer_collection("host", &offer(remote, MediaType::Music, 1))
            .await
            .unwrap();
        s.apply_catalogue(
            "host",
            &delta(
                remote,
                true,
                true,
                1,
                vec![file_media(1, "Album/01.flac", tagged("Album", 1, 1))],
            ),
        )
        .await
        .unwrap();
        collections.push(collection);
    }
    let library = s
        .create_library("Music", MediaType::Music, &collections)
        .await
        .unwrap();
    let (artists, total) = s
        .browse_artists(&library, 0, 10, "album", false)
        .await
        .unwrap();
    assert_eq!(total, 1);
    assert_eq!(artists, vec![("Album Artist".into(), 2)]);
    let (albums, total) = s
        .browse_page(&library, 0, 10, "", "year", Some("Album Artist"))
        .await
        .unwrap();
    assert_eq!(total, 2);
    assert_ne!(albums[0].id, albums[1].id);
    s.set_library_collections(&library, &collections[..1])
        .await
        .unwrap();
    assert_eq!(
        s.browse_artists(&library, 0, 10, "", true).await.unwrap().0[0].1,
        1
    );
    assert_eq!(
        s.browse_artists(&library, 1, 10, "", false).await.unwrap(),
        (vec![], 1)
    );
    assert_eq!(
        s.browse_artists(&library, 0, 10, "absent", false)
            .await
            .unwrap()
            .1,
        0
    );
}
