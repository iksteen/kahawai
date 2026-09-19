mod common;
use common::*;
use kahawai_mediadb::*;

async fn fixture() -> (tempfile::TempDir, Store, String, String) {
    let (dir, store) = store().await;
    let collection = collection(
        &store,
        "films",
        MediaType::Movies,
        &["The Matrix (1999).mkv"],
    )
    .await;
    let item = store.collection_items(&collection).await.unwrap()[0]
        .id
        .clone();
    (dir, store, collection, item)
}
fn answer(provider: &str, strength: i32, title: &str) -> EnrichmentAnswer {
    EnrichmentAnswer {
        candidates: vec![EnrichmentCandidate {
            complete: true,
            record: record(provider, title, title, Some(1999), MediaType::Movies),
            strength,
            links: vec![],
        }],
        local: None,
        ..Default::default()
    }
}
#[tokio::test]
async fn every_failed_provider_releases_its_claim_and_cannot_block_other_lanes() {
    for failed in [
        "tmdb",
        "tvdb",
        "local",
        "anidb",
        "anidb-hash",
        "anilist",
        "anime-mappings",
        "musicbrainz",
        "fanart",
        "theaudiodb",
        "tmdb-artwork",
        "tvdb-artwork",
        "anilist-artwork",
        "local-artwork",
        "coverartarchive",
        "artist-collage",
    ] {
        for blocked in [false, true] {
            let (_dir, store) = store().await;
            let col = collection(
                &store,
                "films",
                MediaType::Movies,
                &["The Matrix (1999).mkv", "Dark City (1998).mkv"],
            )
            .await;
            let mut order = vec!["tmdb".into(), "tvdb".into()];
            if !order.iter().any(|p| p == failed) {
                order.push(failed.into());
            }
            store
                .set_provider_order(MediaType::Movies, &order)
                .await
                .unwrap();
            assert!(
                store
                    .claim_enrichment(failed, 100, 30)
                    .await
                    .unwrap()
                    .is_none()
            );
            store
                .create_library("Movies", MediaType::Movies, &[col])
                .await
                .unwrap();
            let job = store
                .claim_enrichment(failed, 100, 30)
                .await
                .unwrap()
                .unwrap();
            store
                .fail_enrichment(&job, 10000, "provider keeps refusing", true, blocked)
                .await
                .unwrap();
            let deferred = store
                .claim_enrichment(failed, 200, 30)
                .await
                .unwrap()
                .unwrap();
            assert_ne!(deferred.item_id, job.item_id);
            store.defer_enrichment(&deferred).await.unwrap();
            // A cache miss inherits the pause without replacing its cause.
            assert_eq!(
                store.provider_pause(failed).await.unwrap(),
                Some((10000, blocked))
            );
            // Duplicate completion is harmless, including its attempt count.
            store.defer_enrichment(&deferred).await.unwrap();
            assert!(
                store
                    .claim_enrichment(failed, 9999, 30)
                    .await
                    .unwrap()
                    .is_none()
            );
            if blocked {
                assert!(
                    store
                        .claim_enrichment(failed, 10000, 30)
                        .await
                        .unwrap()
                        .is_none(),
                    "credential blocks do not expire"
                );
            } else {
                for _ in 0..2 {
                    let resumed = store
                        .claim_enrichment(failed, 10000, 30)
                        .await
                        .unwrap()
                        .expect("timed pause expires at its deadline");
                    assert_eq!(
                        resumed.attempts,
                        if resumed.item_id == deferred.item_id {
                            1
                        } else {
                            2
                        },
                        "paused polls are not attempts"
                    );
                    store
                        .finish_enrichment(&resumed, &EnrichmentAnswer::default())
                        .await
                        .unwrap();
                }
            }
            let other = if failed == "tmdb" { "tvdb" } else { "tmdb" };
            let ready = store
                .claim_enrichment(other, 200, 30)
                .await
                .unwrap()
                .unwrap();
            assert!(
                store
                    .finish_enrichment(&ready, &EnrichmentAnswer::default())
                    .await
                    .unwrap()
            );
            let status = store.enrichment_status().await.unwrap();
            assert!(
                status
                    .iter()
                    .any(|s| s.provider == other && s.state == "done")
            );
            assert!(
                !status
                    .iter()
                    .any(|s| s.provider == failed && s.state == "running")
            );
        }
    }
}
#[tokio::test]
async fn weak_review_manual_pin_and_late_response_preserve_stable_identity() {
    let (_dir, store, col, item) = fixture().await;
    let library = store
        .create_library("Movies", MediaType::Movies, std::slice::from_ref(&col))
        .await
        .unwrap();
    let original = store.collection_items(&col).await.unwrap()[0]
        .library_item_id
        .clone();
    assert_eq!(
        store
            .library_item(&library, &original)
            .await
            .unwrap()
            .match_confidence,
        None
    );
    let job = store
        .claim_enrichment("tmdb", 1, 30)
        .await
        .unwrap()
        .unwrap();
    store
        .finish_enrichment(&job, &answer("tmdb", 0, "Dark City"))
        .await
        .unwrap();
    assert!(
        store
            .enrichment_input(&item)
            .await
            .unwrap()
            .selected
            .is_none()
    );
    assert_eq!(
        store.collection_items(&col).await.unwrap()[0].library_item_id,
        original
    );
    assert_eq!(
        store
            .library_item(&library, &original)
            .await
            .unwrap()
            .match_confidence
            .as_deref(),
        Some("weak")
    );
    let candidate = store.review_candidates(&item).await.unwrap().remove(0);
    let late = store
        .claim_enrichment("tvdb", 1, 30)
        .await
        .unwrap()
        .unwrap();
    let revision = store.enrichment_input(&item).await.unwrap().revision;
    store
        .correct_metadata(&item, revision, "pick", Some(&candidate.id))
        .await
        .unwrap();
    assert!(
        !store
            .finish_enrichment(&late, &answer("tvdb", 20, "The Matrix"))
            .await
            .unwrap()
    );
    let current = store.enrichment_input(&item).await.unwrap();
    assert!(current.manual);
    assert_eq!(
        store
            .library_item(&library, &current.library_item_id)
            .await
            .unwrap()
            .match_confidence
            .as_deref(),
        Some("manual")
    );
    assert_eq!(current.selected.unwrap().1.title, "Dark City");
    assert_ne!(
        store.collection_items(&col).await.unwrap()[0].library_item_id,
        original
    );
    assert!(store.library_item_record(&original).await.unwrap().archived);
    store
        .set_provider_order(MediaType::Movies, &["tvdb".into(), "tmdb".into()])
        .await
        .unwrap();
    assert_eq!(
        store
            .enrichment_input(&item)
            .await
            .unwrap()
            .selected
            .unwrap()
            .1
            .title,
        "Dark City"
    );
    assert!(
        store
            .correct_metadata(&item, revision, "clear", None)
            .await
            .unwrap_err()
            .is::<StaleEnrichment>()
    );
}
#[tokio::test]
async fn expired_claim_reopens_after_restart_and_membership_removal_prevents_completion() {
    let (dir, store, col, item) = fixture().await;
    let library = store
        .create_library("Movies", MediaType::Movies, &[col])
        .await
        .unwrap();
    let first = store
        .claim_enrichment("tmdb", 10, 20)
        .await
        .unwrap()
        .unwrap();
    store.close().await;
    let store = Store::open(&dir.path().join("mediadb.db")).await.unwrap();
    assert!(
        store
            .claim_enrichment("tmdb", 29, 20)
            .await
            .unwrap()
            .is_none()
    );
    let second = store
        .claim_enrichment("tmdb", 30, 20)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(first.token, second.token);
    assert!(
        !store
            .finish_enrichment(&first, &answer("tmdb", 10, "The Matrix"))
            .await
            .unwrap()
    );
    store.remove_library(&library).await.unwrap();
    assert!(
        !store
            .finish_enrichment(&second, &answer("tmdb", 10, "The Matrix"))
            .await
            .unwrap()
    );
    assert!(
        store
            .enrichment_input(&item)
            .await
            .unwrap()
            .selected
            .is_none()
    );
}
#[tokio::test]
async fn provider_order_uses_answers_not_response_races_and_failure_releases_fallback() {
    let (_dir, store, col, item) = fixture().await;
    store
        .create_library("Movies", MediaType::Movies, &[col])
        .await
        .unwrap();
    let second = store
        .claim_enrichment("tvdb", 1, 30)
        .await
        .unwrap()
        .unwrap();
    store
        .finish_enrichment(&second, &answer("tvdb", 10, "Dark City"))
        .await
        .unwrap();
    assert!(
        store
            .enrichment_input(&item)
            .await
            .unwrap()
            .selected
            .is_none()
    );
    let first = store
        .claim_enrichment("tmdb", 1, 30)
        .await
        .unwrap()
        .unwrap();
    store
        .fail_enrichment(
            &first,
            i64::MAX / 2,
            "continuously unavailable",
            true,
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .enrichment_input(&item)
            .await
            .unwrap()
            .selected
            .unwrap()
            .1
            .title,
        "Dark City"
    );
    store
        .set_provider_order(MediaType::Movies, &["tvdb".into(), "tmdb".into()])
        .await
        .unwrap();
    assert_eq!(
        store
            .enrichment_input(&item)
            .await
            .unwrap()
            .selected
            .unwrap()
            .1
            .title,
        "Dark City"
    );
}

#[tokio::test]
async fn parked_provider_still_allows_another_copy_to_finish_from_a_retained_answer() {
    let (_dir, store, col, _) = fixture().await;
    store
        .create_library("Movies", MediaType::Movies, &[col])
        .await
        .unwrap();
    let failed = store
        .claim_enrichment("tmdb", 1, 30)
        .await
        .unwrap()
        .unwrap();
    let cached = answer("tmdb", 10, "The Matrix");
    store
        .put_cache_answer(
            "tmdb",
            "shared-question",
            &serde_json::to_string(&cached).unwrap(),
            1,
        )
        .await
        .unwrap();
    store
        .fail_enrichment(&failed, i64::MAX, "rate limited", true, false)
        .await
        .unwrap();
    let second = collection(
        &store,
        "second",
        MediaType::Movies,
        &["The Matrix (1999).mkv"],
    )
    .await;
    store
        .create_library("Second", MediaType::Movies, &[second])
        .await
        .unwrap();
    let job = store
        .claim_enrichment("tmdb", 2, 30)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(job.item_id, failed.item_id);
    let retained: EnrichmentAnswer = serde_json::from_str(
        &store
            .cache_answer("tmdb", "shared-question")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(store.finish_enrichment(&job, &retained).await.unwrap());
    assert!(
        store
            .enrichment_input(&job.item_id)
            .await
            .unwrap()
            .selected
            .is_some()
    );
    assert_eq!(
        store.provider_pause("tmdb").await.unwrap(),
        Some((i64::MAX, false))
    );
}

#[tokio::test]
async fn partial_search_does_not_erase_details_and_retry_preserves_rejections() {
    let (_dir, store, col, item) = fixture().await;
    store
        .create_library("Movies", MediaType::Movies, &[col])
        .await
        .unwrap();
    let mut complete = answer("tmdb", 0, "Dark City");
    complete.candidates[0].record.description.overview = Some("Full provider detail".into());
    let job = store
        .claim_enrichment("tmdb", 1, 30)
        .await
        .unwrap()
        .unwrap();
    store.finish_enrichment(&job, &complete).await.unwrap();
    let id = store.review_candidates(&item).await.unwrap()[0].id.clone();
    let rev = store.enrichment_input(&item).await.unwrap().revision;
    store
        .correct_metadata(&item, rev, "retry", None)
        .await
        .unwrap();
    let job = store
        .claim_enrichment("tmdb", 2, 30)
        .await
        .unwrap()
        .unwrap();
    assert!(job.force);
    let mut partial = answer("tmdb", 0, "Dark City");
    partial.candidates[0].complete = false;
    store.finish_enrichment(&job, &partial).await.unwrap();
    assert_eq!(
        store.review_candidates(&item).await.unwrap()[0]
            .record
            .description
            .overview
            .as_deref(),
        Some("Full provider detail")
    );
    let rev = store.enrichment_input(&item).await.unwrap().revision;
    store
        .correct_metadata(&item, rev, "reject", Some(&id))
        .await
        .unwrap();
    let job = store
        .claim_enrichment("tmdb", 3, 30)
        .await
        .unwrap()
        .unwrap();
    complete.candidates[0].strength = 20;
    store.finish_enrichment(&job, &complete).await.unwrap();
    assert!(
        store
            .enrichment_input(&item)
            .await
            .unwrap()
            .selected
            .is_none()
    );
    assert!(store.review_candidates(&item).await.unwrap()[0].rejected);
}

#[tokio::test]
async fn review_filters_and_counts_reflect_weak_answers_and_selected_identity() {
    let (_dir, store, col, item) = fixture().await;
    let library = store
        .create_library("Movies", MediaType::Movies, std::slice::from_ref(&col))
        .await
        .unwrap();
    assert_eq!(
        store
            .enrichment_items(Some(&library), Some(&col), false, "", 0, 50)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .enrichment_items(None, None, true, "", 0, 50)
            .await
            .unwrap()
            .is_empty()
    );
    let job = store
        .claim_enrichment("tmdb", 1, 30)
        .await
        .unwrap()
        .unwrap();
    store
        .finish_enrichment(&job, &answer("tmdb", 0, "Dark City"))
        .await
        .unwrap();
    assert_eq!(
        store
            .enrichment_items(None, None, true, "", 0, 50)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(store.enrichment_counts().await.unwrap(), (0, 1, 0));
    let candidate = store.review_candidates(&item).await.unwrap().remove(0);
    let rev = store.enrichment_input(&item).await.unwrap().revision;
    store
        .correct_metadata(&item, rev, "pick", Some(&candidate.id))
        .await
        .unwrap();
    assert!(
        store
            .enrichment_items(None, None, true, "", 0, 50)
            .await
            .unwrap()
            .is_empty()
    );
    let rows = store
        .enrichment_items(None, None, false, "dark CITY", 0, 50)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].review_needed);
    assert_eq!(rows[0].occurrence, "movie:The Matrix (1999).mkv");
    assert!(!rows[0].root_id.is_empty());
    assert_eq!(
        store
            .enrichment_items(None, None, false, ".mkv", 0, 50)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .enrichment_items(None, None, false, "%", 0, 50)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .enrichment_items(None, None, false, "missing", 0, 50)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .enrichment_items(None, None, false, "matrix", 1, 50)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.enrichment_input(&item).await.unwrap().library_item_id,
        store.collection_items(&col).await.unwrap()[0].library_item_id
    );
    assert_eq!(store.enrichment_counts().await.unwrap(), (1, 0, 0));
    assert!(
        store
            .artist_donors(&item, &library)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn saved_identity_assignment_reuses_stable_ids_and_does_not_change_other_copies() {
    let (_dir, store, col, item) = fixture().await;
    store
        .create_library("Movies", MediaType::Movies, std::slice::from_ref(&col))
        .await
        .unwrap();
    let other = collection(&store, "other", MediaType::Movies, &["Heat (1995).mkv"]).await;
    store
        .create_library(
            "Other movies",
            MediaType::Movies,
            std::slice::from_ref(&other),
        )
        .await
        .unwrap();
    let target = store.collection_items(&other).await.unwrap().remove(0);
    let record = store
        .put_provider_record(&record(
            "tmdb",
            "heat",
            "Heat",
            Some(1995),
            MediaType::Movies,
        ))
        .await
        .unwrap();
    store
        .assign_metadata(&target.id, Some(&record))
        .await
        .unwrap();
    let target_input = store.enrichment_input(&target.id).await.unwrap();
    let before = store.enrichment_input(&item).await.unwrap();
    let choices = store
        .matching_identities(&item, "HEAT", 0, 200)
        .await
        .unwrap();
    assert_eq!(choices.len(), 1);
    assert_eq!(choices[0].id, target_input.library_item_id);
    let alternatives = store.matching_identities(&item, "", 0, 1).await.unwrap();
    assert_eq!(alternatives.len(), 1);
    assert_eq!(alternatives[0].id, target_input.library_item_id);
    store
        .assign_identity(&item, before.revision, &choices[0].id)
        .await
        .unwrap();
    let assigned = store.enrichment_input(&item).await.unwrap();
    assert_eq!(assigned.library_item_id, target_input.library_item_id);
    assert!(
        store
            .matching_identities(&item, "HEAT", 0, 200)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(assigned.selected.as_ref().unwrap().0, record);
    assert!(assigned.manual);
    assert!(
        store
            .assign_identity(&item, before.revision, &choices[0].id)
            .await
            .unwrap_err()
            .is::<StaleEnrichment>()
    );
    assert_eq!(
        store
            .enrichment_input(&target.id)
            .await
            .unwrap()
            .library_item_id,
        target_input.library_item_id
    );
}

#[tokio::test]
async fn manual_matching_keeps_albums_separate_and_rejects_cross_type_assignment() {
    let (_dir, store, _, movie) = fixture().await;
    let col = collection(
        &store,
        "albums",
        MediaType::Music,
        &[
            "Artist/Album (2001)/01 - Song.flac",
            "Artist/Other (2002)/01 - Song.flac",
        ],
    )
    .await;
    store
        .create_library("Music", MediaType::Music, std::slice::from_ref(&col))
        .await
        .unwrap();
    let albums = store.collection_items(&col).await.unwrap();
    assert_eq!(albums.len(), 2);
    let before = store.enrichment_input(&albums[0].id).await.unwrap();
    let target = store.enrichment_input(&albums[1].id).await.unwrap();
    store
        .assign_identity(&before.item_id, before.revision, &target.library_item_id)
        .await
        .unwrap();
    assert_ne!(
        store
            .enrichment_input(&before.item_id)
            .await
            .unwrap()
            .library_item_id,
        target.library_item_id
    );
    let movie = store.enrichment_input(&movie).await.unwrap();
    assert!(
        store
            .assign_identity(&movie.item_id, movie.revision, &target.library_item_id)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .enrichment_input(&movie.item_id)
            .await
            .unwrap()
            .revision,
        movie.revision
    );
}

#[tokio::test]
async fn artist_portrait_uses_other_visible_albums_but_not_hidden_or_stale_identities() {
    let (_dir, store) = store().await;
    let mut copies = Vec::new();
    let mut collections = Vec::new();
    for album in ["Known", "Unknown"] {
        let (col, _) = store
            .offer_collection("host", &offer(album, MediaType::Music, 1))
            .await
            .unwrap();
        store
            .apply_catalogue(
                "host",
                &delta(
                    album,
                    true,
                    true,
                    1,
                    vec![file_media(1, "01.flac", tagged(album, 1, 1))],
                ),
            )
            .await
            .unwrap();
        copies.push(store.collection_items(&col).await.unwrap()[0].id.clone());
        collections.push(col);
    }
    let private = store
        .create_library("Private", MediaType::Music, &collections[..1])
        .await
        .unwrap();
    let job = store
        .claim_enrichment("musicbrainz", 1, 30)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.item_id, copies[0]);
    store
        .finish_enrichment(
            &job,
            &EnrichmentAnswer {
                artist: Some(ArtistIdentity {
                    id: "artist".into(),
                    name: "Album Artist".into(),
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    store
        .put_artist_artwork(
            "artist",
            "fanart",
            Some("https://example.test/portrait.jpg"),
            1,
        )
        .await
        .unwrap();
    let public = store
        .create_library("Public", MediaType::Music, &collections[1..])
        .await
        .unwrap();
    assert!(
        store
            .artist_artwork(&copies[1], &public)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .artist_artwork(&copies[1], &private)
            .await
            .unwrap()
            .is_empty()
    );
    store
        .set_library_collections(&public, &collections)
        .await
        .unwrap();
    assert_eq!(
        store.artist_artwork(&copies[1], &public).await.unwrap(),
        ["https://example.test/portrait.jpg"]
    );
    let revision = store.enrichment_input(&copies[0]).await.unwrap().revision;
    store
        .correct_metadata(&copies[0], revision, "retry", None)
        .await
        .unwrap();
    assert!(
        store
            .artist_artwork(&copies[1], &public)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn corrected_nfo_replaces_only_an_automatic_local_identity() {
    for pinned in [false, true] {
        let (_dir, store, col, item) = fixture().await;
        store
            .create_library("Movies", MediaType::Movies, &[col])
            .await
            .unwrap();
        let mut original = answer("local", 30, "Wrong Film");
        original.local = Some(original.candidates[0].record.clone());
        let job = store
            .claim_enrichment("local", 100, 30)
            .await
            .unwrap()
            .unwrap();
        store.finish_enrichment(&job, &original).await.unwrap();
        let before = store.enrichment_input(&item).await.unwrap();
        if pinned {
            store
                .correct_metadata(
                    &item,
                    before.revision,
                    "confirm",
                    Some(&before.selected.as_ref().unwrap().0),
                )
                .await
                .unwrap();
        }
        let mut correction = answer("local", 30, "Correct Film");
        correction.local = Some(correction.candidates[0].record.clone());
        let job = store
            .claim_enrichment("local", 100, 30)
            .await
            .unwrap()
            .unwrap();
        store.finish_enrichment(&job, &correction).await.unwrap();
        let corrected = store.enrichment_input(&item).await.unwrap();
        assert_eq!(
            corrected.selected.as_ref().unwrap().1.title,
            if pinned { "Wrong Film" } else { "Correct Film" }
        );
        assert_eq!(corrected.manual, pinned);
        assert_eq!(corrected.library_item_id == before.library_item_id, pinned);
        if !pinned {
            // Completion runs once more against the new identity. Replaying the
            // same NFO must settle, rather than perpetually requeue every provider.
            let job = store
                .claim_enrichment("local", 100, 30)
                .await
                .unwrap()
                .unwrap();
            store.finish_enrichment(&job, &correction).await.unwrap();
            assert_eq!(
                store.enrichment_input(&item).await.unwrap().revision,
                corrected.revision
            );
        }
        assert!(
            store
                .claim_enrichment("local", 100, 30)
                .await
                .unwrap()
                .is_none()
        );
    }
}
