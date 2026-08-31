//! Minimal production-web server for the SEC-WEB-1 browser gate.
//!
//! It deliberately uses the real web router and request boundary. Only the
//! boot API calls and adversarial probe routes are fixtures, so Chromium
//! exercises the exact headers, file serving, MIME types and production bundle
//! that the hub ships without needing a database or configured installation.

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Json;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::{get, post};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let web_dir = PathBuf::from(args.next().unwrap_or_else(|| "web/dist".into()));
    let address: SocketAddr = args
        .next()
        .unwrap_or_else(|| "127.0.0.1:18420".into())
        .to_string_lossy()
        .parse()?;
    anyhow::ensure!(
        args.next().is_none(),
        "usage: csp_browser_fixture [web-dir] [address]"
    );

    let web_dir = kahawai_hub::web::resolve_dir(&web_dir)?;
    let api = axum::Router::new()
        .route(
            "/api/v1/bootstrap",
            get(|| async {
                Json(serde_json::json!({
                    "setup_required": false,
                    "setup_available": false,
                    "setup_url": null
                }))
            }),
        )
        .route(
            "/api/v1/auth/refresh",
            post(|| async {
                kahawai_hub::error::ApiError::new(
                    kahawai_hub::error::ErrorCode::InvalidRefresh,
                    "no browser session",
                )
            }),
        )
        // Same-origin and intentionally script-shaped, but declared JSON. The
        // browser test loads it through <script> and proves `nosniff` wins.
        .route(
            "/api/v1/security/nosniff-script",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/json")],
                    "globalThis.__kahawaiNosniffExecuted = true",
                )
                    .into_response()
            }),
        )
        // A normal same-origin script (not automation-injected JavaScript)
        // that attempts eval. CSP must allow the file and refuse its eval.
        .route(
            "/api/v1/security/eval-script.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript")],
                    "try { eval('globalThis.__kahawaiEvalExecuted = true') } catch (_) { globalThis.__kahawaiEvalBlocked = true }",
                )
                    .into_response()
            }),
        )
        // Same origin is intentional and stricter than a foreign attacker:
        // `frame-ancestors 'none'` must refuse even the hub's own page.
        .route(
            "/security/frame-probe",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    "<!doctype html><iframe id=probe src=/app/></iframe><script>probe.addEventListener('load',()=>document.body.dataset.probeLoaded='yes')</script>",
                )
            }),
        );
    let app = api
        .merge(kahawai_hub::web::router(Some(web_dir)))
        .layer(axum::middleware::from_fn(
            kahawai_hub::error::request_context,
        ));

    let listener = tokio::net::TcpListener::bind(address).await?;
    println!("SEC-WEB-1 fixture listening on http://{address}/app/");
    axum::serve(listener, app).await?;
    Ok(())
}
