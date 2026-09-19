mod common;
use common::*;
use kahawai_mediadb::*;

fn episode(season: Option<u32>, episode: u32, title: &str) -> ProviderChild {
    ProviderChild {
        position: ProviderChildPosition {
            season,
            episode: Some(episode),
            ..Default::default()
        },
        title: title.into(),
        ..Default::default()
    }
}
async fn describe(s: &Store, copy: &str, title: &str, children: Vec<ProviderChild>) -> String {
    let r = s
        .put_provider_record(&ProviderRecord {
            provider: "fixture".into(),
            namespace: "show".into(),
            external_id: title.into(),
            language: "en".into(),
            media_type: MediaType::Series,
            title: title.into(),
            year: Some(2000),
            description: Description::default(),
            children: Some(children),
        })
        .await
        .unwrap();
    s.assign_metadata(copy, Some(&r)).await.unwrap();
    s.enrichment_input(copy).await.unwrap().library_item_id
}
#[tokio::test]
async fn positions_survive_metadata_reorder_copy_changes_and_restart() {
    let (dir, s) = store().await;
    let a = collection(
        &s,
        "a",
        MediaType::Series,
        &["Show (2000)/Show.S01E01-E02.mkv"],
    )
    .await;
    let b = collection(&s, "b", MediaType::Series, &["Show (2000)/Show.S01E01.mkv"]).await;
    let lib = s
        .create_library("Shows", MediaType::Series, &[a.clone(), b.clone()])
        .await
        .unwrap();
    let copy = s.collection_items(&a).await.unwrap().remove(0);
    let parent = describe(
        &s,
        &copy.id,
        "Show",
        vec![
            episode(Some(1), 2, "Second"),
            episode(Some(1), 1, "First"),
            episode(Some(1), 3, "Missing"),
        ],
    )
    .await;
    let page = s
        .library_children(&lib, &parent, 0, 200, &ChildFilter::default())
        .await
        .unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.children[0].title, "First");
    assert_eq!(page.children[0].source_count, 2);
    let id = page.children[0].id.clone();
    let second = page.children[1].id.clone();
    let detail = s.library_child(&lib, &id).await.unwrap();
    assert_eq!(detail.renditions.len(), 2);
    assert!(
        matches!(&detail.renditions[0].data.kind, EntryKind::Episode { episodes } if episodes[0].episode_end==Some(2))
    );
    describe(
        &s,
        &copy.id,
        "Show",
        vec![
            episode(Some(1), 1, "Renamed"),
            episode(Some(1), 2, "Second"),
        ],
    )
    .await;
    assert_eq!(
        s.library_child(&lib, &id).await.unwrap().child.title,
        "Renamed"
    );
    let moved = describe(&s, &copy.id, "Other", vec![]).await;
    assert_ne!(moved, parent);
    assert!(s.library_child(&lib, &second).await.is_err());
    assert_eq!(
        s.library_child(&lib, &id).await.unwrap().child.source_count,
        1
    );
    describe(&s, &copy.id, "Show", vec![]).await;
    assert_eq!(
        s.library_child(&lib, &second).await.unwrap().child.id,
        second
    );
    let original_entry = s.media_entries(&copy.id).await.unwrap().remove(0).data;
    let mut renumbered = original_entry.clone();
    renumbered.kind = EntryKind::Episode {
        episodes: vec![EpisodeSpan {
            season: Some(2),
            episode: 1,
            episode_end: None,
        }],
    };
    let mut occurrence = NewOccurrence {
        collection_id: a.clone(),
        root_id: copy.root_id.clone(),
        occurrence: copy.occurrence.clone(),
        detected: copy.detected.clone(),
        entries: vec![renumbered],
    };
    s.put_occurrence(&occurrence).await.unwrap();
    assert!(s.library_child(&lib, &second).await.is_err());
    let renumbered = ChildId {
        parent: parent.clone(),
        position: ChildPosition::Episode {
            season: Some(2),
            episode: 1,
        },
    }
    .encode();
    assert!(s.library_child(&lib, &renumbered).await.is_ok());
    occurrence.entries = vec![original_entry];
    s.put_occurrence(&occurrence).await.unwrap();
    assert_eq!(
        s.library_child(&lib, &second).await.unwrap().child.id,
        second
    );
    s.set_library_collections(&lib, &[b, a.clone()])
        .await
        .unwrap();
    assert_eq!(s.library_child(&lib, &id).await.unwrap().child.id, id);
    s.close().await;
    let s = Store::open(&dir.path().join("mediadb.db")).await.unwrap();
    assert_eq!(
        s.library_child(&lib, &second).await.unwrap().child.id,
        second
    );
    s.remove_collection(&a).await.unwrap();
    assert!(s.library_child(&lib, &second).await.is_err());
    let a = collection(
        &s,
        "a",
        MediaType::Series,
        &["Show (2000)/Replacement.S01E02.mkv"],
    )
    .await;
    let copy = s.collection_items(&a).await.unwrap().remove(0);
    describe(&s, &copy.id, "Show", vec![]).await;
    s.set_library_collections(&lib, &[a]).await.unwrap();
    assert_eq!(
        s.library_child(&lib, &second).await.unwrap().child.id,
        second
    );
}
#[tokio::test]
async fn ranges_are_paged_without_expansion_and_access_is_scoped() {
    let (_dir, s) = store().await;
    let col = collection(
        &s,
        "shows",
        MediaType::Series,
        &["Show (2000)/Show.S01E01.mkv"],
    )
    .await;
    let lib = s
        .create_library("Shows", MediaType::Series, std::slice::from_ref(&col))
        .await
        .unwrap();
    let copy = s.collection_items(&col).await.unwrap().remove(0);
    let mut entry = s.media_entries(&copy.id).await.unwrap().remove(0).data;
    entry.kind = EntryKind::Episode {
        episodes: vec![
            EpisodeSpan {
                season: Some(1),
                episode: 0,
                episode_end: Some(u32::MAX),
            },
            EpisodeSpan {
                season: None,
                episode: 1,
                episode_end: None,
            },
        ],
    };
    s.put_occurrence(&NewOccurrence {
        collection_id: col.clone(),
        root_id: copy.root_id,
        occurrence: copy.occurrence,
        detected: copy.detected,
        entries: vec![entry],
    })
    .await
    .unwrap();
    let finished = std::collections::BTreeSet::from([(Some(1), 0), (Some(1), 1)]);
    assert_eq!(
        s.next_episode(&lib, &copy.library_item_id, (None, 1), &finished)
            .await
            .unwrap()
            .unwrap()
            .child
            .position,
        ChildPosition::Episode {
            season: Some(1),
            episode: 2
        }
    );
    assert_eq!(
        s.next_episode(
            &lib,
            &copy.library_item_id,
            (Some(1), u32::MAX - 1),
            &finished
        )
        .await
        .unwrap()
        .unwrap()
        .child
        .position,
        ChildPosition::Episode {
            season: Some(1),
            episode: u32::MAX
        }
    );
    assert!(
        s.next_episode(&lib, &copy.library_item_id, (Some(1), u32::MAX), &finished)
            .await
            .unwrap()
            .is_none()
    );
    let filter = ChildFilter {
        season: Some(Some(1)),
        disc: None,
    };
    let page = s
        .library_children(
            &lib,
            &copy.library_item_id,
            u64::from(u32::MAX) - 1,
            2,
            &filter,
        )
        .await
        .unwrap();
    assert_eq!(page.total, u64::from(u32::MAX) + 1);
    assert_eq!(page.children.len(), 2);
    assert!(matches!(
        page.children[1].position,
        ChildPosition::Episode {
            episode: u32::MAX,
            ..
        }
    ));
    let seasonal = ChildId {
        parent: copy.library_item_id.clone(),
        position: ChildPosition::Episode {
            season: Some(1),
            episode: 1,
        },
    }
    .encode();
    let absolute = ChildId {
        parent: copy.library_item_id,
        position: ChildPosition::Episode {
            season: None,
            episode: 1,
        },
    }
    .encode();
    assert_ne!(seasonal, absolute);
    assert!(s.library_child(&lib, &absolute).await.is_ok());
    let hidden = s
        .create_library("Hidden", MediaType::Series, &[])
        .await
        .unwrap();
    assert!(s.library_child(&hidden, &seasonal).await.is_err());
    assert!(
        s.library_children(
            &lib,
            &ChildId::parse(&seasonal).unwrap().parent,
            0,
            201,
            &filter
        )
        .await
        .is_err()
    );
}
#[tokio::test]
async fn tracks_keep_album_boundaries_unknown_positions_and_physical_titles() {
    let (_dir, s) = store().await;
    let a = collection(
        &s,
        "a",
        MediaType::Music,
        &[
            "Artist/Album (2000)/01 - First.flac",
            "Artist/Album (2000)/02 - Second.flac",
        ],
    )
    .await;
    let b = collection(
        &s,
        "b",
        MediaType::Music,
        &["Artist/Album (2000)/01 - First.flac"],
    )
    .await;
    let lib = s
        .create_library("Music", MediaType::Music, &[a.clone(), b.clone()])
        .await
        .unwrap();
    let copy = s.collection_items(&a).await.unwrap().remove(0);
    let mut entries = s
        .media_entries(&copy.id)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.data)
        .collect::<Vec<_>>();
    entries[1].kind = EntryKind::Track {
        disc: None,
        track: None,
    };
    s.put_occurrence(&NewOccurrence {
        collection_id: a,
        root_id: copy.root_id,
        occurrence: copy.occurrence,
        detected: copy.detected,
        entries,
    })
    .await
    .unwrap();
    let page = s
        .library_children(&lib, &copy.library_item_id, 0, 200, &ChildFilter::default())
        .await
        .unwrap();
    assert_eq!(page.total, 2);
    let other = s.collection_items(&b).await.unwrap().remove(0);
    let other = s
        .library_children(
            &lib,
            &other.library_item_id,
            0,
            200,
            &ChildFilter::default(),
        )
        .await
        .unwrap();
    assert_ne!(page.children[0].id, other.children[0].id);
    assert!(matches!(
        page.children[1].position,
        ChildPosition::UnnumberedTrack { .. }
    ));
    for child in page.children {
        assert_eq!(ChildId::parse(&child.id).unwrap().encode(), child.id);
        assert_eq!(
            s.library_child(&lib, &child.id)
                .await
                .unwrap()
                .renditions
                .len(),
            1
        );
    }
    assert!(ChildId::parse("parent:description:1").is_err());
}

#[tokio::test]
async fn child_catalogue_precedence_is_separate_from_description_resolution() {
    let (_dir, s) = store().await;
    let collection = collection(&s, "a", MediaType::Series, &["Show (2000)/Show.S01E01.mkv"]).await;
    let lib = s
        .create_library(
            "Shows",
            MediaType::Series,
            std::slice::from_ref(&collection),
        )
        .await
        .unwrap();
    let copy = s.collection_items(&collection).await.unwrap().remove(0);
    let mut primary = record("primary", "1", "Show", Some(2000), MediaType::Series);
    primary.description.overview = Some("Parent synopsis".into());
    let primary_id = s.put_provider_record(&primary).await.unwrap();
    s.assign_metadata(&copy.id, Some(&primary_id))
        .await
        .unwrap();
    let mut supplement = record("supplement", "1", "Show", Some(2000), MediaType::Series);
    let mut first = episode(Some(1), 1, "First");
    first.provider_id = Some("external-episode".into());
    first.description = Description {
        overview: Some("Episode synopsis".into()),
        genres: Some(vec!["Drama".into()]),
        cast: Some(vec![Credit {
            name: "Actor".into(),
            role: Some("Role".into()),
        }]),
        artwork: Some(vec!["still-a".into(), "still-b".into()]),
        ..Default::default()
    };
    supplement.children = Some(vec![first.clone()]);
    let supplement_id = s.put_provider_record(&supplement).await.unwrap();
    s.set_provider_order(MediaType::Series, &["primary".into(), "supplement".into()])
        .await
        .unwrap();
    s.set_supplements(&copy.id, std::slice::from_ref(&supplement_id))
        .await
        .unwrap();
    let parent = s.enrichment_input(&copy.id).await.unwrap().library_item_id;
    let child = s
        .library_children(&lib, &parent, 0, 20, &ChildFilter::default())
        .await
        .unwrap()
        .children
        .remove(0);
    assert_eq!(child.title, "First");
    assert_eq!(child.metadata.description, first.description);
    for key in ["title", "overview", "genres", "cast", "artwork"] {
        assert_eq!(child.metadata.provenance[key], supplement_id);
    }
    let metadata = s.resolve_metadata(&copy.id).await.unwrap();
    assert_eq!(metadata.description.overview, primary.description.overview);
    assert!(!metadata.provenance.contains_key("children"));
    assert!(
        serde_json::to_value(metadata.description)
            .unwrap()
            .get("children")
            .is_none()
    );
    // An explicit empty primary answer suppresses supplements, without deleting
    // the physical child or changing its ID. Removing that answer restores them.
    primary.children = Some(vec![]);
    s.put_provider_record(&primary).await.unwrap();
    let empty = s.library_child(&lib, &child.id).await.unwrap().child;
    assert_eq!(empty.id, child.id);
    assert_eq!(empty.metadata.description, Description::default());
    assert_eq!(empty.metadata.provenance["title"], "detected");
    primary.children = None;
    s.put_provider_record(&primary).await.unwrap();
    assert_eq!(
        s.library_child(&lib, &child.id)
            .await
            .unwrap()
            .child
            .metadata
            .description,
        first.description
    );
}
