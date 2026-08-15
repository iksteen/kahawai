//! Embedded web UI (HUB-25/28): the Vite build of `web/` compiled into the
//! binary, served on the same listener as the API. The SPA is a pure client
//! of the public API — no private endpoints.
//!
//! `--web-dir` swaps the embedded bundle for one on disk. It is a development
//! and operator affordance — try a bundle this binary does not carry, without
//! a Cargo rebuild — and not a second serving path: everything below decides
//! only WHERE a file comes from, and the rest (the SPA fallback, the `assets/`
//! 404, the cache headers) is identical either way. Live editing is Vite's dev
//! server proxying `/api` at the hub, not this.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use tower_http::compression::CompressionLayer;

#[derive(rust_embed::Embed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
struct Assets;

/// Where `/app/` is read from. `None` is the embedded bundle, which is what a
/// release ships and the only thing that needs no filesystem to exist.
///
/// A directory here is CANONICAL — `resolve_dir` made it so. The containment
/// check in `load` compares a resolved path against this one, and two paths
/// can only be compared once both are resolved.
type WebDir = Arc<Option<PathBuf>>;

/// Vet `--web-dir` where the mistake was made, at startup, rather than at the
/// first request.
///
/// A typo, or a relative path under a unit file whose working directory is
/// `/`, otherwise reached the fallback and answered every request with a 200
/// and "the web UI was not embedded in this build" — which is false, points
/// the diagnosis at the binary instead of the flag, and says it in the one
/// place the operator is least likely to be looking.
pub fn resolve_dir(dir: &Path) -> Result<PathBuf> {
    let resolved =
        std::fs::canonicalize(dir).with_context(|| format!("--web-dir {}", dir.display()))?;
    if !resolved.is_dir() {
        bail!("--web-dir {} is not a directory", resolved.display());
    }
    Ok(resolved)
}

/// Rust-only checks and satellite builds intentionally do not generate Vite's
/// bundle. Keep `/app/` honest and diagnosable in such a binary without
/// pretending that this page is the application.
const NO_WEB_UI: &str = "<!doctype html><html><head><title>kahawai</title></head><body><h1>kahawai</h1><p>The web UI was not embedded in this build.</p></body></html>";

pub fn router(web_dir: Option<PathBuf>) -> Router {
    Router::new()
        .route("/", get(|| async { Redirect::permanent("/app/") }))
        .route("/app", get(|| async { Redirect::permanent("/app/") }))
        .route("/app/", get(index))
        .route("/app/{*path}", get(serve))
        // The hub ignored `Accept-Encoding` and sent every byte as-is, which
        // made the build's gzip column fiction: 270 kB of JavaScript arrived
        // as 270 kB. Everything served here is text or wasm and compresses
        // to roughly a third. Recomputed per request, which is affordable
        // precisely because these are the responses that carry
        // `immutable` — a returning client does not ask again.
        .layer(CompressionLayer::new())
        .with_state(Arc::new(web_dir))
}

async fn index(State(dir): State<WebDir>) -> Response {
    spa_response(&dir, "index.html").await
}

async fn serve(State(dir): State<WebDir>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches("/app/");
    spa_response(&dir, path).await
}

/// First half of the containment check: the spelling.
///
/// Only plain descending names are joined — no `..`, no empty or absolute
/// segment, and no `%`, so there is no encoding left to decode a traversal out
/// of afterwards. Vite emits content-hashed ASCII names, so refusing
/// everything else costs nothing.
///
/// This is a filter on the STRING and not a list of expected artefacts: every
/// readable file under the directory is served, which is what pointing the hub
/// at a directory means. It is also why the second half exists — a string
/// filter cannot see a symlink.
fn safe_rel(path: &str) -> Option<&str> {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/');
    if !path.chars().all(plain) {
        return None;
    }
    if path
        .split('/')
        .any(|segment| segment.is_empty() || segment == "..")
    {
        return None;
    }
    Some(path)
}

async fn load(dir: &WebDir, path: &str) -> Option<Vec<u8>> {
    let Some(root) = dir.as_ref() else {
        return Assets::get(path).map(|asset| asset.data.into_owned());
    };
    // Second half of the containment check: where the name actually leads.
    // `safe_rel` refuses a `..` in the request, and a symlink inside the
    // directory is a `..` the request never has to spell — one `dist` built by
    // a tool that links, or one stray link in a directory somebody pointed
    // this at, and `/app/*` exports arbitrary readable files. Unauthenticated,
    // by necessity, on a bind that is usually 0.0.0.0.
    //
    // Both sides resolved and then compared. This is the check that holds the
    // directory; the string filter only keeps the obvious spellings out.
    let resolved = tokio::fs::canonicalize(root.join(safe_rel(path)?))
        .await
        .ok()?;
    if !resolved.starts_with(root) {
        return None;
    }
    tokio::fs::read(resolved).await.ok()
}

async fn spa_response(dir: &WebDir, path: &str) -> Response {
    let (data, path) = match load(dir, path).await {
        Some(data) => (data, path),
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
        None => match load(dir, "index.html").await {
            Some(data) => (data, "index.html"),
            None => {
                return (
                    [
                        (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                        (header::CACHE_CONTROL, "no-cache"),
                    ],
                    NO_WEB_UI,
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
        data,
    )
        .into_response()
}
