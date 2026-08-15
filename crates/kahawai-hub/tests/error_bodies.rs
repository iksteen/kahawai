//! Every refusal reaches a client as `{code, message}`, over the wire.
//!
//! The module's own unit tests pin the code→status table; this pins that the
//! table is what a real request gets back, on real routes, through axum's
//! `IntoResponse` — which is the half a table cannot prove.
//!
//! And it pins the negative: the anyhow chain does NOT come out. That is the
//! finding this change answers (`kahawai-hub-review-findings.md` §2 and §6) —
//! `format!("{e:#}")` published the hub's scratch layout, the pipeline
//! worker's argv and GStreamer's stderr to anyone whose transcode failed.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use kahawai_hub::auth::Auth;
use tower::ServiceExt;

/// Status and the parsed body, which is the pair a client actually sees.
async fn refusal(router: axum::Router, request: Request<Body>) -> (StatusCode, String, String) {
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("application/json"),
        "a refusal must be JSON, got {content_type:?}"
    );
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let code = body["code"].as_str().expect("a code").to_string();
    let message = body["message"].as_str().expect("a message").to_string();
    (status, code, message)
}

async fn setup_router() -> (tempfile::TempDir, axum::Router, Arc<Auth>) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let auth = Arc::new(Auth::new(db, dir.path()).await.unwrap());
    let router = kahawai_hub::api::setup_router(auth.clone(), None);
    (dir, router, auth)
}

/// The public router, with a CORS origin configured. Heavier than
/// `setup_router` — the public one is the only place the CORS layer exists.
async fn api_harness() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(kahawai_hub::registry::Registry::new(
        db.clone(),
        Default::default(),
    ));
    let auth = Arc::new(Auth::new(db, dir.path()).await.unwrap());
    // Past setup, or `login` answers `setup_required` before it ever reaches
    // the throttle — which is how the first cut of the Retry-After test
    // "failed": it was never testing the throttle at all.
    auth.complete_setup("admin", "hunter22222hunter")
        .await
        .unwrap();
    let sessions = Arc::new(kahawai_hub::sessions::Sessions::new(
        tempfile::tempdir().unwrap().keep(),
    ));
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
    let enricher = Arc::new(kahawai_hub::enrich::Enricher::new(dir.path().to_path_buf()));
    let router = kahawai_hub::api::router(
        registry,
        auth,
        sessions,
        enrollments,
        Arc::new(kahawai_hub::subtitles::Subtitles::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        Arc::new(kahawai_hub::artwork::Artwork::new(
            tempfile::tempdir().unwrap().keep(),
            enricher.clone(),
        )),
        enricher,
        kahawai_hub::api::NetOptions {
            cors_origins: vec!["https://app.example.com".into()],
            ..Default::default()
        },
    );
    (dir, router)
}

fn setup_request(username: &str, password: &str) -> Request<Body> {
    Request::post("/api/v1/setup")
        .header("host", "localhost:8422")
        .header("origin", "http://localhost:8422")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"username": username, "password": password}).to_string(),
        ))
        .unwrap()
}

#[tokio::test]
async fn a_refusal_is_json_with_a_code() {
    let (_dir, router, _auth) = setup_router().await;
    let (status, code, message) = refusal(router, setup_request("", "short")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code, "bad_request");
    assert!(!message.is_empty());
}

/// Setup is a first-run flow. Its second attempt is a different refusal from
/// its first, and a client can now say which without reading either sentence.
#[tokio::test]
async fn the_same_route_distinguishes_its_refusals_by_code() {
    let (_dir, router, auth) = setup_router().await;
    let ok = router
        .clone()
        .oneshot(setup_request("admin", "hunter222222"))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::NO_CONTENT);
    assert!(!auth.setup_required());

    let (status, code, _) = refusal(router, setup_request("admin", "hunter222222")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code, "setup_complete");
}

/// A 500's cause is by definition something the caller cannot act on, and it
/// is the one that carries paths and subprocess output. It goes to the log.
#[tokio::test]
async fn an_internal_failure_says_nothing_about_the_hub() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let auth = Arc::new(Auth::new(db.clone(), dir.path()).await.unwrap());
    let router = kahawai_hub::api::setup_router(auth, None);
    // Storage failure rather than validation failure: the arm that used to
    // hand the client whatever the database said.
    db.close().await;

    let (status, code, message) = refusal(router, setup_request("admin", "hunter222222")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(code, "internal");
    for leak in [dir.path().display().to_string().as_str(), "sqlite", "pool"] {
        assert!(
            !message.to_lowercase().contains(&leak.to_lowercase()),
            "a 500 leaked {leak:?}: {message}"
        );
    }
}

/// A malformed body is a refusal like any other.
///
/// axum's own `Json` rejection is `text/plain` with no code, so this was the
/// one 4xx in the hub that did not carry one — on twenty-one routes whose
/// document says otherwise. `ApiJson` is what makes it uniform, and this is
/// the only test that goes through an extractor rather than a handler.
#[tokio::test]
async fn a_body_that_does_not_parse_refuses_in_the_same_shape() {
    let (_dir, router, _auth) = setup_router().await;
    let request = Request::post("/api/v1/setup")
        .header("host", "localhost:8422")
        .header("origin", "http://localhost:8422")
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let (status, code, message) = refusal(router, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code, "bad_request");
    // axum's account of what was wrong with it survives; only the shape
    // changed.
    assert!(message.to_lowercase().contains("json"), "{message}");
}

/// A body that parses but is the wrong shape — axum answers 422 for this, and
/// no route declared one. Narrowed to the 400 they all do declare.
#[tokio::test]
async fn a_body_of_the_wrong_shape_is_the_status_the_document_promises() {
    let (_dir, router, _auth) = setup_router().await;
    let request = Request::post("/api/v1/setup")
        .header("host", "localhost:8422")
        .header("origin", "http://localhost:8422")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"username": 7}"#))
        .unwrap();
    let (status, code, _) = refusal(router, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code, "bad_request");
}

/// A 429 that the hub can put a clock on says so in a header.
///
/// The contract is that the status carries the retry decision, so a client
/// that honours it knows to come back and not when. A login lockout runs from
/// 30 s to fifteen minutes and the only statement of which was `message` —
/// prose this module tells clients not to read.
#[tokio::test]
async fn a_throttled_login_says_how_long_in_a_header() {
    let (_dir, router) = api_harness().await;
    let attempt = || {
        Request::post("/api/v1/auth/token")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"client": "api", "username": "admin", "password": "wrong"})
                    .to_string(),
            ))
            .unwrap()
    };
    // Enough wrong answers to trip it. The threshold is the auth module's;
    // this asks until the status changes rather than encoding it here.
    let mut throttled = None;
    for _ in 0..12 {
        let response = router.clone().oneshot(attempt()).await.unwrap();
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            throttled = Some(response);
            break;
        }
    }
    let response = throttled.expect("repeated wrong passwords did not throttle");
    let secs = response
        .headers()
        .get(axum::http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .expect("a 429 the hub can time carries Retry-After");
    assert!(secs >= 1, "Retry-After was {secs}");
}

/// No `responses(...)` block declares the same status twice.
///
/// OpenAPI has one response per status and utoipa keeps the LAST of two, in
/// silence. A sweep that added a `setup_required` 503 to a route that already
/// declared a `provider_unconfigured` 503 therefore did not document both — it
/// dropped the one that route actually returns.
///
/// Source-level, because the generated document is where the evidence has
/// already been destroyed. The sibling test below compares declared statuses
/// against their codes and cannot see this one: both entries were 503.
#[test]
fn no_route_declares_a_status_twice() {
    let source = include_str!("../src/api.rs");
    let mut wrong = Vec::new();
    let mut blocks = 0;
    for (i, block) in source.split("responses(").enumerate().skip(1) {
        let Some(end) = block.find("\n    )") else {
            continue;
        };
        blocks += 1;
        let mut seen = Vec::new();
        for line in block[..end].lines() {
            let Some(rest) = line.trim().strip_prefix("(status = ") else {
                continue;
            };
            let status: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if status.is_empty() {
                continue;
            }
            if seen.contains(&status) {
                wrong.push(format!("responses block #{i} declares {status} twice"));
            }
            seen.push(status);
        }
    }
    assert!(blocks > 40, "only found {blocks} responses blocks to check");
    assert_eq!(wrong, Vec::<String>::new());
}

/// Every status the document declares agrees with the code its description
/// names.
///
/// The one class of untruth the TypeScript contract test cannot see: it checks
/// body shapes and which statuses are declared, never that a declared status
/// matches the code beside it. `provider_unconfigured` moved from 409 to 503
/// in the code and its declaration stayed at 409, so the published document
/// described a response the hub does not send — on the one route that returns
/// it. Descriptions here name their code in backticks, which is what makes
/// this checkable at all.
#[test]
fn a_declared_status_agrees_with_the_code_its_description_names() {
    let document = kahawai_hub::api::openapi_document();
    let json = serde_json::to_value(&document).unwrap();
    let mut checked = 0;
    let mut wrong = Vec::new();
    for (path, item) in json["paths"].as_object().unwrap() {
        for (verb, op) in item.as_object().unwrap() {
            let Some(responses) = op.get("responses").and_then(|r| r.as_object()) else {
                continue;
            };
            for (status, response) in responses {
                let Some(description) = response.get("description").and_then(|d| d.as_str()) else {
                    continue;
                };
                // ``code`` — the only backticked token in these descriptions.
                let Some(named) = description
                    .split('`')
                    .nth(1)
                    .filter(|t| t.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
                else {
                    continue;
                };
                let Ok(code) =
                    serde_json::from_value::<kahawai_hub::error::ErrorCode>(named.into())
                else {
                    wrong.push(format!(
                        "{verb} {path} {status}: `{named}` is not an ErrorCode"
                    ));
                    continue;
                };
                checked += 1;
                if code.status().as_str() != status {
                    wrong.push(format!(
                        "{verb} {path} declares {status} for `{named}`, which is {}",
                        code.status().as_u16()
                    ));
                }
            }
        }
    }
    assert!(checked > 20, "only {checked} descriptions named a code");
    assert_eq!(wrong, Vec::<String>::new());
}

/// A configured CORS origin still gets its preflight answered.
///
/// The router's own fallbacks and its CORS layer interact, and the order is
/// not a style question. `layer` wraps each route AND each method router's
/// default fallback; setting the fallbacks afterwards replaced the wrapped
/// ones with unwrapped handlers. No route here registers `options`, so a
/// browser preflight reaches the method-not-allowed fallback — which meant
/// every cross-origin POST/PUT/DELETE preflighted, got a 405 with no
/// `Access-Control-Allow-Origin`, and was blocked by the browser.
///
/// Nothing else in the suite would have noticed: every other test speaks to
/// the router directly, where CORS is invisible.
#[tokio::test]
async fn a_preflight_from_an_allowed_origin_is_answered_and_not_refused() {
    let (_dir, router) = api_harness().await;
    let response = router
        .oneshot(
            Request::options("/api/v1/auth/token")
                .header("origin", "https://app.example.com")
                .header("access-control-request-method", "POST")
                .header("access-control-request-headers", "content-type")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the preflight was refused"
    );
    assert!(
        response
            .headers()
            .contains_key("access-control-allow-origin"),
        "the preflight carried no Access-Control-Allow-Origin"
    );
}

/// And the fallbacks still answer, through the layer rather than around it.
#[tokio::test]
async fn the_fallbacks_survive_the_cors_layer() {
    let (_dir, router) = api_harness().await;
    let (status, code, _) = refusal(
        router,
        Request::get("/api/v1/no-such-thing")
            .header("origin", "https://app.example.com")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code, "not_found");
}

/// The two refusals axum makes without a handler.
///
/// Its defaults are a bare 404 and a bare 405 — no body, no content type — so
/// a typo in a path handed a client generated from the document nothing to
/// parse on a status it was told would carry `{code, message}`. They are the
/// router's own answer now.
#[tokio::test]
async fn an_unknown_route_and_a_wrong_method_refuse_in_the_same_shape() {
    let (_dir, router, _auth) = setup_router().await;
    let (status, code, _) = refusal(
        router.clone(),
        Request::get("/api/v1/no-such-thing")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code, "not_found");

    // A known path, a verb it does not answer.
    let (status, code, _) = refusal(
        router,
        Request::get("/api/v1/setup").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(code, "method_not_allowed");
}

/// A path segment or query parameter that will not parse is a refusal too.
///
/// `ApiJson` closed the body hole and left this one: `?limit=abc` was still
/// answering axum's `text/plain` with no code, on routes that declared no 400
/// at all. Making the contract true of most refusals is not what the document
/// says.
#[tokio::test]
async fn a_query_parameter_that_will_not_parse_refuses_in_the_same_shape() {
    // The setup listener has no parameterised route, so this one goes through
    // the extractor directly — the unit under test is `ApiQuery`, not a route.
    use axum::extract::FromRequestParts;
    #[derive(serde::Deserialize)]
    struct Page {
        #[allow(dead_code)]
        limit: usize,
    }
    let (mut parts, _) = Request::get("/api/v1/items?limit=abc")
        .body(Body::empty())
        .unwrap()
        .into_parts();
    let refused = <kahawai_hub::error::ApiQuery<Page> as FromRequestParts<()>>::from_request_parts(
        &mut parts,
        &(),
    )
    .await
    .err()
    .expect("limit=abc is not a usize");
    let response = axum::response::IntoResponse::into_response(refused);
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
}

/// A wrong `Content-Type` is not the same bug as a body that will not parse,
/// and axum already knows which. Collapsing both into 400 was an earlier cut
/// of `ApiJson`, and it quietly changed the status QUERY had been answering.
#[tokio::test]
async fn a_wrong_content_type_is_415_and_not_400() {
    let (_dir, router, _auth) = setup_router().await;
    let request = Request::post("/api/v1/setup")
        .header("host", "localhost:8422")
        .header("origin", "http://localhost:8422")
        .header("content-type", "text/plain")
        .body(Body::from("{}"))
        .unwrap();
    let (status, code, _) = refusal(router, request).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(code, "unsupported_media_type");
}

/// A UNIQUE constraint firing is the request's data, not the hub's health.
///
/// `refusal_or_internal` treats a `sqlx::Error` anywhere in the chain as proof
/// the hub is unwell, which is right for the producers that refuse with
/// `Option::context` and `ensure!` — and inverted for `create_library`, whose
/// one user-caused refusal IS the `libraries.name UNIQUE` constraint. Its
/// first cut answered 500 "the hub could not complete this request" to an
/// admin who typed a name that was already taken.
#[tokio::test]
async fn a_name_that_is_taken_is_a_conflict_and_not_a_hub_fault() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = std::sync::Arc::new(kahawai_hub::registry::Registry::new(
        db,
        kahawai_transport::mtls::AllowedCerts::default(),
    ));
    registry.create_library("Films", "movies").await.unwrap();

    let again = registry
        .create_library("Films", "movies")
        .await
        .unwrap_err();
    assert!(
        kahawai_hub::api::is_unique_violation(&again),
        "a duplicate name must be recognisable as the caller's, not the hub's: {again:#}"
    );

    // And the other refusal on that route is not one, so it does not fall into
    // the same arm.
    let bad_type = registry
        .create_library("Shows", "nonsense")
        .await
        .unwrap_err();
    assert!(
        !kahawai_hub::api::is_unique_violation(&bad_type),
        "{bad_type:#}"
    );
}

/// The Origin guard, which is a 403 that must not read as "sign in again".
#[tokio::test]
async fn a_forbidden_setup_page_is_forbidden_and_not_unauthenticated() {
    let (_dir, router, _auth) = setup_router().await;
    let request = Request::post("/api/v1/setup")
        .header("host", "kahawai.example")
        .header("origin", "http://kahawai.example")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"username": "admin", "password": "hunter222222"}).to_string(),
        ))
        .unwrap();
    let (status, code, _) = refusal(router, request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(code, "forbidden");
}
