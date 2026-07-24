//! Embedded web UI (HUB-25/28): the Vite build of `web/` compiled into the
//! binary, served on the same listener as the API. The SPA is a pure client
//! of the public API — no private endpoints.

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;

#[derive(rust_embed::Embed)]
#[folder = "../../web/dist"]
struct Assets;

pub fn router() -> Router {
    Router::new()
        .route("/", get(|| async { Redirect::permanent("/app/") }))
        .route("/app", get(|| async { Redirect::permanent("/app/") }))
        .route("/app/", get(|| async { spa_response("index.html") }))
        .route("/app/{*path}", get(serve))
}

async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches("/app/");
    spa_response(path)
}

fn spa_response(path: &str) -> Response {
    let (asset, path) = match Assets::get(path) {
        Some(a) => (a, path),
        // Client-side routes fall back to the SPA shell.
        None => match Assets::get("index.html") {
            Some(a) => (a, "index.html"),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    "web UI not embedded in this build (web/dist missing at compile time)",
                )
                    .into_response()
            }
        },
    };
    let mime = match path.rsplit_once('.').map(|(_, e)| e) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("map" | "json") => "application/json",
        // instantiateStreaming refuses anything but application/wasm.
        Some("wasm") => "application/wasm",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    };
    // Vite emits content-hashed asset names; index.html must revalidate.
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, cache)],
        asset.data.into_owned(),
    )
        .into_response()
}
