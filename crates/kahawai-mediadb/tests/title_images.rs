mod common;
use common::*;
use kahawai_mediadb::*;

#[tokio::test]
async fn image_roles_resolve_independently_with_provenance() {
    let (_dir, s) = store().await;
    let col = collection(&s, "movies", MediaType::Movies, &["Movie (2000)/Movie.mkv"]).await;
    let lib = s
        .create_library("Movies", MediaType::Movies, std::slice::from_ref(&col))
        .await
        .unwrap();
    let copy = s.collection_items(&col).await.unwrap().remove(0);
    let mut primary = record("tmdb", "1", "Movie", Some(2000), MediaType::Movies);
    primary.description.logo = Some("primary-logo".into());
    let primary_id = s.put_provider_record(&primary).await.unwrap();
    s.assign_metadata(&copy.id, Some(&primary_id))
        .await
        .unwrap();
    let mut extra = record("tvdb", "2", "Movie", Some(2000), MediaType::Movies);
    extra.description.logo = Some("other-logo".into());
    extra.description.backdrop = Some("background".into());
    extra.description.banner = Some("banner".into());
    let extra_id = s.put_provider_record(&extra).await.unwrap();
    s.set_provider_order(MediaType::Movies, &["tmdb".into(), "tvdb".into()])
        .await
        .unwrap();
    s.set_supplements(&copy.id, std::slice::from_ref(&extra_id))
        .await
        .unwrap();
    let resolved = s
        .library_item(
            &lib,
            &s.enrichment_input(&copy.id).await.unwrap().library_item_id,
        )
        .await
        .unwrap()
        .metadata;
    assert_eq!(resolved.description.logo.as_deref(), Some("primary-logo"));
    assert_eq!(resolved.description.backdrop.as_deref(), Some("background"));
    assert_eq!(resolved.provenance["logo"], primary_id);
    assert_eq!(resolved.provenance["banner"], extra_id);
    assert_eq!(
        s.provider_artwork(&copy.id, "tvdb").await.unwrap(),
        ["background", "banner", "other-logo"]
    );
}

#[tokio::test]
async fn prefetch_collects_headshots_and_only_source_backed_episode_stills() {
    let (_dir, s) = store().await;
    let col = collection(&s, "shows", MediaType::Series, &["Show/Show.S01E02.mkv"]).await;
    s.create_library("Shows", MediaType::Series, std::slice::from_ref(&col))
        .await
        .unwrap();
    let copy = s.collection_items(&col).await.unwrap().remove(0);
    let mut record = record("tmdb", "1", "Show", None, MediaType::Series);
    record.description.logo = Some("logo".into());
    record.description.cast = Some(vec![
        Credit {
            name: "Actor".into(),
            id: Some("tmdb:10".into()),
            image: Some("headshot".into()),
            ..Default::default()
        },
        Credit {
            name: "Same Actor".into(),
            id: Some("tmdb:10".into()),
            image: Some("headshot".into()),
            ..Default::default()
        },
    ]);
    record.children = Some(
        (1..=3)
            .map(|number| ProviderChild {
                title: format!("Episode {number}"),
                position: ProviderChildPosition {
                    season: Some(1),
                    episode: Some(number),
                    ..Default::default()
                },
                description: Description {
                    artwork: Some(vec![format!("/still{number}.jpg")]),
                    ..Default::default()
                },
                ..Default::default()
            })
            .collect(),
    );
    let id = s.put_provider_record(&record).await.unwrap();
    s.assign_metadata(&copy.id, Some(&id)).await.unwrap();
    assert_eq!(
        s.provider_artwork(&copy.id, "tmdb").await.unwrap(),
        [
            "headshot",
            "https://image.tmdb.org/t/p/w780/still2.jpg",
            "logo"
        ]
    );
}

#[tokio::test]
async fn movie_fanart_requires_a_verified_tmdb_identity_and_cannot_change_metadata_identity() {
    for verified in [false, true] {
        let (_dir, s) = store().await;
        let col = collection(&s, "movies", MediaType::Movies, &["Movie (2000)/Movie.mkv"]).await;
        let library = s
            .create_library("Movies", MediaType::Movies, std::slice::from_ref(&col))
            .await
            .unwrap();
        let copy = s.collection_items(&col).await.unwrap().remove(0);
        let mut primary = record("tmdb", "1", "Movie", Some(2000), MediaType::Movies);
        primary.namespace = "movie".into();
        let primary_id = s.put_provider_record(&primary).await.unwrap();
        s.assign_metadata(&copy.id, Some(&primary_id))
            .await
            .unwrap();
        let mut art = record(
            "fanart",
            if verified { "1" } else { "999" },
            "Movie",
            Some(2000),
            MediaType::Movies,
        );
        art.namespace = "movie".into();
        art.description.logo = Some("logo".into());
        let job = s.claim_enrichment("fanart", 1, 180).await.unwrap().unwrap();
        s.finish_enrichment(
            &job,
            &EnrichmentAnswer {
                artwork_supplement: Some(art),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let item = s
            .library_item(
                &library,
                &s.enrichment_input(&copy.id).await.unwrap().library_item_id,
            )
            .await
            .unwrap();
        assert_eq!(
            item.metadata.description.logo.as_deref(),
            verified.then_some("logo")
        );
        assert_eq!(
            s.enrichment_input(&copy.id)
                .await
                .unwrap()
                .selected
                .unwrap()
                .0,
            primary_id
        );
        assert!(
            s.review_candidates(&copy.id)
                .await
                .unwrap()
                .iter()
                .all(|candidate| candidate.record.provider != "fanart")
        );
    }
}
