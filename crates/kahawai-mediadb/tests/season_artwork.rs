mod common;
use common::*;
use kahawai_mediadb::*;

fn cover(scope: SeasonArtworkScope, url: &str) -> ProviderSeasonArtwork {
    ProviderSeasonArtwork {
        provider_id: None,
        scope,
        artwork: vec![url.into()],
    }
}

#[tokio::test]
async fn season_answers_roundtrip_without_manufacturing_children() {
    let (_dir, s) = store().await;
    let col = collection(
        &s,
        "shows",
        MediaType::Series,
        &["Show (2000)/Show.S00E01.mkv", "Show (2000)/Show.S02E01.mkv"],
    )
    .await;
    let lib = s
        .create_library("Shows", MediaType::Series, std::slice::from_ref(&col))
        .await
        .unwrap();
    let copy = s.collection_items(&col).await.unwrap().remove(0);
    let mut r = record("tmdb", "1", "Show", Some(2000), MediaType::Series);
    let old = serde_json::to_value(&r).unwrap();
    let mut old = old.as_object().unwrap().clone();
    old.remove("season_artwork");
    assert!(
        serde_json::from_value::<ProviderRecord>(old.into())
            .unwrap()
            .season_artwork
            .is_none()
    );
    r.season_artwork = Some(vec![
        cover(SeasonArtworkScope::Season { number: 0 }, "specials"),
        cover(SeasonArtworkScope::Season { number: 2 }, "second"),
        cover(SeasonArtworkScope::Season { number: 99 }, "nonexistent"),
    ]);
    let id = s.put_provider_record(&r).await.unwrap();
    assert_eq!(
        s.provider_record(&id).await.unwrap().season_artwork,
        r.season_artwork
    );
    s.assign_metadata(&copy.id, Some(&id)).await.unwrap();
    let parent = s.enrichment_input(&copy.id).await.unwrap().library_item_id;
    let page = s
        .library_children(&lib, &parent, 0, 1, &ChildFilter::default())
        .await
        .unwrap();
    assert_eq!(page.groups.len(), 2);
    assert_eq!(
        page.groups[1]
            .metadata
            .as_ref()
            .unwrap()
            .description
            .artwork
            .as_ref()
            .unwrap(),
        &["second"]
    );
    let resolved = s
        .library_season_artwork(&lib, &parent, Some(0))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolved.metadata.provenance["artwork"], id);
    assert_eq!(resolved.representative_id, copy.id);
    assert!(
        s.library_season_artwork(&lib, &parent, Some(99))
            .await
            .is_err()
    );
    r.season_artwork = Some(vec![]);
    s.put_provider_record(&r).await.unwrap();
    assert!(
        s.library_season_artwork(&lib, &parent, Some(2))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn missing_season_falls_through_and_all_provider_covers_are_retained() {
    let (_dir, s) = store().await;
    let col = collection(
        &s,
        "shows",
        MediaType::Series,
        &["Show (2000)/Show.S01E01.mkv", "Show (2000)/Show.S02E01.mkv"],
    )
    .await;
    let lib = s
        .create_library("Shows", MediaType::Series, std::slice::from_ref(&col))
        .await
        .unwrap();
    let copy = s.collection_items(&col).await.unwrap().remove(0);
    let mut a = record("tmdb", "1", "Show", Some(2000), MediaType::Series);
    a.season_artwork = Some(vec![cover(
        SeasonArtworkScope::Season { number: 1 },
        "tmdb-first",
    )]);
    let mut b = record("tvdb", "2", "Show", Some(2000), MediaType::Series);
    b.season_artwork = Some(vec![
        cover(SeasonArtworkScope::Season { number: 1 }, "tvdb-first"),
        cover(SeasonArtworkScope::Season { number: 2 }, "tvdb-second"),
    ]);
    let primary = s.put_provider_record(&a).await.unwrap();
    let supplement = s.put_provider_record(&b).await.unwrap();
    s.assign_metadata(&copy.id, Some(&primary)).await.unwrap();
    s.set_supplements(&copy.id, std::slice::from_ref(&supplement))
        .await
        .unwrap();
    let parent = s.enrichment_input(&copy.id).await.unwrap().library_item_id;
    let one = s
        .library_season_artwork(&lib, &parent, Some(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(one.metadata.provenance["artwork"], primary);
    let two = s
        .library_season_artwork(&lib, &parent, Some(2))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(two.metadata.provenance["artwork"], supplement);
    assert_eq!(
        s.provider_artwork(&copy.id, "tvdb").await.unwrap(),
        vec!["tvdb-first", "tvdb-second"]
    );
    assert!(
        s.library_season_artwork("hidden", &parent, Some(1))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn native_entry_cover_requires_one_unambiguous_group() {
    let (_dir, s) = store().await;
    let col = collection(
        &s,
        "anime",
        MediaType::Anime,
        &["Show (2000)/Show.S01E01.mkv", "Show (2000)/Show.S02E01.mkv"],
    )
    .await;
    let lib = s
        .create_library("Anime", MediaType::Anime, std::slice::from_ref(&col))
        .await
        .unwrap();
    let copy = s.collection_items(&col).await.unwrap().remove(0);
    let mut r = record("anilist", "1", "Show", Some(2000), MediaType::Anime);
    r.namespace = "anime".into();
    r.season_artwork = Some(vec![cover(SeasonArtworkScope::NativeEntry, "entry")]);
    let id = s.put_provider_record(&r).await.unwrap();
    s.assign_metadata(&copy.id, Some(&id)).await.unwrap();
    let parent = s.enrichment_input(&copy.id).await.unwrap().library_item_id;
    assert!(
        s.library_season_artwork(&lib, &parent, Some(1))
            .await
            .unwrap()
            .is_none()
    );
    let single = collection(
        &s,
        "single",
        MediaType::Anime,
        &["Show (2000)/Show.S01E01.mkv"],
    )
    .await;
    let lib2 = s
        .create_library("Single", MediaType::Anime, std::slice::from_ref(&single))
        .await
        .unwrap();
    let copy2 = s.collection_items(&single).await.unwrap().remove(0);
    s.assign_metadata(&copy2.id, Some(&id)).await.unwrap();
    assert!(
        s.library_season_artwork(&lib2, &parent, Some(1))
            .await
            .unwrap()
            .is_some()
    );
}
