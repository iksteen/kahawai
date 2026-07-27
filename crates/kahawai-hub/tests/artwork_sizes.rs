//! HUB-12: named artwork sizes, and the one thing the artwork cache is
//! allowed to delete.
//!
//! OPS-6 says caches here are not evicted, and this does not change that:
//! nothing is dropped for being big. What IS dropped, at startup only, is
//! a derivative that can never be served again — its size left the list,
//! or the original it was made from is gone. Since that code removes
//! files, it gets a test that says exactly which ones.

use std::path::Path;
use std::sync::Arc;

use kahawai_hub::artwork::{Artwork, SIZES};

fn touch(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"x").unwrap();
}

fn open(dir: &Path) -> Artwork {
    let enricher = Arc::new(kahawai_hub::enrich::Enricher::new(dir.to_path_buf()));
    // The sweep runs on construction — that IS startup for this cache.
    Artwork::new(dir.to_path_buf(), enricher)
}

#[test]
fn startup_drops_retired_sizes_and_orphans_but_nothing_else() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    let (live_name, live_px) = SIZES[0];
    let live = dir.join(format!("size-{live_name}-{live_px}"));

    // Two originals, and derivatives of each at the live size.
    touch(&dir.join("aaaa000000000001"));
    touch(&dir.join("tmdb-bbbb000000000002"));
    touch(&live.join("aaaa000000000001"));
    touch(&live.join("tmdb-bbbb000000000002"));
    // A derivative whose original was never there.
    touch(&live.join("cccc000000000003"));
    // A size that is no longer in the list, and one whose pixel count
    // changed — the directory name carries the number, so a re-numbered
    // size is a different directory and this covers both.
    touch(&dir.join("size-tiny-16/aaaa000000000001"));
    touch(&dir.join(format!("size-{live_name}-{}", live_px + 1)).join("aaaa000000000001"));

    open(dir);

    assert!(dir.join("aaaa000000000001").exists(), "originals are never touched");
    assert!(dir.join("tmdb-bbbb000000000002").exists());
    assert!(live.join("aaaa000000000001").exists(), "a live size with its original stays");
    assert!(live.join("tmdb-bbbb000000000002").exists());
    assert!(
        !live.join("cccc000000000003").exists(),
        "a copy whose original is gone can never be served"
    );
    assert!(!dir.join("size-tiny-16").exists(), "a retired size goes wholesale");
    assert!(
        !dir.join(format!("size-{live_name}-{}", live_px + 1)).exists(),
        "re-numbering a size retires the old pixel count"
    );
}

/// The sweep must not wander outside what it owns. Anything that is not a
/// `size-` directory belongs to somebody else.
#[test]
fn startup_leaves_everything_that_is_not_a_size_directory_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    touch(&dir.join("dddd000000000004"));
    touch(&dir.join("notes.txt"));
    touch(&dir.join("subdir/whatever"));

    open(dir);

    assert!(dir.join("dddd000000000004").exists());
    assert!(dir.join("notes.txt").exists());
    assert!(dir.join("subdir/whatever").exists());
}

/// A cache directory that does not exist yet is the normal first-run
/// case, not an error.
#[test]
fn startup_on_a_cache_that_does_not_exist_is_fine() {
    let tmp = tempfile::tempdir().unwrap();
    open(&tmp.path().join("not-created-yet"));
}
