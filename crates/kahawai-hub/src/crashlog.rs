//! Worker crash logs, kept where a human can read them.
//!
//! A pipeline worker that aborts — a Rust panic in a GStreamer callback
//! cannot unwind, so it takes the process — writes the only useful
//! evidence to its stderr: the panic message with file and line. That
//! stderr lives in the session's scratch dir as `worker.log`, and every
//! one of the three things that could delete it does:
//!
//!   * the TC-6 retry wipes the session dir before its second attempt,
//!   * the whole scratch root is cleared at hub startup,
//!   * teardown removes the dir.
//!
//! So the window in which the file exists is milliseconds, and the
//! error text that survives quotes only its LAST FOUR LINES — which for
//! a panic are backtrace frames, not the message. That is exactly what
//! reached us from the field: unresolved addresses and no cause. This
//! module copies the log aside the moment a worker fails, from the hub's
//! own workers and from satellites (which ship theirs over the link,
//! since their copy dies the same way).
//!
//! Retention is bounded, unlike the caches (OPS-6 keeps those because
//! they are expensive to rebuild). A crash log is cheap, only the recent
//! ones are diagnostic, and a crash LOOP must not fill the disk it is
//! reported on — so the newest `KEEP` survive and older ones are pruned.
//!
//! # Session bundles (OPS-10)
//!
//! `crashes/` holds a FAILED worker's stderr. `session-logs/` holds a
//! bundle for every session that merely ENDS — which is the case a
//! crash log cannot cover, because a session that hangs never fails and
//! its evidence is deleted with the run dir the moment it is torn down.
//!
//! Two differences from the crash store, both deliberate:
//!
//! * **The item id is in the filename** (`{unix}-{item}-{session}.log`).
//!   That is what makes "logs for the last session of this item" a glob
//!   rather than a schema change, which matters because the sessions
//!   themselves are ephemeral and leave no row behind.
//! * **The cut keeps HEAD and TAIL.** A crash's message is at the end,
//!   so `tail_bytes` is right for it. A hang's evidence is at the
//!   START — the plan, the caps negotiation, which encoder was chosen —
//!   and measured bundles run ~27 KB against a 256 KB cap, so the cut
//!   only ever fires on a warning storm, where both ends beat one.

use std::path::{Path, PathBuf};

/// How many crash logs to keep. Enough to cover a session's retry pair
/// and a few earlier failures; small enough that a crash loop cannot
/// grow without bound.
const KEEP: usize = 40;

/// Cap per log. GStreamer debug output can be enormous; the panic and
/// its backtrace are at the END, so the tail is what matters.
pub const MAX_BYTES: usize = 256 * 1024;

pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("crashes")
}

/// Keep `body` as the crash record for one failed worker. `origin` is
/// the module id of the satellite that ran it, or "local" for the hub's
/// own worker. Best-effort by construction: a diagnostic that fails
/// must never be what breaks the failure path.
pub fn store(data_dir: &Path, session_id: &str, origin: &str, body: &str) {
    if body.trim().is_empty() {
        return;
    }
    let dir = dir(data_dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Sortable by name, and unique per attempt: a retry writes its own.
    let name = format!("{stamp}-{origin}-{session_id}.log");
    let tail = tail_bytes(body, MAX_BYTES);
    if std::fs::write(dir.join(&name), tail).is_ok() {
        tracing::warn!(
            crash_log = %dir.join(&name).display(),
            "worker failed; its stderr was kept here"
        );
    }
    prune(&dir);
}

/// The last `max` bytes, cut at a line boundary so the file starts
/// cleanly.
fn tail_bytes(body: &str, max: usize) -> &str {
    if body.len() <= max {
        return body;
    }
    let cut = body.len() - max;
    match body[cut..].find('\n') {
        Some(nl) => &body[cut + nl + 1..],
        None => &body[cut..],
    }
}

fn prune(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "log"))
        .collect();
    if logs.len() <= KEEP {
        return;
    }
    // Names lead with a unix stamp, so lexical order is chronological.
    logs.sort();
    for old in &logs[..logs.len() - KEEP] {
        let _ = std::fs::remove_file(old);
    }
}

/// Where session bundles live (OPS-10).
pub fn bundle_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("session-logs")
}

/// Keep one session's diagnostics. `item_id` rides in the filename so a
/// later "logs for this item" lookup is a directory glob.
pub fn store_bundle(data_dir: &Path, item_id: &str, session_id: &str, body: &str) {
    if body.trim().is_empty() {
        return;
    }
    let dir = bundle_dir(data_dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = format!("{stamp}-{item_id}-{session_id}.log");
    if std::fs::write(dir.join(&name), head_and_tail(body, MAX_BYTES)).is_ok() {
        tracing::debug!(bundle = %dir.join(&name).display(), "session diagnostics kept");
    }
    prune(&dir);
}

/// The newest bundle for an item, whoever played it — the point is
/// debugging somebody else's report.
pub fn newest_for_item(data_dir: &Path, item_id: &str) -> Option<PathBuf> {
    newest_matching(&bundle_dir(data_dir), &format!("-{item_id}-"))
}

/// A specific session's bundle, if one was kept.
pub fn bundle_for_session(data_dir: &Path, session_id: &str) -> Option<PathBuf> {
    newest_matching(&bundle_dir(data_dir), &format!("-{session_id}.log"))
}

/// Names lead with a unix stamp, so lexical order is chronological and
/// the last match is the newest.
fn newest_matching(dir: &Path, needle: &str) -> Option<PathBuf> {
    let mut hits: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(needle))
        })
        .collect();
    hits.sort();
    hits.pop()
}

/// Both ends of `body`, with the middle replaced by a marker saying how
/// much went. See the module doc for why a bundle is cut this way and a
/// crash log is not.
fn head_and_tail(body: &str, max: usize) -> String {
    if body.len() <= max {
        return body.to_string();
    }
    let half = max / 2;
    let head_end = body[..half].rfind('\n').map_or(half, |i| i + 1);
    let tail_start = body.len() - half;
    let tail_start = body[tail_start..]
        .find('\n')
        .map_or(tail_start, |i| tail_start + i + 1);
    let dropped = tail_start - head_end;
    format!(
        "{}\n... {dropped} bytes omitted from the middle ...\n\n{}",
        &body[..head_end],
        &body[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_tail_at_a_line_boundary() {
        let body = (0..500).map(|i| format!("line {i}\n")).collect::<String>();
        let t = tail_bytes(&body, 100);
        assert!(t.len() <= 100);
        assert!(t.starts_with("line "), "cut mid-line: {t:?}");
        assert!(t.ends_with("line 499\n"), "kept the wrong end");
        // Small bodies pass through whole — a panic is usually short.
        assert_eq!(tail_bytes("boom\n", 100), "boom\n");
    }

    #[test]
    fn a_bundle_keeps_both_ends_and_is_found_by_item() {
        let d = tempfile::tempdir().unwrap();
        // Small bodies pass through whole — the cut is for pathology.
        store_bundle(d.path(), "item-a", "sess-1", "hello\nworld\n");
        let p = newest_for_item(d.path(), "item-a").expect("found by item");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello\nworld\n");
        assert!(bundle_for_session(d.path(), "sess-1").is_some());
        // Another item's bundle must not answer for this one.
        assert!(newest_for_item(d.path(), "item-b").is_none());

        // A later bundle for the same item wins.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        store_bundle(d.path(), "item-a", "sess-2", "newer\n");
        let p = newest_for_item(d.path(), "item-a").unwrap();
        assert!(p.to_string_lossy().contains("sess-2"), "got {p:?}");

        // The cut keeps the START, which a crash log deliberately drops.
        let big: String = (0..40_000).map(|i| format!("line {i}\n")).collect();
        assert!(big.len() > MAX_BYTES);
        let cut = head_and_tail(&big, MAX_BYTES);
        assert!(cut.len() <= MAX_BYTES + 200, "cut to {}", cut.len());
        assert!(cut.starts_with("line 0\n"), "lost the head");
        assert!(cut.trim_end().ends_with("line 39999"), "lost the tail");
        assert!(cut.contains("omitted from the middle"));
    }

    #[test]
    fn stores_and_prunes_but_never_panics() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..KEEP + 15 {
            store(
                d.path(),
                &format!("sess{i}"),
                "local",
                "panicked at remux.rs:1\n",
            );
        }
        let n = std::fs::read_dir(dir(d.path())).unwrap().count();
        assert!(n <= KEEP, "kept {n}, cap is {KEEP}");
        // Empty bodies write nothing; unwritable roots are survivable.
        store(d.path(), "s", "local", "   \n");
        store(Path::new("/proc/nonexistent/nope"), "s", "local", "x");
    }
}
