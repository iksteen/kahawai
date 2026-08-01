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
