//! Everything a run directory can say about itself, as text a human can
//! paste into a bug report (OPS-10).
//!
//! Best-effort throughout: a missing file is a missing section, never a
//! failure — this runs on teardown paths and must not be able to break
//! them. The hub stores the result under its data dir; a transcoder ships
//! it over the link at session end and on request.

use std::fmt::Write as _;
use std::path::Path;

/// The bundle for one run. `label` says who gathered it ("hub-local
/// worker", "satellite"): the hub concatenates its own header with a
/// satellite's bundle, and the reader needs to see where one ends.
pub fn gather(label: &str, session_id: &str, dir: &Path) -> String {
    let mut out = String::with_capacity(32 * 1024);
    let _ = writeln!(out, "== {label}: session {session_id}");
    let _ = writeln!(out, "run dir: {}", dir.display());

    let segments = segment_files(dir);
    let _ = writeln!(out, "segments: {}", segments.len());
    if let Some(first) = segments.first() {
        let _ = writeln!(out, "first segment: {}", first_segment_summary(first));
    }
    for name in [
        "start.pos",
        "viewer.pos",
        "pace.json",
        "pace.taken.json",
        "facts.jsonl",
    ] {
        if let Ok(body) = std::fs::read_to_string(dir.join(name)) {
            let _ = writeln!(out, "\n== {name}\n{}", body.trim_end());
        }
    }
    if let Ok(playlist) = std::fs::read_to_string(dir.join("master.m3u8")) {
        let _ = writeln!(
            out,
            "\n== master.m3u8 (tail)\n{}",
            tail_lines(&playlist, 40)
        );
    }
    if let Ok(log) = std::fs::read_to_string(dir.join("worker.log")) {
        let _ = writeln!(out, "\n== worker.log\n{log}");
    }
    out
}

fn segment_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("segment"))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

/// Is the first segment independently decodable? One line, and the
/// whole diagnosis of a wedged session: a player handed a segment with
/// no parameter sets stalls there forever while the worker happily
/// produces minutes more behind it.
///
/// H.264-in-TS only, which is what the start codes below assume. Other
/// pipelines say so rather than being silently skipped.
pub fn first_segment_summary(path: &Path) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return "unreadable".into();
    };
    if !path.extension().is_some_and(|ext| ext == "ts") {
        return format!("{} bytes (not TS; no NAL summary)", bytes.len());
    }
    let (mut sps, mut pps, mut idr) = (false, false, false);
    for window in bytes.windows(4) {
        if window[0] == 0 && window[1] == 0 && window[2] == 1 {
            match window[3] & 0x1f {
                5 => idr = true,
                7 => sps = true,
                8 => pps = true,
                _ => {}
            }
        }
    }
    format!(
        "{} bytes, SPS={sps} PPS={pps} IDR={idr}{}",
        bytes.len(),
        if sps && pps && idr {
            ""
        } else {
            "  <-- NOT independently decodable; a player wedges here"
        }
    )
}

/// The last `n` lines of `body`.
pub fn tail_lines(body: &str, n: usize) -> String {
    let lines: Vec<&str> = body.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}
