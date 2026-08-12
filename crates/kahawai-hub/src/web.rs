//! Embedded web UI (HUB-25/28): the Vite build of `web/` compiled into the
//! binary, served on the same listener as the API. The SPA is a pure client
//! of the public API — no private endpoints.

use axum::Router;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use tower_http::compression::CompressionLayer;

#[derive(rust_embed::Embed)]
#[folder = "../../web/dist"]
struct Assets;

pub fn router() -> Router {
    Router::new()
        .route("/", get(|| async { Redirect::permanent("/app/") }))
        .route("/app", get(|| async { Redirect::permanent("/app/") }))
        .route("/app/", get(|| async { spa_response("index.html") }))
        .route("/app/{*path}", get(serve))
        // The hub ignored `Accept-Encoding` and sent every byte as-is, which
        // made the build's gzip column fiction: 270 kB of JavaScript arrived
        // as 270 kB. Everything served here is text or wasm and compresses
        // to roughly a third. Recomputed per request, which is affordable
        // precisely because these are the responses that carry
        // `immutable` — a returning client does not ask again.
        .layer(CompressionLayer::new())
}

async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches("/app/");
    spa_response(path)
}

fn spa_response(path: &str) -> Response {
    let (asset, path) = match Assets::get(path) {
        Some(a) => (a, path),
        // A missing BUILD ARTEFACT is a 404, not the shell. The fallback
        // exists for client-side routes, and `assets/…` is never one: those
        // paths are content-hashed and only ever produced by the build.
        //
        // Answering them with `index.html` and a 200 is what made a hub
        // upgrade break every tab that was already open. The app code-splits
        // the player, so pressing Play fetches a hash that the new binary no
        // longer embeds; the browser got HTML where a module was promised,
        // rejected it on the MIME type, and `React.lazy` caches that rejected
        // promise for the life of the page — so the error boundary's Try again
        // could never work and only a reload helped. A 404 lets the client
        // tell "this build is gone" from "this route is yours to handle".
        None if path.starts_with("assets/") => {
            return (StatusCode::NOT_FOUND, "no such asset in this build").into_response();
        }
        // Client-side routes fall back to the SPA shell.
        None => match Assets::get("index.html") {
            Some(a) => (a, "index.html"),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    "web UI not embedded in this build (web/dist missing at compile time)",
                )
                    .into_response();
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
