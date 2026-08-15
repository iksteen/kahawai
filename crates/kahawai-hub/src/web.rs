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
    // And that it is the bundle rather than the directory ABOVE it. A
    // directory exists, canonicalises and is a directory whether or not
    // anything was ever built into it, so `--web-dir /srv/kahawai` when the
    // bundle is in `/srv/kahawai/dist` started the hub, logged that it was
    // serving from disk, and answered every request with a 200 saying the UI
    // was not embedded in this build — the exact outcome this function was
    // written to stop, one directory up.
    if !resolved.join("index.html").is_file() {
        bail!(
            "--web-dir {} has no index.html; point it at a built bundle (a Vite `dist`)",
            resolved.display()
        );
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
    let raw = uri.path().trim_start_matches("/app/");
    // ONE decode, at the edge, and everything below reads the result: the
    // lookup, the MIME type, the cache policy and the `assets/` rule. They
    // have to agree about what was asked for, and they cannot if some of them
    // read the encoded spelling — `/app/assets%2Fmain-abc123.js` found the
    // file and then called it a client-side route, so a MISS on that spelling
    // answered `index.html` with a 200, which is the upgrade-breaks-open-tabs
    // failure the `assets/` branch exists to prevent.
    //
    // Decoding for BOTH sources, not just the directory. An earlier cut did it
    // inside the directory branch alone and so moved the divergence rather
    // than closing it: the same bundle served a percent-encoded name from
    // `--web-dir` and 404'd it from the embedded copy.
    match safe_rel(raw) {
        Some(path) => spa_response(&dir, &path).await,
        // Not a name this serves. The RAW spelling still decides the
        // `assets/` rule, so a refused build-artefact path is a 404 rather
        // than the shell.
        // A name this cannot serve answers exactly as a miss does: a refusal
        // and an absence are the same thing to a client, and telling them
        // apart would only report what is on disk.
        None => shell_or_404(&dir, raw).await,
    }
}

/// First half of the containment check: the spelling.
///
/// Percent-decoded first, then checked — no `..`, no empty or absolute
/// segment. Refusing `%` outright was the first cut and it was wrong in a way
/// only a real bundle shows: Vite's default `assetFileNames` keeps the source
/// basename, so an imported `café.png` becomes `assets/café-a1b2c3.png` and
/// arrives percent-encoded. `rust_embed` applies no such filter, so the two
/// modes disagreed about which builds they could serve — the same hub, the
/// same bundle, a 404 from one and a 200 from the other.
///
/// Decoding is safe here because it happens BEFORE the checks rather than
/// after: there is no second encoding left for a `..` to hide in, and the
/// resolved-path comparison in `load` is what actually holds the directory.
///
/// This is a filter on the STRING and not a list of expected artefacts: every
/// readable file under the directory is served, which is what pointing the hub
/// at a directory means. It is also why the second half exists — a string
/// filter cannot see a symlink.
fn safe_rel(path: &str) -> Option<String> {
    let decoded = percent_decode(path)?;
    if decoded
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return None;
    }
    // A NUL cannot be in a path at all, and a backslash is a separator on some
    // platforms and an ordinary filename character here — a name that means
    // two different things on two systems is not one to serve.
    //
    // `%2f` needs no case of its own: it decodes to `/` BEFORE the split
    // above, so it arrives as the separator it is and its segments are checked
    // like any other. That is the whole reason decoding comes first.
    if decoded.contains('\0') || decoded.contains('\\') {
        return None;
    }
    Some(decoded)
}

/// `%XX` only, and only for bytes that make valid UTF-8. Anything malformed is
/// not a path this serves.
fn percent_decode(path: &str) -> Option<String> {
    if !path.contains('%') {
        return Some(path.to_string());
    }
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = path.get(i + 1..i + 3)?;
            // Two HEXDIG, as RFC 3986 says, and checked rather than left to
            // the parser: `from_str_radix` accepts a sign, so `%+A` decoded to
            // a newline and `%-0` to a NUL. Harmless downstream — everything
            // is vetted after — but two spellings resolving to one file is not
            // something a path should allow.
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The path is already decoded and already vetted by [`safe_rel`].
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
    let resolved = tokio::fs::canonicalize(root.join(path)).await.ok()?;
    if !resolved.starts_with(root) {
        return None;
    }
    tokio::fs::read(resolved).await.ok()
}

async fn spa_response(dir: &WebDir, path: &str) -> Response {
    let Some(data) = load(dir, path).await else {
        return shell_or_404(dir, path).await;
    };
    served(data, path)
}

/// The fallback half: a build artefact is a 404, anything else is the shell.
async fn shell_or_404(dir: &WebDir, path: &str) -> Response {
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
    if path.starts_with("assets/") {
        return (StatusCode::NOT_FOUND, "no such asset in this build").into_response();
    }
    // Client-side routes fall back to the SPA shell.
    match load(dir, "index.html").await {
        Some(data) => served(data, "index.html"),
        // With a directory, a missing shell is not a build without a UI —
        // `resolve_dir` refused that at startup. It is a bundle being
        // rebuilt underneath a running hub, which is the whole point of
        // the flag: `vite build` clears `dist` and writes it again. Saying
        // "not embedded in this build" there blames the binary for a state
        // that lasts a second, so this says the true thing and says it
        // with a status a client will retry.
        None if dir.is_some() => (
            StatusCode::SERVICE_UNAVAILABLE,
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            "the web bundle is being rebuilt",
        )
            .into_response(),
        None => (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            NO_WEB_UI,
        )
            .into_response(),
    }
}

/// One body, with the type and the cache policy its NAME implies.
fn served(data: Vec<u8>, path: &str) -> Response {
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
