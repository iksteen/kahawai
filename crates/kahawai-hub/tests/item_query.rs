//! `QUERY /api/v1/items/{id}` (RFC 10008) — the converged half of the
//! item resource.
//!
//! Three of these guard properties that are invisible at a glance and
//! would fail silently:
//!
//! - the route is reached through `MethodRouter::fallback`, because
//!   axum's `MethodFilter` has no extension methods — and whether the
//!   `require_auth` layer reaches a fallback depends on which of two
//!   near-identically-named axum functions the router uses
//!   (`Router::route_layer` maps it, `MethodRouter::route_layer` does
//!   not). An unauthenticated 200 would be the failure;
//! - that fallback swallows EVERY unmatched method, so axum's own 405
//!   machinery stops running and the `Allow` header is ours to write;
//! - RFC 10008 requires rejecting a missing or inconsistent
//!   `Content-Type`.

use std::sync::Arc;

use kahawai_hub::registry::{FileUpsertRecord, Registry};
use tower::ServiceExt;

const TEST_ROOT: &str = "/kahawai-test-root";

/// Built from the real structs, never hand-written JSON. A literal
/// omitting one required field parses to an EMPTY `MediaInfo` —
/// `parse_info` in the negotiator swallows the error — and every stream
/// then negotiates to "none" while shape-only assertions stay green.
/// That is exactly what the hand-written fixture here did until
/// `the_fixture_declarations_actually_parse` caught it.
fn info(subs: &[(&str, &str)]) -> kahawai_core::media::MediaInfo {
    use kahawai_core::media::{AudioStream, MediaInfo, SubtitleStream, VideoStream};
    MediaInfo {
        container: Some("matroska".into()),
        duration_ms: Some(60_000),
        video: vec![VideoStream {
            codec: "h264".into(),
            width: 1920,
            height: 1080,
            ..Default::default()
        }],
        audio: vec![AudioStream {
            codec: "aac".into(),
            channels: 2,
            sample_rate: 48_000,
            ..Default::default()
        }],
        subtitles: subs
            .iter()
            .map(|(format, lang)| SubtitleStream {
                format: (*format).into(),
                language: Some((*lang).into()),
            })
            .collect(),
        ..Default::default()
    }
}

/// An item whose only subtitle is ASS, so the ladder has something to
/// resolve. The declaration is what negotiation and the track sync both
/// read, so no bytes are needed to pose the question.
fn ass_rec(path: &str) -> FileUpsertRecord {
    FileUpsertRecord {
        root_token: kahawai_core::media::root_token(std::path::Path::new(TEST_ROOT)),
        streams_json: serde_json::to_string(&info(&[("ass", "en")])).unwrap(),
        ..rec(path, 200)
    }
}

fn rec(path: &str, size: u64) -> FileUpsertRecord {
    FileUpsertRecord {
        root_token: kahawai_core::media::root_token(std::path::Path::new(TEST_ROOT)),
        path_rel: path.into(),
        size,
        mtime_unix: 1,
        head_xxh3: 1,
        tail_xxh3: 2,
        oshash: 3,
        streams_json: serde_json::to_string(&info(&[])).unwrap(),
    }
}

/// One part of a work, with its own running time and its own chapters on
/// its own timeline — which is how a container states them.
fn chaptered(path: &str, size: u64, duration_ms: u64, at: &[(u64, &str)]) -> FileUpsertRecord {
    let mut declared = info(&[]);
    declared.duration_ms = Some(duration_ms);
    declared.chapters = Some(
        at.iter()
            .map(|(start_ms, title)| kahawai_core::media::Chapter {
                start_ms: *start_ms,
                end_ms: None,
                title: Some((*title).into()),
            })
            .collect(),
    );
    FileUpsertRecord {
        streams_json: serde_json::to_string(&declared).unwrap(),
        ..rec(path, size)
    }
}

/// The chapter list a seek bar and a detail page draw from, on the
/// item's timeline rather than each file's.
#[tokio::test]
async fn chapters_of_a_two_part_film_run_on_one_timeline() {
    let fx = fixture_with(chaptered(
        "Heat (1995)/Heat (1995) cd1.mkv",
        100,
        60_000,
        &[(0, "Opening"), (30_000, "Part A")],
    ))
    .await;
    fx.reg
        .upsert_files(
            "01H",
            "movies",
            vec![
                chaptered(
                    "Heat (1995)/Heat (1995) cd1.mkv",
                    100,
                    60_000,
                    &[(0, "Opening"), (30_000, "Part A")],
                ),
                chaptered(
                    "Heat (1995)/Heat (1995) cd2.mkv",
                    110,
                    50_000,
                    &[(0, "Part B"), (40_000, "Credits")],
                ),
            ],
        )
        .await
        .unwrap();

    let response = fx
        .api
        .clone()
        .oneshot(
            axum::http::Request::get(format!("/api/v1/items/{}", fx.id))
                .header("authorization", &fx.bearer)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let item: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let listed: Vec<(u64, String)> = item["chapters"]
        .as_array()
        .expect("chapters on the detail")
        .iter()
        .map(|c| {
            (
                c["start_ms"].as_u64().unwrap(),
                c["title"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    // The second CD's own clock starts at zero; the viewer's does not.
    assert_eq!(
        listed,
        [
            (0, "Opening".to_string()),
            (30_000, "Part A".into()),
            (60_000, "Part B".into()),
            (100_000, "Credits".into()),
        ]
    );
}

/// Alternative encodes are alternatives, and only one of them is playing.
#[tokio::test]
async fn chapters_come_from_the_source_playback_would_pick() {
    let fx = fixture_with(chaptered(
        "Heat (1995) cd1.mkv",
        100,
        60_000,
        &[(0, "Split")],
    ))
    .await;
    fx.reg
        .upsert_files(
            "01H",
            "movies",
            vec![
                chaptered("Heat (1995) cd1.mkv", 100, 60_000, &[(0, "Split")]),
                chaptered("Heat (1995) REPACK.mkv", 400, 110_000, &[(0, "Whole")]),
            ],
        )
        .await
        .unwrap();

    let response = fx
        .api
        .clone()
        .oneshot(
            axum::http::Request::get(format!("/api/v1/items/{}", fx.id))
                .header("authorization", &fx.bearer)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let item: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let first_source = item["sources"][0]["path_rel"].as_str().unwrap().to_string();
    let titles: Vec<&str> = item["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles,
        [if first_source.contains("REPACK") {
            "Whole"
        } else {
            "Split"
        }],
        "the chapters must belong to the source listed first"
    );
}

/// A chaptered rendition with a stated codec and height, for the tests where
/// which FILE supplies the chapters is the whole question.
fn rendition(path: &str, size: u64, codec: &str, height: u32, chapter: &str) -> FileUpsertRecord {
    let mut declared = info(&[]);
    declared.video[0].codec = codec.into();
    declared.video[0].height = height;
    declared.chapters = Some(vec![kahawai_core::media::Chapter {
        start_ms: 30_000,
        end_ms: None,
        title: Some(chapter.into()),
    }]);
    FileUpsertRecord {
        streams_json: serde_json::to_string(&declared).unwrap(),
        ..rec(path, size)
    }
}

/// An incomplete part set folds wrong offsets, so it supplies nothing.
#[tokio::test]
async fn a_part_set_missing_its_first_cd_supplies_no_chapters() {
    let fx = fixture_with(chaptered(
        "Heat (1995)/Heat (1995) cd1.mkv",
        100,
        60_000,
        &[(0, "Opening"), (30_000, "Part A")],
    ))
    .await;
    fx.reg
        .upsert_files(
            "01H",
            "movies",
            vec![
                chaptered(
                    "Heat (1995)/Heat (1995) cd1.mkv",
                    100,
                    60_000,
                    &[(0, "Opening")],
                ),
                chaptered(
                    "Heat (1995)/Heat (1995) cd2.mkv",
                    110,
                    50_000,
                    &[(0, "Part B"), (40_000, "Credits")],
                ),
            ],
        )
        .await
        .unwrap();
    // cd1 vanishes: the set is cd2-only, and cd2's chapters folded from
    // offset zero would be an hour early.
    sqlx::query(
        "DELETE FROM playable_source_parts WHERE file_id IN
           (SELECT id FROM files WHERE path_rel LIKE '%cd1%')",
    )
    .execute(&fx.db)
    .await
    .unwrap();

    let response = fx
        .api
        .clone()
        .oneshot(
            axum::http::Request::get(format!("/api/v1/items/{}", fx.id))
                .header("authorization", &fx.bearer)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let item: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        item["chapters"].is_null(),
        "an incomplete set must not supply chapters: {}",
        item["chapters"]
    );

    // End-to-end on QUERY too: an incomplete-only item answers null there
    // as well. (This travels item_body's guard — negotiation never picks
    // the incomplete set, so the override's own completeness check is
    // belt-and-braces for the duplicate-path-across-roots case and has no
    // reachable fixture; proven by mutating it to `true` under this test.)
    let resp = fx
        .api
        .clone()
        .oneshot(query(
            &format!("/api/v1/items/{}", fx.id),
            Some(&fx.bearer),
            Some("application/json"),
            "{}",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let j = json_of(resp).await;
    assert!(
        j["chapters"].is_null(),
        "QUERY must not fold an incomplete set either: {}",
        j["chapters"]
    );
}

/// An item whose only source is offline still shows last week's chapters —
/// the same stance `segments` takes — because there is nothing to play at
/// all, so the list cannot describe the wrong file.
#[tokio::test]
async fn an_offline_sole_source_still_lists_its_chapters() {
    let fx = fixture_with(chaptered(
        "Heat (1995).mkv",
        100,
        60_000,
        &[(30_000, "Act 2")],
    ))
    .await;
    fx.reg.disconnected("01H");

    let response = fx
        .api
        .clone()
        .oneshot(
            axum::http::Request::get(format!("/api/v1/items/{}", fx.id))
                .header("authorization", &fx.bearer)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let item: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(item["chapters"][0]["title"], "Act 2");
}

/// QUERY's ticks must describe the file the client will PLAY. Ranking says
/// 4K HEVC; an h264-only client negotiates the 1080p — and the chapters
/// follow the negotiation, not the rank.
#[tokio::test]
async fn query_chapters_and_force_follow_the_cheapest_source() {
    let fx = fixture_with(rendition(
        "Heat (1995).mkv",
        400,
        "hevc",
        2160,
        "From the 4K",
    ))
    .await;
    fx.reg
        .upsert_files(
            "01H",
            "movies",
            vec![
                rendition("Heat (1995).mkv", 400, "hevc", 2160, "From the 4K"),
                rendition(
                    "Heat (1995) [1080p].mkv",
                    100,
                    "h264",
                    1080,
                    "From the 1080p",
                ),
            ],
        )
        .await
        .unwrap();

    let profile = r#"{"profile":{"containers":["mp4"],
        "video":[{"codec":"h264"}],"audio":["aac"],
        "hdr":false,"graphics_overlay":false,"ass_render":false,
        "target_duration":{"mode":"ignore"}}}"#;
    let resp = fx
        .api
        .clone()
        .oneshot(query(
            &format!("/api/v1/items/{}", fx.id),
            Some(&fx.bearer),
            Some("application/json"),
            profile,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let j = json_of(resp).await;
    // Rank (height DESC) lists the 4K first…
    assert!(
        j["sources"][0]["path_rel"]
            .as_str()
            .unwrap()
            .contains("Heat (1995).mkv")
    );
    // …but an h264-only client plays the 1080p, and the ticks say so.
    assert_eq!(
        j["negotiated"]["source"]["path_rel"], "Heat (1995) [1080p].mkv",
        "premise: negotiation picked the cheaper file"
    );
    assert_eq!(j["chapters"][0]["title"], "From the 1080p");

    // Force is audio-bounded. Give only the HEVC source a measurement and
    // make video encoding available: it still must not outrank the ordinary
    // direct-play H.264 source.
    let user_id: String = sqlx::query_scalar("SELECT id FROM users WHERE username='admin'")
        .fetch_one(&fx.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO user_prefs(user_id,scope,key,value)
         VALUES(?,'','loudness_normalization','force')",
    )
    .bind(user_id)
    .execute(&fx.db)
    .await
    .unwrap();
    let hevc_file: i64 =
        sqlx::query_scalar("SELECT id FROM files WHERE path_rel='Heat (1995).mkv'")
            .fetch_one(&fx.db)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO audio_loudness
           (file_id,stream_index,analyzer,size,mtime_unix,integrated_lufs,
            true_peak_dbtp,source_channels,source_channel_mask,
            native_integrated_lufs,native_true_peak_dbtp,error,measured_at)
         VALUES(?,0,?,400,1,-24,-8,2,3,-24,-8,'',unixepoch())",
    )
    .bind(hevc_file)
    .bind(kahawai_media::loudness::ANALYZER)
    .execute(&fx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO audio_loudness_layouts
           (file_id,stream_index,channels,channel_mask,integrated_lufs,true_peak_dbtp)
         VALUES(?,0,2,3,-24,-8)",
    )
    .bind(hevc_file)
    .execute(&fx.db)
    .await
    .unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    std::mem::forget(rx);
    fx.reg.connected("tc", "transcoder", "tc", "fp", "test");
    fx.reg
        .register_tc_link("tc", kahawai_proto::PROTOCOL_MINOR, tx);
    fx.reg.set_transcoder_caps(
        "tc",
        &kahawai_proto::v1::CapabilityReport {
            encoders: vec![
                kahawai_proto::v1::EncoderCap {
                    codec: "h264".into(),
                    element: "x264enc".into(),
                    hardware: false,
                    speed_1080: Some(2.0),
                    speed_2160: Some(0.5),
                },
                kahawai_proto::v1::EncoderCap {
                    codec: "aac".into(),
                    element: "avenc_aac".into(),
                    hardware: false,
                    speed_1080: None,
                    speed_2160: None,
                },
            ],
            max_sessions: 1,
            decode_caps: vec!["video/x-h265".into(), "audio/mpeg".into()],
            ..Default::default()
        },
    );
    let forced = fx
        .api
        .clone()
        .oneshot(query(
            &format!("/api/v1/items/{}", fx.id),
            Some(&fx.bearer),
            Some("application/json"),
            profile,
        ))
        .await
        .unwrap();
    assert_eq!(forced.status(), 200);
    let forced = json_of(forced).await;
    assert_eq!(
        forced["negotiated"]["source"]["path_rel"], "Heat (1995) [1080p].mkv",
        "force normalization promoted a measured video transcode over ordinary direct play"
    );
}

fn test_router_with(
    registry: Arc<Registry>,
    auth: Arc<kahawai_hub::auth::Auth>,
    sessions: Arc<kahawai_hub::sessions::Sessions>,
    subs_dir: std::path::PathBuf,
    net: kahawai_hub::api::NetOptions,
) -> axum::Router {
    let ca = Arc::new(
        kahawai_hub::pki::HubCa::load_or_create(tempfile::tempdir().unwrap().keep().as_path())
            .unwrap(),
    );
    let enrollments = Arc::new(kahawai_hub::enrollment_service::EnrollmentService::new(
        ca,
        registry.clone(),
        std::time::Duration::from_secs(900),
        90,
    ));
    kahawai_hub::api::router(
        registry,
        auth,
        sessions,
        enrollments,
        Arc::new(kahawai_hub::subtitles::Subtitles::new(subs_dir)),
        Arc::new(kahawai_hub::artwork::Artwork::new(
            tempfile::tempdir().unwrap().keep(),
            Arc::new(kahawai_hub::enrich::Enricher::new(
                tempfile::tempdir().unwrap().keep(),
            )),
        )),
        Arc::new(kahawai_hub::enrich::Enricher::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        Arc::new(kahawai_hub::segments::Detector::new()),
        net,
    )
}

/// `db` and `subs_dir` come back so a test can look for the artefacts
/// QUERY must NOT have produced.
struct Fx {
    reg: Arc<Registry>,
    // Keep this bound for the whole test. Dropping it unlinks hub.db while
    // the router's lazy SQLite pool is still using the directory.
    _dir: tempfile::TempDir,
    api: axum::Router,
    bearer: String,
    id: String,
    db: sqlx::SqlitePool,
    subs_dir: std::path::PathBuf,
}

async fn fixture() -> Fx {
    fixture_with(rec("Heat (1995).mkv", 100)).await
}

/// UI-27: one film in several parts must be tellable from several encodes.
///
/// The list is one row per FILE, ordered by what playback would pick, so both
/// cases read as "N sources" in an order that means nothing. The grouping was
/// in the database — `playable_sources` collects a family and states how many
/// parts it expects — and simply was not in the response, so no client could
/// say what it was looking at.
#[tokio::test]
async fn a_multi_part_film_is_tellable_from_alternative_encodes() {
    // Two CDs of one film, and a second encode of the same film beside them.
    let fx = fixture_with(rec("Heat (1995)/Heat (1995) cd1.mkv", 100)).await;
    fx.reg
        .upsert_files(
            "01H",
            "movies",
            vec![
                rec("Heat (1995)/Heat (1995) cd1.mkv", 100),
                rec("Heat (1995)/Heat (1995) cd2.mkv", 110),
                rec("Heat (1995)/Heat (1995) REPACK.mkv", 400),
            ],
        )
        .await
        .unwrap();

    let response = fx
        .api
        .clone()
        .oneshot(
            axum::http::Request::get(format!("/api/v1/items/{}", fx.id))
                .header("authorization", &fx.bearer)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let item: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // `ItemRow<S>`'s generic field: a count when browsing, the rows themselves
    // on a detail.
    let sources = item["sources"].as_array().expect("sources on the detail");

    // Group the way a client would have to, on the fields the response now
    // carries and did not before.
    let mut by_source: std::collections::BTreeMap<i64, Vec<i64>> = Default::default();
    let mut expected: std::collections::BTreeMap<i64, i64> = Default::default();
    for s in sources {
        let id = s["source_id"].as_i64().expect("source_id on every source");
        by_source
            .entry(id)
            .or_default()
            .push(s["part"].as_i64().expect("part on every source"));
        expected.insert(id, s["parts"].as_i64().expect("parts on every source"));
    }

    let multi: Vec<_> = by_source.values().filter(|p| p.len() > 1).collect();
    assert_eq!(
        multi.len(),
        1,
        "expected exactly one multi-part source in {by_source:?}"
    );
    assert_eq!(*multi[0], vec![1, 2], "parts are not numbered in order");
    assert!(
        by_source.len() > 1,
        "the alternative encode did not come back as its own source: {by_source:?}"
    );
    for (id, parts) in &by_source {
        assert_eq!(
            expected[id],
            parts.len() as i64,
            "source {id} reports {} parts and returned {}",
            expected[id],
            parts.len()
        );
    }
}

/// Protocol 4 moved the cost switch to each mediahost. The hub endpoint is
/// therefore always a harmless wake broadcast and never a hub policy gate.
#[tokio::test]
async fn the_detection_trigger_is_a_mediahost_wake() {
    let net = kahawai_hub::api::NetOptions {
        ..Default::default()
    };
    let fx = fixture_net(rec("Heat (1995).mkv", 100), net).await;
    let response = fx
        .api
        .clone()
        .oneshot(
            axum::http::Request::post("/admin/v1/segments")
                .header("authorization", &fx.bearer)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
}

/// Nothing pending answers immediately with no season named — the shape
/// the web poller's "Every season has been analysed" exit depends on —
/// and still carries the follow/boot pair.
#[tokio::test]
async fn a_trigger_with_nothing_pending_names_no_season() {
    let fx = fixture().await;
    let response = fx
        .api
        .clone()
        .oneshot(
            axum::http::Request::post("/admin/v1/segments")
                .header("authorization", &fx.bearer)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let answer: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(answer["series"].is_null(), "{answer}");
    assert!(
        answer["follow"].is_u64() && answer["boot"].is_u64(),
        "{answer}"
    );
}

/// The id a third-party lookup keys on rides the metadata: the parent
/// show's for an episode — an episode's own provider_id names the
/// EPISODE's record, the wrong namespace for services that key TV on
/// the show — and the item's own otherwise. AniList resolves neither.
#[tokio::test]
async fn the_item_carries_the_id_a_lookup_keys_on() {
    let fx = fixture_with(rec("Heat (1995).mkv", 700_000_000)).await;

    let get = |id: String| {
        let api = fx.api.clone();
        let bearer = fx.bearer.clone();
        async move {
            let response = api
                .oneshot(
                    axum::http::Request::get(format!("/api/v1/items/{id}"))
                        .header("authorization", &bearer)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()
        }
    };

    // A movie answers with its own id.
    sqlx::query(
        "INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence, updated_at)
         VALUES (?, 'tmdb', '949', 'Heat', 'auto', unixepoch())",
    )
    .bind(&fx.id)
    .execute(&fx.db)
    .await
    .unwrap();
    let movie = get(fx.id.clone()).await;
    assert_eq!(movie["metadata"]["tmdb_id"], 949, "{movie}");
    assert!(movie["metadata"]["tvdb_id"].is_null(), "{movie}");

    // An episode answers with its SHOW's id, never its own record's.
    sqlx::query(
        "INSERT INTO items (id, kind, title, norm_title, module_id, collection_id, parent_id, season, episode)
         VALUES ('show1', 'show', 'Lain', 'lain', '01H', 'movies', NULL, NULL, NULL),
                ('ep1', 'episode', 'Weird', 'weird', '01H', 'movies', 'show1', 1, 1)",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence, updated_at)
         VALUES ('show1', 'tmdb', '1403', 'Lain', 'auto', unixepoch()),
                ('ep1', 'tmdb', '63412', 'Weird', 'auto', unixepoch())",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    let episode = get("ep1".into()).await;
    assert_eq!(episode["metadata"]["tmdb_id"], 1403, "{episode}");

    // An AniList identity keys nothing.
    sqlx::query(
        "INSERT INTO items (id, kind, title, norm_title, module_id, collection_id)
         VALUES ('anime1', 'movie', 'Akira', 'akira', '01H', 'movies')",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence, updated_at)
         VALUES ('anime1', 'anilist', '47', 'Akira', 'auto', unixepoch())",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    let anime = get("anime1".into()).await;
    assert!(anime["metadata"]["tmdb_id"].is_null(), "{anime}");
    assert!(anime["metadata"]["tvdb_id"].is_null(), "{anime}");
    // Null because the FIELD says so, not because serialization dropped it:
    // `Value::index` answers Null for an absent key too.
    let fields = anime["metadata"].as_object().unwrap();
    assert!(
        fields.contains_key("tmdb_id") && fields.contains_key("tvdb_id"),
        "{anime}"
    );

    // An episode with its OWN tmdb record under an unresolved show must
    // key nothing — falling back to the episode's id would hand a lookup
    // the wrong namespace, which is the bug this ordering exists to stop.
    sqlx::query(
        "INSERT INTO items (id, kind, title, norm_title, module_id, collection_id, parent_id, season, episode)
         VALUES ('bareshow', 'show', 'Bare', 'bare', '01H', 'movies', NULL, NULL, NULL),
                ('bareep', 'episode', 'One', 'one', '01H', 'movies', 'bareshow', 1, 1)",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence, updated_at)
         VALUES ('bareep', 'tmdb', '777001', 'One', 'auto', unixepoch())",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    let bare = get("bareep".into()).await;
    assert!(bare["metadata"]["tmdb_id"].is_null(), "{bare}");

    // A `.nfo` library elects `local` as the describing provider; the
    // stored tmdb answer must key anyway, and tvdb rides alongside
    // independently of which provider was chosen.
    sqlx::query(
        "INSERT INTO items (id, kind, title, norm_title, module_id, collection_id)
         VALUES ('nfo1', 'movie', 'Kept', 'kept', '01H', 'movies')",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence, updated_at)
         VALUES ('nfo1', 'local', 'movie.nfo', 'Kept', 'auto', unixepoch()),
                ('nfo1', 'tmdb', '603', 'Kept', 'auto', unixepoch()),
                ('nfo1', 'tvdb', '169', 'Kept', 'auto', unixepoch())",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT OR REPLACE INTO item_match (item_id, provider, provider_id, media_type, manual, updated_at)
         VALUES ('nfo1', 'local', 'movie.nfo', 'movies', 0, unixepoch())",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    let nfo = get("nfo1".into()).await;
    assert_eq!(nfo["metadata"]["tmdb_id"], 603, "{nfo}");
    assert_eq!(nfo["metadata"]["tvdb_id"], 169, "{nfo}");

    // A weak guess and a rejected id must not key a lookup: a losing
    // 'weak' answer is the wrong title often enough to hand out another
    // film's boundaries, and a rejected one was rejected by a human.
    sqlx::query(
        "INSERT INTO items (id, kind, title, norm_title, module_id, collection_id)
         VALUES ('weak1', 'movie', 'Guess', 'guess', '01H', 'movies')",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence, updated_at)
         VALUES ('weak1', 'tmdb', '111', 'Guess', 'weak', unixepoch()),
                ('weak1', 'tvdb', '222', 'Guess', 'auto', unixepoch())",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO rejected_matches (item_id, provider, provider_id, rejected_at)
         VALUES ('weak1', 'tvdb', '222', unixepoch())",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    let weak = get("weak1".into()).await;
    // The metadata object itself must exist, or these two pass vacuously.
    assert!(!weak["metadata"]["confidence"].is_null(), "{weak}");
    assert!(
        weak["metadata"]["tmdb_id"].is_null(),
        "weak keys nothing: {weak}"
    );
    assert!(
        weak["metadata"]["tvdb_id"].is_null(),
        "rejected keys nothing: {weak}"
    );

    // Unless a human CONFIRMED the weak guess: confirm writes only
    // manual_match, never the answer row, and a vouched match must key.
    sqlx::query(
        "INSERT OR REPLACE INTO manual_match (item_id, provider, provider_id, pinned_at)
         VALUES ('weak1', 'tmdb', '111', unixepoch())",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    let vouched = get("weak1".into()).await;
    assert_eq!(vouched["metadata"]["tmdb_id"], 111, "{vouched}");
    assert!(
        vouched["metadata"]["tvdb_id"].is_null(),
        "rejection still wins: {vouched}"
    );

    // The provider's curated numbering rides along for the client's
    // TV lookups; the show's row carries none.
    sqlx::query(
        "UPDATE provider_metadata SET proj_season = 2, proj_episode = 1 WHERE item_id = 'ep1'",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    let projected = get("ep1".into()).await;
    assert_eq!(projected["metadata"]["proj_season"], 2, "{projected}");
    assert_eq!(projected["metadata"]["proj_episode"], 1, "{projected}");

    // The projection must come from the SAME provider as the id it rides
    // with: a TVDB numbering under a TMDB id keys another episode. Give the
    // episode a rival tvdb row with a different projection; the tmdb id is
    // served, so the tmdb projection must be too.
    sqlx::query(
        "INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence, updated_at, proj_season, proj_episode)
         VALUES ('ep1', 'tvdb', '55', 'Weird', 'auto', unixepoch(), 4, 9),
                ('show1', 'tvdb', '5000', 'Lain', 'auto', unixepoch(), NULL, NULL)",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    let coherent = get("ep1".into()).await;
    assert_eq!(coherent["metadata"]["tmdb_id"], 1403, "{coherent}");
    assert_eq!(
        coherent["metadata"]["proj_season"], 2,
        "the numbering must match the tmdb id, not the rival tvdb row: {coherent}"
    );
    assert_eq!(coherent["metadata"]["proj_episode"], 1, "{coherent}");

    // The discriminating shape: strip the tmdb projection so ONLY the
    // rival tvdb row carries one. The tvdb numbering must never ride the
    // tmdb id — instead the ids NARROW to the provider the numbering
    // belongs to, so the pair a client sends is coherent and a lookup
    // that could work is not refused.
    sqlx::query(
        "UPDATE provider_metadata SET proj_season = NULL, proj_episode = NULL
         WHERE item_id = 'ep1' AND provider = 'tmdb'",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    let uncurated = get("ep1".into()).await;
    assert!(
        uncurated["metadata"]["tmdb_id"].is_null(),
        "an id with no numbering stands aside for the pair that has one: {uncurated}"
    );
    assert_eq!(uncurated["metadata"]["tvdb_id"], 5000, "{uncurated}");
    assert_eq!(uncurated["metadata"]["proj_season"], 4, "{uncurated}");
    assert_eq!(uncurated["metadata"]["proj_episode"], 9, "{uncurated}");
    // Put it back for the cases below.
    sqlx::query(
        "UPDATE provider_metadata SET proj_season = 2, proj_episode = 1
         WHERE item_id = 'ep1' AND provider = 'tmdb'",
    )
    .execute(&fx.db)
    .await
    .unwrap();

    // A double-episode file carries its span on the detail row — the
    // client's multi-episode guard reads it, so it must survive this path.
    sqlx::query("UPDATE items SET episode_end = 2 WHERE id = 'ep1'")
        .execute(&fx.db)
        .await
        .unwrap();
    let doubled = get("ep1".into()).await;
    assert_eq!(doubled["episode_end"], 2, "{doubled}");

    // A zero-padded provider id parses to a DIFFERENT valid id; the
    // round-trip guard must refuse it rather than key another title.
    sqlx::query(
        "INSERT INTO items (id, kind, title, norm_title, module_id, collection_id)
         VALUES ('padded', 'movie', 'Bond', 'bond', '01H', 'movies')",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence, updated_at)
         VALUES ('padded', 'tmdb', '007', 'Bond', 'auto', unixepoch())",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    let padded = get("padded".into()).await;
    assert!(padded["metadata"]["tmdb_id"].is_null(), "{padded}");

    // Zero and negatives round-trip cleanly; only the positivity guard
    // refuses them, and 0 or -1 handed to a lookup is a real key there.
    sqlx::query(
        "INSERT INTO items (id, kind, title, norm_title, module_id, collection_id)
         VALUES ('zero', 'movie', 'Zero', 'zero', '01H', 'movies'),
                ('neg', 'movie', 'Neg', 'neg', '01H', 'movies')",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence, updated_at)
         VALUES ('zero', 'tmdb', '0', 'Zero', 'auto', unixepoch()),
                ('neg', 'tmdb', '-1', 'Neg', 'auto', unixepoch())",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    for id in ["zero", "neg"] {
        let bad = get(id.into()).await;
        assert!(bad["metadata"]["tmdb_id"].is_null(), "{bad}");
    }

    // An item under a NON-show parent keys nothing — not the parent's id
    // (wrong namespace) and not its own (the parent is the authority the
    // schema names).
    sqlx::query(
        "INSERT INTO items (id, kind, title, norm_title, module_id, collection_id, parent_id)
         VALUES ('boxset', 'movie', 'Box', 'box', '01H', 'movies', NULL),
                ('boxed', 'movie', 'Boxed', 'boxed', '01H', 'movies', 'boxset')",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence, updated_at)
         VALUES ('boxset', 'tmdb', '888', 'Box', 'auto', unixepoch()),
                ('boxed', 'tmdb', '999', 'Boxed', 'auto', unixepoch())",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    let boxed = get("boxed".into()).await;
    assert!(
        boxed["metadata"]["tmdb_id"].is_null(),
        "a non-show parent must key nothing: {boxed}"
    );

    // The projection carries the same trust filters as the ids: a weak
    // tvdb episode row must not donate its numbering — nor trigger the
    // narrowing — on the strength of a guess.
    sqlx::query(
        "UPDATE provider_metadata SET confidence = 'weak' WHERE item_id = 'ep1' AND provider = 'tvdb'",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE provider_metadata SET proj_season = NULL, proj_episode = NULL
         WHERE item_id = 'ep1' AND provider = 'tmdb'",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    let untrusted = get("ep1".into()).await;
    assert_eq!(
        untrusted["metadata"]["tmdb_id"], 1403,
        "a weak projection must not narrow the ids: {untrusted}"
    );
    assert!(
        untrusted["metadata"]["proj_season"].is_null(),
        "{untrusted}"
    );
    assert!(
        untrusted["metadata"]["proj_episode"].is_null(),
        "{untrusted}"
    );
}

async fn fixture_with(file: FileUpsertRecord) -> Fx {
    fixture_net(file, Default::default()).await
}

async fn fixture_net(file: FileUpsertRecord, net: kahawai_hub::api::NetOptions) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let reg = Arc::new(Registry::new(db.clone(), Default::default()));
    reg.announce_collection("01H", "movies", "movies", &[TEST_ROOT.into()])
        .await
        .unwrap();
    reg.upsert_files("01H", "movies", vec![file]).await.unwrap();
    // A source nobody can reach cannot be negotiated against, so the
    // mediahost has to be up for the question to have an answer.
    reg.connected("01H", "mediahost", "mh", "fp", "test");

    let auth = Arc::new(
        kahawai_hub::auth::Auth::new(db.clone(), dir.path())
            .await
            .unwrap(),
    );
    auth.complete_setup("admin", "password-123").await.unwrap();
    let pair = auth.login("admin", "password-123").await.unwrap();
    let bearer = format!("Bearer {}", pair.access_token);

    let id: String = sqlx::query_scalar("SELECT id FROM items LIMIT 1")
        .fetch_one(&db)
        .await
        .unwrap();
    let subs_dir = tempfile::tempdir().unwrap().keep();
    let api = test_router_with(
        reg.clone(),
        auth,
        Arc::new(kahawai_hub::sessions::Sessions::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        subs_dir.clone(),
        net,
    );
    Fx {
        reg,
        _dir: dir,
        api,
        bearer,
        id,
        db,
        subs_dir,
    }
}

fn query(
    uri: &str,
    bearer: Option<&str>,
    ctype: Option<&str>,
    body: &str,
) -> axum::http::Request<axum::body::Body> {
    let mut b = axum::http::Request::builder().method("QUERY").uri(uri);
    if let Some(t) = bearer {
        b = b.header("authorization", t);
    }
    if let Some(c) = ctype {
        b = b.header("content-type", c);
    }
    b.body(axum::body::Body::from(body.to_string())).unwrap()
}

async fn json_of(resp: axum::response::Response) -> serde_json::Value {
    let body = axum::body::to_bytes(resp.into_body(), 1 << 22)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// The converged answer: the item, its discovered streams, and what
/// this client would actually be served.
#[tokio::test]
async fn query_returns_the_item_and_what_it_would_be_served() {
    let Fx {
        _dir,
        api,
        bearer,
        id,
        ..
    } = fixture().await;
    let profile = r#"{"profile":{"containers":["mp4"],
        "video":[{"codec":"h264"}],"audio":["aac"],
        "hdr":false,"graphics_overlay":false,"ass_render":false,
        "target_duration":{"mode":"ignore"}}}"#;
    let resp = api
        .oneshot(query(
            &format!("/api/v1/items/{id}"),
            Some(&bearer),
            Some("application/json"),
            profile,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("accept-query").unwrap(),
        "application/json",
        "the resource must advertise the query format it takes"
    );
    let j = json_of(resp).await;
    assert_eq!(j["title"], "Heat");
    // The discovered half is still there — QUERY is a superset of GET.
    assert_eq!(j["sources"][0]["streams"]["container"], "matroska");
    // ...and the converged half names the source it judged, so a
    // multi-source item cannot describe one file and play another.
    let n = &j["negotiated"];
    assert_eq!(n["source"]["path_rel"], "Heat (1995).mkv");
    // A REAL verdict, not merely a present one: "none" and "unplayable"
    // are non-empty strings, so the loose form of this assertion sat
    // green for a whole fixture that never parsed.
    assert_ne!(n["cost"], "unplayable", "{n}");
    assert_ne!(n["streams"]["video"], "none", "{n}");
    assert_ne!(n["streams"]["audio"], "none", "{n}");
    assert!(n["subtitles"].is_array());
}

/// HUB-37. The skip boundaries ride on QUERY, and this is the only way a
/// client gets them: the player asks once on its way into playback and the
/// answer arrives with the source it was chosen for.
#[tokio::test]
async fn query_carries_the_skip_boundaries() {
    let Fx {
        _dir,
        api,
        bearer,
        id,
        db,
        ..
    } = fixture().await;
    for (kind, start, end, source) in [
        ("recap", 0, 91_000, "blackframe"),
        ("intro", 270_000, 306_000, "chromaprint"),
        ("credits", 2_882_000, 2_916_000, "blackframe"),
    ] {
        sqlx::query(
            "INSERT INTO media_segments (item_id, kind, start_ms, end_ms, source)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(kind)
        .bind(start)
        .bind(end)
        .bind(source)
        .execute(&db)
        .await
        .unwrap();
    }

    let profile = r#"{"profile":{"containers":["mp4"],
        "video":[{"codec":"h264"}],"audio":["aac"],
        "hdr":false,"graphics_overlay":false,"ass_render":false,
        "target_duration":{"mode":"ignore"}}}"#;
    let resp = api
        .oneshot(query(
            &format!("/api/v1/items/{id}"),
            Some(&bearer),
            Some("application/json"),
            profile,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let j = json_of(resp).await;
    let segments = j["segments"].as_array().expect("segments array");
    // Earliest first, so a player can take the first match rather than sort.
    let kinds: Vec<&str> = segments
        .iter()
        .map(|s| s["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["recap", "intro", "credits"], "{j}");
    assert_eq!(segments[1]["start_ms"], 270_000);
    assert_eq!(segments[1]["end_ms"], 306_000);
    // Which analyzer answered, because the two fail differently.
    assert_eq!(segments[2]["source"], "blackframe");
}

/// The failure this would otherwise hide: `Router::route_layer` maps
/// `require_auth` onto the method fallback, but `MethodRouter::route_layer`
/// would not. Getting the wrong one serves item data unauthenticated.
#[tokio::test]
async fn query_without_a_token_is_refused() {
    let Fx { _dir, api, id, .. } = fixture().await;
    let resp = api
        .oneshot(query(
            &format!("/api/v1/items/{id}"),
            None,
            Some("application/json"),
            "{}",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "the method fallback bypassed auth");
}

/// The fallback swallows every unmatched method, so axum's own 405
/// response — `Allow` header included — no longer happens for us.
#[tokio::test]
async fn an_unsupported_method_still_says_what_is_allowed() {
    let Fx {
        _dir,
        api,
        bearer,
        id,
        ..
    } = fixture().await;
    let resp = api
        .oneshot(
            axum::http::Request::builder()
                .method("PUT")
                .uri(format!("/api/v1/items/{id}"))
                .header("authorization", &bearer)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 405);
    assert_eq!(resp.headers().get("allow").unwrap(), "GET, QUERY");
}

/// RFC 10008: "Servers MUST fail the request if the Content-Type
/// request field is missing or is inconsistent with the request
/// content."
#[tokio::test]
async fn a_query_without_a_json_content_type_is_refused() {
    let Fx {
        _dir,
        api,
        bearer,
        id,
        ..
    } = fixture().await;
    for ctype in [None, Some("text/plain")] {
        let resp = api
            .clone()
            .oneshot(query(
                &format!("/api/v1/items/{id}"),
                Some(&bearer),
                ctype,
                "{}",
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            415,
            "content-type {ctype:?} should have been refused"
        );
    }
}

/// **QUERY is safe.** The whole design rests on this: it reports the
/// plan as it stands and never does the work. Point it at an ASS track
/// with the ladder set overlay-FIRST — the arrangement that makes
/// rasterising the preferred answer — and it must still come back
/// without having rasterised anything.
///
/// The failure this catches is silent and plausible: `overlay_ready`
/// (`subtitles.rs`) generates a `raster` row and its NDJSON, takes up
/// to 30 s, and reaching for it here would make every item page pay a
/// session's start-up cost. Nothing else in the suite would notice,
/// because the ANSWER would be right — only its price would be wrong.
#[tokio::test]
async fn query_rasterises_nothing() {
    let Fx {
        _dir,
        api,
        bearer,
        id,
        db,
        subs_dir,
        ..
    } = fixture_with(ass_rec("Subbed (2011).mkv")).await;

    let user: String = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO user_prefs (user_id, scope, key, value)
         VALUES (?, '', 'ass_order', 'overlay,flatten,burn')",
    )
    .bind(&user)
    .execute(&db)
    .await
    .unwrap();

    // A client that renders no ASS itself but does composite bitmaps:
    // overlay is both reachable and first, so a QUERY willing to
    // generate would generate here.
    let resp = api
        .oneshot(query(
            &format!("/api/v1/items/{id}"),
            Some(&bearer),
            Some("application/json"),
            // Direct-playable (matroska accepted), so the plan is a real
            // one: an unplayable verdict carries no subtitles at all and
            // would pass this test for the wrong reason.
            r#"{"profile":{"containers":["matroska","mp4"],"video":[{"codec":"h264"}],
                "audio":["aac"],"hdr":false,
                "graphics_overlay":true,"ass_render":false,
                "target_duration":{"mode":"ignore"}}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let j = json_of(resp).await;
    let n = &j["negotiated"];
    assert!(
        n.is_object(),
        "the connected fixture must have a negotiation: {}",
        j["unavailable"]
    );
    assert_ne!(n["cost"], "unplayable", "nothing to rasterise FOR: {n}");
    let subs = n["subtitles"].as_array().unwrap();
    assert_eq!(subs.len(), 1, "the ASS track must be listed: {j}");

    // No raster row...
    let rasters: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM subtitle_tracks WHERE origin = 'raster'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(rasters, 0, "QUERY created a raster track row");

    // ...and no raster body on disk. Both, because either one alone
    // could be the half that a partial implementation writes.
    let stray: Vec<String> = std::fs::read_dir(&subs_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("raster-"))
        .collect();
    assert!(stray.is_empty(), "QUERY wrote raster artefacts: {stray:?}");

    // And having generated nothing, it must not have PROMISED overlay
    // either: the rung it reports is the one that is real right now.
    assert_ne!(
        subs[0]["delivery"], "overlay",
        "overlay promised with no rasterised track in existence: {j}"
    );
}

/// The fixtures declare their streams as JSON, and `parse_info`
/// swallows a bad parse into an EMPTY `MediaInfo` — which negotiates to
/// "none" for every stream and would make the tests above pass for
/// reasons that have nothing to do with what they claim to test.
#[test]
fn the_fixture_declarations_actually_parse() {
    for r in [rec("x.mkv", 1), ass_rec("y.mkv")] {
        let info: kahawai_core::media::MediaInfo =
            serde_json::from_str(&r.streams_json).unwrap_or_else(|e| panic!("{}: {e}", r.path_rel));
        assert_eq!(info.container.as_deref(), Some("matroska"));
        assert_eq!(info.video.len(), 1, "{}: no video", r.path_rel);
        assert_eq!(info.audio.len(), 1, "{}: no audio", r.path_rel);
    }
    let subs =
        serde_json::from_str::<kahawai_core::media::MediaInfo>(&ass_rec("y.mkv").streams_json)
            .unwrap()
            .subtitles;
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].format, "ass");
}
