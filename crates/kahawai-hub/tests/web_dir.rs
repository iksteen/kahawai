//! `--web-dir` serves a bundle this binary does not carry.
//!
//! The embedded assets are empty in a Rust-only checkout, which is what makes
//! this measurable: every byte the directory source returns is a byte the
//! embedded source could not have produced. The SPA rules are asserted against
//! the directory too, because they are the part that would silently diverge —
//! an `assets/` miss must stay a 404 rather than becoming the shell, or a hub
//! upgrade breaks every open tab.
//!
//! The traversal cases are the reason this file exists at all: a URI path is
//! the only untrusted string that reaches a filesystem here.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

/// Through `resolve_dir`, as the hub does. The containment check compares
/// resolved paths, so a router handed an unresolved root would refuse
/// everything — on a box where the temp dir is reached through a symlink, a
/// helper that skipped this would fail every test for the wrong reason.
async fn get(dir: &std::path::Path, path: &str) -> (StatusCode, String) {
    let response = kahawai_hub::web::router(kahawai_hub::web::resolve_dir(dir).ok())
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn bundle() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), "<!doctype html>from disk").unwrap();
    std::fs::create_dir(dir.path().join("assets")).unwrap();
    std::fs::write(dir.path().join("assets/app-abc123.js"), "export {}").unwrap();
    // Reachable, and meant to be: pointing the hub at a directory serves
    // everything readable under it. Here to pin that down — an earlier draft
    // of this file claimed `safe_rel` was an allowlist of build artefacts,
    // which would invite pointing `--web-dir` at `web/` instead of `web/dist`
    // and publishing `.env` and `src/` on an unauthenticated route.
    std::fs::write(dir.path().join("plain.txt"), "served").unwrap();
    dir
}

#[tokio::test]
async fn serves_the_directory_rather_than_the_embedded_bundle() {
    let dir = bundle();
    assert_eq!(
        get(dir.path(), "/app/").await,
        (StatusCode::OK, "<!doctype html>from disk".into())
    );
    assert_eq!(
        get(dir.path(), "/app/assets/app-abc123.js").await,
        (StatusCode::OK, "export {}".into())
    );
}

#[tokio::test]
async fn client_routes_fall_back_to_the_shell_but_assets_do_not() {
    let dir = bundle();
    let (status, body) = get(dir.path(), "/app/library/films/item/7").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "<!doctype html>from disk");

    let (status, _) = get(dir.path(), "/app/assets/gone-999.js").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_request_path_cannot_climb_out_of_the_directory() {
    let dir = bundle();
    let outside = dir.path().parent().unwrap().join("outside.txt");
    std::fs::write(&outside, "escaped").unwrap();

    // Anything that is not a plain descending name is refused before it
    // reaches the filesystem, so a refusal is indistinguishable from a miss:
    // these answer the SPA shell, or the `assets/` 404 for the one that is
    // spelled like a build artefact. Never a file.
    for path in [
        "/app/../outside.txt",
        "/app/assets/../../outside.txt",
        "/app/%2e%2e/outside.txt",
        "/app/secret.txt%00.js",
        "/app//etc/hostname",
    ] {
        let (status, body) = get(dir.path(), path).await;
        assert_ne!(body, "escaped", "{path} escaped the web directory");
        match status {
            StatusCode::OK => assert_eq!(body, "<!doctype html>from disk", "{path}"),
            StatusCode::NOT_FOUND => {}
            other => panic!("{path} answered {other}"),
        }
    }
    std::fs::remove_file(outside).unwrap();
}

#[tokio::test]
async fn every_readable_file_under_the_directory_is_served() {
    let dir = bundle();
    assert_eq!(
        get(dir.path(), "/app/plain.txt").await,
        (StatusCode::OK, "served".into())
    );
}

/// The case a string filter cannot see. `safe_rel` refuses a `..` in the
/// request; a link inside the bundle is a `..` the request never spells.
#[tokio::test]
async fn a_symlink_out_of_the_directory_does_not_escape_it() {
    let dir = bundle();
    let outside = dir.path().parent().unwrap().join("linked-outside.txt");
    std::fs::write(&outside, "escaped").unwrap();
    std::os::unix::fs::symlink(&outside, dir.path().join("link.txt")).unwrap();
    std::os::unix::fs::symlink(&outside, dir.path().join("assets/link-abc.js")).unwrap();

    let (status, body) = get(dir.path(), "/app/link.txt").await;
    assert_ne!(body, "escaped", "a symlink escaped the web directory");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "<!doctype html>from disk");

    let (status, body) = get(dir.path(), "/app/assets/link-abc.js").await;
    assert_ne!(body, "escaped", "a symlink escaped the web directory");
    assert_eq!(status, StatusCode::NOT_FOUND);
    std::fs::remove_file(outside).unwrap();
}

/// A wrong `--web-dir` is refused at startup, where the mistake was made.
/// Serving it and answering "the web UI was not embedded in this build" is
/// what this replaces: a 200, no log line, and the diagnosis pointed at the
/// binary instead of the flag.
#[test]
fn a_web_dir_that_is_not_a_directory_is_refused() {
    let dir = bundle();
    assert!(kahawai_hub::web::resolve_dir(&dir.path().join("nope")).is_err());
    assert!(kahawai_hub::web::resolve_dir(&dir.path().join("index.html")).is_err());

    // And a good one comes back absolute, so the containment check below has
    // something it can compare against.
    let resolved = kahawai_hub::web::resolve_dir(dir.path()).unwrap();
    assert!(resolved.is_absolute(), "{}", resolved.display());
}
