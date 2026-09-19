mod common;
use common::*;
use kahawai_mediadb::*;
use kahawai_proto::v1 as p;
use prost::Message;

#[tokio::test]
async fn library_ids_are_stable_and_assignments_are_copy_owned() {
    let (_dir, s) = store().await;
    let a = collection(
        &s,
        "a",
        MediaType::Movies,
        &[
            "Matrix.1999.1080p-A.mkv",
            "Matrix.1999.720p-B.mkv",
            "Matrix.2003.mkv",
            "Unknown-A.mkv",
            "Unknown-B.mkv",
        ],
    )
    .await;
    let b = collection(&s, "b", MediaType::Movies, &["Matrix.1999.mkv"]).await;
    let library = s
        .create_library("All", MediaType::Movies, &[a.clone(), b.clone()])
        .await
        .unwrap();
    let browse = s.browse(&library, 0, 100).await.unwrap();
    assert_eq!(browse.len(), 4);
    assert_eq!(
        browse
            .iter()
            .find(|g| g.year == Some(1999))
            .unwrap()
            .copy_ids
            .len(),
        3
    );
    let ai = s.collection_items(&a).await.unwrap();
    let bi = s.collection_items(&b).await.unwrap();
    let a_copy = ai.iter().find(|i| i.detected.year == Some(1999)).unwrap();
    let mut ar = record("tmdb", "a", "Matrix", Some(1999), MediaType::Movies);
    ar.description.overview = Some("Collection A".into());
    let ar = s.put_provider_record(&ar).await.unwrap();
    s.assign_metadata(&a_copy.id, Some(&ar)).await.unwrap();
    let mut br = record("tvdb", "b", "MATRIX", Some(1999), MediaType::Movies);
    br.description.overview = Some("Collection B".into());
    let br_id = s.put_provider_record(&br).await.unwrap();
    s.assign_metadata(&bi[0].id, Some(&br_id)).await.unwrap();
    let group = s
        .browse(&library, 0, 100)
        .await
        .unwrap()
        .into_iter()
        .find(|g| g.year == Some(1999))
        .unwrap();
    assert_eq!(
        group.metadata.description.overview.as_deref(),
        Some("Collection A")
    );
    s.set_library_collections(&library, &[b.clone(), a.clone()])
        .await
        .unwrap();
    assert_eq!(
        s.library_item(&library, &group.id)
            .await
            .unwrap()
            .metadata
            .description
            .overview
            .as_deref(),
        Some("Collection B")
    );
    let only_a = s
        .create_library("A only", MediaType::Movies, std::slice::from_ref(&a))
        .await
        .unwrap();
    assert_eq!(
        s.library_item(&only_a, &group.id)
            .await
            .unwrap()
            .metadata
            .description
            .overview
            .as_deref(),
        Some("Collection A")
    );
    br.title = "Other movie".into();
    br.year = Some(2010);
    s.put_provider_record(&br).await.unwrap();
    assert_eq!(
        s.library_item(&library, &group.id)
            .await
            .unwrap()
            .copy_ids
            .len(),
        2
    );
    assert_eq!(s.browse(&library, 0, 100).await.unwrap().len(), 5);
    s.assign_metadata(&bi[0].id, None).await.unwrap();
    assert_eq!(
        s.library_item(&library, &group.id)
            .await
            .unwrap()
            .copy_ids
            .len(),
        3
    );
    s.remove_collection(&b).await.unwrap();
    assert_eq!(s.browse(&library, 0, 100).await.unwrap().len(), 4);
    assert_eq!(s.browse(&library, 1, 2).await.unwrap().len(), 2);
}

#[tokio::test]
async fn fallback_is_per_type_provenance_is_explicit_and_reassignment_drops_supplements() {
    let (_dir, s) = store().await;
    let col = collection(&s, "c", MediaType::Movies, &["Copy.2000.mkv"]).await;
    let item = s.collection_items(&col).await.unwrap().remove(0).id;
    let mut primary = record("primary", "1", "Chosen", None, MediaType::Movies);
    primary.description.genres = Some(vec![]);
    let p = s.put_provider_record(&primary).await.unwrap();
    let mut secondary = record(
        "secondary",
        "1",
        "Other spelling",
        Some(2000),
        MediaType::Movies,
    );
    secondary.description.genres = Some(vec!["Drama".into()]);
    secondary.description.overview = Some("Secondary".into());
    let q = s.put_provider_record(&secondary).await.unwrap();
    let mut third = secondary.clone();
    third.provider = "third".into();
    third.description.overview = Some("Third".into());
    let r = s.put_provider_record(&third).await.unwrap();
    s.assign_metadata(&item, Some(&p)).await.unwrap();
    s.set_supplements(&item, &[q.clone(), r.clone()])
        .await
        .unwrap();
    s.set_provider_order(MediaType::Movies, &["third".into(), "secondary".into()])
        .await
        .unwrap();
    let resolved = s.resolve_metadata(&item).await.unwrap();
    assert_eq!(resolved.description.genres, Some(vec![]));
    assert_eq!(resolved.description.overview.as_deref(), Some("Third"));
    assert_eq!(resolved.provenance["overview"], r);
    assert_eq!(resolved.provenance["genres"], p);
    assert_eq!(resolved.providers[&p], "primary");
    assert_eq!(resolved.providers[&r], "third");
    assert!(
        !resolved.providers.contains_key(&q),
        "shadowed answers receive no credit"
    );
    let library = s
        .create_library("L", MediaType::Movies, &[col])
        .await
        .unwrap();
    assert_eq!(
        s.browse(&library, 0, 10).await.unwrap()[0].copy_ids,
        std::slice::from_ref(&item)
    );
    s.set_provider_order(MediaType::Movies, &["secondary".into(), "third".into()])
        .await
        .unwrap();
    assert_eq!(
        s.resolve_metadata(&item)
            .await
            .unwrap()
            .description
            .overview
            .as_deref(),
        Some("Secondary")
    );
    s.assign_metadata(&item, Some(&p)).await.unwrap();
    assert!(
        s.resolve_metadata(&item)
            .await
            .unwrap()
            .description
            .overview
            .is_some()
    );
    let replacement = s
        .put_provider_record(&record(
            "primary",
            "2",
            "Replacement",
            Some(2001),
            MediaType::Movies,
        ))
        .await
        .unwrap();
    s.assign_metadata(&item, Some(&replacement)).await.unwrap();
    assert!(
        s.resolve_metadata(&item)
            .await
            .unwrap()
            .description
            .overview
            .is_none()
    );
    assert!(s.set_supplements(&item, &[q.clone(), q]).await.is_err());
    let wrong = s
        .put_provider_record(&record("x", "a", "Album", Some(2001), MediaType::Music))
        .await
        .unwrap();
    assert!(s.assign_metadata(&item, Some(&wrong)).await.is_err());
}

#[tokio::test]
async fn albums_never_coalesce_and_discs_and_recording_artists_survive() {
    let (_dir, s) = store().await;
    let (col, _) = s
        .offer_collection("host", &offer("music", MediaType::Music, 3))
        .await
        .unwrap();
    s.apply_catalogue(
        "host",
        &delta(
            "music",
            true,
            true,
            3,
            vec![
                file_media(
                    1,
                    "Artist/Album (2001)/CD1/01 - A.flac",
                    tagged("Album", 1, 1),
                ),
                file_media(
                    2,
                    "Artist/Album (2001)/CD2/01 - B.flac",
                    tagged("Album", 2, 1),
                ),
                file_media(3, "Other copy/01 - A.flac", tagged("Album", 1, 1)),
            ],
        ),
    )
    .await
    .unwrap();
    let items = s.collection_items(&col).await.unwrap();
    assert_eq!(items.len(), 2);
    let metadata = s
        .put_provider_record(&record(
            "musicbrainz",
            "same",
            "Album",
            Some(2001),
            MediaType::Music,
        ))
        .await
        .unwrap();
    for i in &items {
        s.assign_metadata(&i.id, Some(&metadata)).await.unwrap();
        assert_eq!(i.detected.artist.as_deref(), Some("Album Artist"));
    }
    let library = s
        .create_library("Music", MediaType::Music, &[col])
        .await
        .unwrap();
    assert_eq!(s.browse(&library, 0, 20).await.unwrap().len(), 2);
    let mut entries = vec![];
    for i in &items {
        entries.extend(s.media_entries(&i.id).await.unwrap());
    }
    assert_eq!(entries.len(), 3);
    assert!(
        entries
            .iter()
            .all(|e| e.data.artist.as_deref() == Some("Guest"))
    );
    assert!(entries.iter().any(|e| matches!(
        e.data.kind,
        EntryKind::Track {
            disc: Some(2),
            track: Some(1)
        }
    )));
}

#[tokio::test]
async fn multipart_editions_and_combined_episodes_keep_their_physical_boundaries() {
    let (_dir, s) = store().await;
    let c = collection(
        &s,
        "movies",
        MediaType::Movies,
        &[
            "Film.2001.A.CD1.mkv",
            "Film.2001.A.CD2.mkv",
            "Film.2001.B.CD1.mkv",
            "Film.2001.B.CD2.mkv",
            "Film.2001.C.CD2.mkv",
        ],
    )
    .await;
    let items = s.collection_items(&c).await.unwrap();
    assert_eq!(items.len(), 3);
    for i in &items {
        let entries = s.media_entries(&i.id).await.unwrap();
        assert_eq!(entries.len(), 1);
        if i.occurrence.contains(".C.") {
            assert_eq!(entries[0].data.parts[0].ordinal, 2);
        } else {
            assert_eq!(
                entries[0]
                    .data
                    .parts
                    .iter()
                    .map(|p| p.ordinal)
                    .collect::<Vec<_>>(),
                vec![1, 2]
            );
        }
    }
    let series = collection(
        &s,
        "series",
        MediaType::Series,
        &[
            "Show (2000)/Season 1/Show.S01E01-E02.mkv",
            "Other copy/Show.S01E01.mkv",
        ],
    )
    .await;
    let series_items = s.collection_items(&series).await.unwrap();
    assert_eq!(series_items.len(), 2);
    let mut coverages = vec![];
    for i in series_items {
        for e in s.media_entries(&i.id).await.unwrap() {
            if let EntryKind::Episode { episodes } = e.data.kind {
                coverages.push(
                    episodes
                        .iter()
                        .map(|e| {
                            u64::from(e.episode_end.unwrap_or(e.episode)) - u64::from(e.episode) + 1
                        })
                        .sum::<u64>(),
                );
            }
        }
    }
    coverages.sort();
    assert_eq!(coverages, vec![1, 2]);
    let files = s.files(&c).await.unwrap();
    let first = &items[0];
    let mut entry = s.media_entries(&first.id).await.unwrap().remove(0).data;
    entry.parts = vec![
        Part {
            file_id: files[0].id.clone(),
            ordinal: 1,
        },
        Part {
            file_id: files[1].id.clone(),
            ordinal: 1,
        },
    ];
    assert!(
        s.put_occurrence(&NewOccurrence {
            collection_id: c,
            root_id: first.root_id.clone(),
            occurrence: "bad".into(),
            detected: first.detected.clone(),
            entries: vec![entry]
        })
        .await
        .is_err()
    );
}

#[tokio::test]
async fn root_namespaces_type_constraints_and_unresolved_files_are_explicit() {
    let (_dir, s) = store().await;
    let mut o = offer("roots", MediaType::Movies, 2);
    o.roots.push(p::CollectionRoot::new("second", "/other"));
    let (col, _) = s.offer_collection("host", &o).await.unwrap();
    let first = file(1, "Film.2000.mkv");
    let mut second = file(2, "Film.2000.mkv");
    second.key = b"second\0Film.2000.mkv".to_vec();
    let mut f = p::FileUpsert::decode(second.payload.as_slice()).unwrap();
    f.files[0].source.as_mut().unwrap().root_token = "second".into();
    second.payload = f.encode_to_vec();
    s.apply_catalogue("host", &delta("roots", true, true, 2, vec![first, second]))
        .await
        .unwrap();
    assert_eq!(s.collection_items(&col).await.unwrap().len(), 2);
    let series = collection(&s, "series", MediaType::Series, &["unparseable.mkv"]).await;
    assert!(s.collection_items(&series).await.unwrap().is_empty());
    assert_eq!(s.files(&series).await.unwrap().len(), 1);
    assert!(
        s.create_library("bad", MediaType::Movies, &[series])
            .await
            .is_err()
    );
    let library = s
        .create_library("ok", MediaType::Movies, std::slice::from_ref(&col))
        .await
        .unwrap();
    assert!(
        s.set_library_collections(&library, &[col.clone(), col])
            .await
            .is_err()
    );
    assert_eq!(s.browse(&library, 0, 10).await.unwrap().len(), 1);
    for invalid in [
        "../escape.mkv",
        "dir/../escape.mkv",
        "dir/./escape.mkv",
        "dir//escape.mkv",
        "/escape.mkv",
    ] {
        assert!(
            s.apply_catalogue(
                "host",
                &delta("roots", false, true, 3, vec![file(3, invalid)])
            )
            .await
            .is_err()
        );
    }
    let mut bad = delta("roots", false, true, 3, vec![file(3, "../escape.mkv")]);
    assert!(s.apply_catalogue("host", &bad).await.is_err());
    bad.records[0] = file(3, "Good.2000.mkv");
    bad.records[0].key = b"second\0Good.2000.mkv".to_vec();
    assert!(s.apply_catalogue("host", &bad).await.is_err());
}

#[test]
fn normalized_identity_preserves_meaningful_characters() {
    assert_eq!(title_key("  CAFÉ\tTest "), title_key("cafe\u{301} test"));
    assert_ne!(title_key("Cafe"), title_key("Café"));
    assert_ne!(title_key("A-B"), title_key("A B"));
}

#[tokio::test]
async fn missing_provider_year_keeps_equal_titles_separate_and_explicit_mapping_survives_import() {
    let (_dir, s) = store().await;
    let c = collection(
        &s,
        "movies",
        MediaType::Movies,
        &["A.2000.mkv", "B.2000.mkv"],
    )
    .await;
    let p = s
        .put_provider_record(&record(
            "tmdb",
            "unknown",
            "Same title",
            None,
            MediaType::Movies,
        ))
        .await
        .unwrap();
    for item in s.collection_items(&c).await.unwrap() {
        s.assign_metadata(&item.id, Some(&p)).await.unwrap();
    }
    let library = s
        .create_library("Missing years", MediaType::Movies, &[c])
        .await
        .unwrap();
    assert_eq!(s.browse(&library, 0, 100).await.unwrap().len(), 2);
    let series = collection(&s, "unresolved", MediaType::Series, &["unresolved.mkv"]).await;
    let source = s.files(&series).await.unwrap().remove(0);
    assert!(source.mapping_error.is_some());
    let manual = s
        .put_occurrence(&NewOccurrence {
            collection_id: series.clone(),
            root_id: source.root_id,
            occurrence: "manual".into(),
            detected: DetectedMetadata {
                title: "Known show".into(),
                year: Some(2000),
                artist: None,
                description: Description::default(),
            },
            entries: vec![NewEntry {
                occurrence: "combined".into(),
                title: "Combined episodes".into(),
                artist: None,
                kind: EntryKind::Episode {
                    episodes: vec![EpisodeSpan {
                        season: Some(1),
                        episode: 1,
                        episode_end: Some(2),
                    }],
                },
                parts: vec![Part {
                    file_id: source.id,
                    ordinal: 1,
                }],
            }],
        })
        .await
        .unwrap();
    s.apply_catalogue(
        "host",
        &delta(
            "unresolved",
            false,
            true,
            2,
            vec![file(2, "unresolved.mkv")],
        ),
    )
    .await
    .unwrap();
    let items = s.collection_items(&series).await.unwrap();
    assert_eq!(items[0].id, manual);
    assert_eq!(items[0].detected.title, "Known show");
    assert!(s.files(&series).await.unwrap()[0].mapping_error.is_none());
}

#[tokio::test]
async fn anime_can_use_native_and_western_provider_records_without_rekeying_episode_numbers() {
    let (_dir, s) = store().await;
    let c = collection(
        &s,
        "anime",
        MediaType::Anime,
        &["Show/[Group] Show - 01.mkv"],
    )
    .await;
    let item = s.collection_items(&c).await.unwrap().remove(0);
    let before = s.media_entries(&item.id).await.unwrap();
    let western = s
        .put_provider_record(&record(
            "tmdb",
            "show",
            "Chosen",
            Some(2000),
            MediaType::Series,
        ))
        .await
        .unwrap();
    s.assign_metadata(&item.id, Some(&western)).await.unwrap();
    let native = s
        .put_provider_record(&record(
            "anidb",
            "show",
            "Chosen",
            Some(2000),
            MediaType::Anime,
        ))
        .await
        .unwrap();
    s.set_supplements(&item.id, &[native]).await.unwrap();
    assert_eq!(
        serde_json::to_value(&before).unwrap(),
        serde_json::to_value(s.media_entries(&item.id).await.unwrap()).unwrap()
    );
}

#[tokio::test]
async fn bare_cd_and_part_filenames_take_the_movie_identity_from_the_directory() {
    let (_dir, s) = store().await;
    let c = collection(
        &s,
        "parts",
        MediaType::Movies,
        &[
            "Film (2000)/CD1.avi",
            "Film (2000)/CD2.avi",
            "Other (2001)/part 1.avi",
            "Other (2001)/part 2.avi",
            "Third (2002)/CD1/CD1.avi",
            "Third (2002)/CD2/CD2.avi",
        ],
    )
    .await;
    let items = s.collection_items(&c).await.unwrap();
    assert_eq!(items.len(), 3);
    for item in &items {
        assert!(matches!(
            item.detected.title.as_str(),
            "Film" | "Other" | "Third"
        ));
        let entries = s.media_entries(&item.id).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.parts.len(), 2);
    }
}

#[tokio::test]
async fn anime_library_distinguishes_movies_and_series_from_physical_entries() {
    let (_dir, s) = store().await;
    let c = collection(
        &s,
        "anime",
        MediaType::Anime,
        &["My.Neighbour.Totoro.1988.mkv", "Show/[Group] Show - 01.mkv"],
    )
    .await;
    let library = s
        .create_library("Anime", MediaType::Anime, &[c])
        .await
        .unwrap();
    let items = s.browse(&library, 0, 10).await.unwrap();
    assert_eq!(items.len(), 2);
    let movie = items
        .iter()
        .find(|i| i.kind == LibraryItemKind::Movie)
        .unwrap();
    let series = items
        .iter()
        .find(|i| i.kind == LibraryItemKind::Series)
        .unwrap();
    assert_eq!(movie.media_type, MediaType::Anime);
    assert_eq!(series.media_type, MediaType::Anime);
    assert_eq!(
        s.playback_item(&library, &movie.id)
            .await
            .unwrap()
            .renditions
            .len(),
        1
    );
    assert!(
        s.playback_item(&library, &series.id)
            .await
            .unwrap()
            .renditions
            .is_empty()
    );
}

#[tokio::test]
async fn attribution_includes_title_only_identity_and_clears_with_assignment() {
    let (_dir, s) = store().await;
    let col = collection(&s, "c", MediaType::Movies, &["Copy.2000.mkv"]).await;
    let item = s.collection_items(&col).await.unwrap().remove(0).id;
    let record = s
        .put_provider_record(&record(
            "tmdb",
            "1",
            "Chosen",
            Some(2000),
            MediaType::Movies,
        ))
        .await
        .unwrap();
    s.assign_metadata(&item, Some(&record)).await.unwrap();
    let resolved = s.resolve_metadata(&item).await.unwrap();
    assert!(resolved.provenance.is_empty());
    assert_eq!(resolved.providers[&record], "tmdb");
    s.assign_metadata(&item, None).await.unwrap();
    assert!(
        s.resolve_metadata(&item)
            .await
            .unwrap()
            .providers
            .is_empty()
    );
}
