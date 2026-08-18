//! QUERY and a real session must pick the SAME subtitle rung.
//!
//! This is the bug the GET/QUERY split was built to make impossible.
//! Before it, the item page's subtitle listing and the session's verdict
//! were two computations over two different inputs — the listing took
//! two booleans in a query string and resolved its own source by size,
//! the session took a whole `CapabilityProfile` and resolved one by
//! cost — and they diverged in practice: the listing promised `burn`
//! where the session would pick overlay, and promised `burn` to a client
//! that refused the video encode carrying it.
//!
//! They now share one `Negotiation`, which is exactly the kind of
//! invariant that holds until someone adds a second caller. So: ask
//! both, for one profile, against one real file with a real embedded
//! ASS track, and compare.
//!
//! The two answers are in different vocabularies on purpose (see the
//! module docs on `tracks::Delivery` and `negotiate::SubtitleTier`), so
//! the comparison is on the rung each names, not on the string.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

const HEADER: &str = "[Script Info]\nScriptType: v4.00+\nPlayResX: 1920\nPlayResY: 1080\n\n\
     [V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, Bold\n\
     Style: Default,Arial,48,&H00FFFFFF,0\n\n\
     [Events]\nFormat: Layer, Start, End, Style, Text\n";

fn render_with_ass(path: &std::path::Path) {
    kahawai_media::testutil::render_h264_ass_mkv(
        path,
        HEADER,
        &[
            (500, 3000, "First line.".into()),
            (3500, 6000, "Second line.".into()),
        ],
    );
}

/// One profile, asked twice. `ass_render: true` is the interesting
/// case — the rung is `native`/`ass`, which no artefact gates, so the
/// two paths have no excuse to differ.
#[tokio::test]
async fn query_and_a_real_session_agree_on_the_ass_rung() {
    let h = common::harness("Subbed (2011).mkv", render_with_ass).await;

    const PROFILE: &str = r#"{"containers":["mp4"],"video":[{"codec":"h264"}],
        "audio":["aac"],"hdr":false,
        "graphics_overlay":true,"ass_render":true,
        "target_duration":{"mode":"accurate"}}"#;

    // 1. QUERY: what would I be served?
    let resp = h
        .api
        .clone()
        .oneshot(
            Request::builder()
                .method("QUERY")
                .uri(format!("/api/v1/items/{}", h.item_id))
                .header("authorization", &h.bearer)
                .header("content-type", "application/json")
                .body(Body::from(format!("{{\"profile\":{PROFILE}}}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let q = common::json_body(resp).await;
    let n = &q["negotiated"];
    assert_ne!(n["cost"], "unplayable", "nothing was negotiated: {n}");

    let q_verdicts = n["streams"]["subtitles"].as_array().unwrap();
    assert_eq!(q_verdicts.len(), 1, "the embedded ASS track: {n}");
    let q_tier = q_verdicts[0]["tier"].as_str().unwrap().to_string();
    let q_delivery = n["subtitles"][0]["delivery"].as_str().unwrap().to_string();

    // The two projections of ONE answer must not contradict each other
    // inside a single response either — this is the pairing that
    // actually drifted during HUB-32d.
    assert_eq!(
        (q_delivery.as_str(), q_tier.as_str()),
        ("ass", "text"),
        "QUERY's own two vocabularies disagree: {n}"
    );

    // 2. The real thing: start a session with the same profile.
    let resp = h
        .api
        .clone()
        .oneshot(
            Request::post("/api/v1/playback/sessions")
                .header("authorization", &h.bearer)
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    "{{\"item_id\":\"{}\",\"profile\":{PROFILE}}}",
                    h.item_id
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let s = common::json_body(resp).await;
    assert_eq!(status, StatusCode::CREATED, "{s}");

    let s_verdicts = s["streams"]["subtitles"].as_array().unwrap();
    assert_eq!(s_verdicts.len(), 1, "session subtitle verdicts: {s}");

    assert_eq!(
        s_verdicts[0]["tier"], q_tier,
        "QUERY said {q_tier}, the session said {} — the split's whole \
         point is that these cannot differ\nquery: {n}\nsession: {s}",
        s_verdicts[0]["tier"]
    );
    assert_eq!(
        s_verdicts[0]["index"], q_verdicts[0]["index"],
        "same rung, different STREAM: {n} vs {s}"
    );

    // Tidy up: a live session holds a lease and a worker.
    let _ = h
        .api
        .oneshot(
            Request::delete(format!(
                "/api/v1/playback/sessions/{}",
                s["session_id"].as_str().unwrap()
            ))
            .header("authorization", &h.bearer)
            .body(Body::empty())
            .unwrap(),
        )
        .await;
}

/// The start response's `subtitle_listing` is computed against the
/// SESSION's effective profile — the whole point: after a
/// capability-masked restart the item QUERY's listing still reflects
/// the page-load profile, and a client reading it kept rendering ASS
/// client-side until a page reload.
#[tokio::test]
async fn the_session_listing_reflects_the_session_profile() {
    let h = common::harness("Masked (2012).mkv", render_with_ass).await;

    let start = |ass_render: bool| {
        let api = h.api.clone();
        let bearer = h.bearer.clone();
        let item_id = h.item_id.clone();
        async move {
            let profile = format!(
                r#"{{"containers":["mp4"],"video":[{{"codec":"h264"}}],
                    "audio":["aac"],"hdr":false,
                    "graphics_overlay":true,"ass_render":{ass_render},
                    "target_duration":{{"mode":"accurate"}}}}"#
            );
            let resp = api
                .oneshot(
                    Request::post("/api/v1/playback/sessions")
                        .header("authorization", &bearer)
                        .header("content-type", "application/json")
                        .body(Body::from(format!(
                            "{{\"item_id\":\"{item_id}\",\"profile\":{profile}}}"
                        )))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = resp.status();
            let s = common::json_body(resp).await;
            assert_eq!(status, StatusCode::CREATED, "{s}");
            s
        }
    };

    let with_render = start(true).await;
    assert_eq!(
        with_render["subtitle_listing"][0]["delivery"], "ass",
        "{with_render}"
    );

    let without = start(false).await;
    let delivery = without["subtitle_listing"][0]["delivery"].as_str().unwrap();
    assert_ne!(
        delivery, "ass",
        "the masked profile must change THIS session's listing: {without}"
    );

    for s in [&with_render, &without] {
        let _ = h
            .api
            .clone()
            .oneshot(
                Request::delete(format!(
                    "/api/v1/playback/sessions/{}",
                    s["session_id"].as_str().unwrap()
                ))
                .header("authorization", &h.bearer)
                .body(Body::empty())
                .unwrap(),
            )
            .await;
    }
}
