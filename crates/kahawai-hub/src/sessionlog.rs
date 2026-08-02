//! One session's diagnostics, kept where a human can read them —
//! **one directory, whatever went wrong**.
//!
//! `<data_dir>/session-logs/{unix}-{item}-{session}.log`, newest `KEEP`.
//!
//! There is deliberately no second store for crashes. Splitting them
//! meant knowing which kind of failure you were chasing BEFORE you knew
//! anything about it, which is backwards: the reason to open a log is
//! that you do not yet know whether the session failed, hung, or was
//! fine. Retention is a number you can raise; two folders is a tax on
//! every future investigation.
//!
//! # Why this exists at all
//!
//! Everything the pipeline knows lives in the session's run dir, and
//! every one of the three things that could delete it does: the TC-6
//! retry wipes it before its second attempt, the scratch root is
//! cleared at hub startup, and teardown removes it. The window is
//! milliseconds. A satellite's copy dies the same way, which is why it
//! ships its half over the link rather than keeping it.
//!
//! # What lands here, and when
//!
//! * **Session end** — the case a crash store could never cover: a
//!   session that HANGS never fails, so nothing else would fire.
//! * **Failure** — both the hub's own worker exiting at start and a
//!   satellite's `SessionError`. A session that fails to start is never
//!   registered as active, so its bundle is the only trace it existed.
//! * **On demand** — the download button, while a session is live.
//!
//! # The item id is in the FILENAME
//!
//! That is what makes "the last session for this item" a directory glob
//! rather than a schema change, and sessions are ephemeral — they leave
//! no row to join against.
//!
//! # The cut keeps HEAD and TAIL
//!
//! A panic's message is at the end; a hang's evidence is at the start —
//! the plan, the caps negotiation, which encoder was chosen. Measured
//! bundles run ~27 KB against a 256 KB cap, so the cut only ever fires
//! on a warning storm, where keeping both ends beats keeping one.

use std::path::{Path, PathBuf};

/// How many sessions to keep. A knob: raise it when a busy hub starts
/// evicting a failure before anyone looks. Bounded at all because a
/// crash LOOP must not fill the disk it is being reported on.
const KEEP: usize = 40;

/// Cap per bundle. GStreamer debug output can be enormous; measured
/// bundles are ~27 KB, so this bounds pathology rather than truncating
/// normal use.
pub const MAX_BYTES: usize = 256 * 1024;

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

/// The one directory. See the module doc.
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("session-logs")
}

/// Keep one session's diagnostics. `item_id` rides in the filename so a
/// later "logs for this item" lookup is a directory glob.
pub fn store(data_dir: &Path, item_id: &str, session_id: &str, body: &str) {
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
    let name = format!("{stamp}-{item_id}-{session_id}.log");
    if std::fs::write(dir.join(&name), head_and_tail(body, MAX_BYTES)).is_ok() {
        tracing::debug!(bundle = %dir.join(&name).display(), "session diagnostics kept");
    }
    prune(&dir);
}

/// The newest bundle for an item, whoever played it — the point is
/// debugging somebody else's report.
pub fn newest_for_item(data_dir: &Path, item_id: &str) -> Option<PathBuf> {
    newest_matching(&dir(data_dir), &format!("-{item_id}-"))
}

/// A specific session's bundle, if one was kept.
pub fn for_session(data_dir: &Path, session_id: &str) -> Option<PathBuf> {
    newest_matching(&dir(data_dir), &format!("-{session_id}.log"))
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
    fn keeps_both_ends_prunes_and_is_found_by_item() {
        let d = tempfile::tempdir().unwrap();
        // Small bodies pass through whole — the cut is for pathology.
        store(d.path(), "item-a", "sess-1", "hello\nworld\n");
        let p = newest_for_item(d.path(), "item-a").expect("found by item");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello\nworld\n");
        assert!(for_session(d.path(), "sess-1").is_some());
        // Another item's bundle must not answer for this one.
        assert!(newest_for_item(d.path(), "item-b").is_none());

        // A later bundle for the same item wins.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        store(d.path(), "item-a", "sess-2", "newer\n");
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
}
