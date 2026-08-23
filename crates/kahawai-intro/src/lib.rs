//! Intro and end-credits detection.
//!
//! A replication of what the Jellyfin plugin
//! [intro-skipper](https://github.com/intro-skipper/intro-skipper) does, on
//! Kahawai's own decode stack: fingerprint the openings of a season's episodes,
//! find the stretch of audio they share, and find the credits either the same
//! way or by hunting backwards for black frames.
//!
//! The algorithms are ported deliberately faithfully — the same constants, the
//! same tie-breaks — because the point is that the two can be *compared*, one
//! against the other, on the same files. `docs/intro-detection-plan.md` has the
//! plan and the measurement design; `docs/intro-detection-results.md` has what
//! the comparison actually said.
//!
//! Blocking, GStreamer-backed: call it from `spawn_blocking` in async contexts,
//! as with `kahawai-media`.

pub mod blackframe;
pub mod chroma;
pub mod decode;
pub mod fingerprint;
pub mod season;
pub mod silence;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Extensions worth opening. Deliberately short: this walks a season directory
/// someone pointed at, it is not a library scanner.
// Video only: an `mp3` here once let a season folder's theme.mp3 join the
// analysis as an "episode" — permanently unreadable to the black-frame pass,
// and a pairwise match for every episode that shares its theme.
const MEDIA_EXTENSIONS: &[&str] = &["mkv", "mp4", "m4v", "avi", "ts", "webm", "mov", "mpg"];

/// Every media file directly inside `dir`, in filename order — which for any
/// sane naming is episode order.
pub fn episodes_in(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| MEDIA_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
                    .unwrap_or(false)
        })
        .collect();
    files.sort();
    Ok(files)
}

/// Print the raw Chromaprint points of one window, one per line — what the
/// comparison rig feeds to both implementations, and what `fpcalc -raw` prints
/// for the same file.
pub fn print_fingerprint(path: &Path, window: (f64, f64)) -> Result<()> {
    let points = fingerprint::fingerprint(&path.into(), window.0, window.1)?;
    let lines: Vec<String> = points.iter().map(|p| p.to_string()).collect();
    println!("{}", lines.join("\n"));
    Ok(())
}

/// `kahawai intro`: analyze a season directory (or the files named) and print
/// the segments.
pub fn run(paths: &[PathBuf], cfg: &season::Config, json: bool) -> Result<()> {
    let files = match paths {
        [one] if one.is_dir() => episodes_in(one)?,
        [] => bail!("give me a season directory or a list of files"),
        many => many.to_vec(),
    };
    if files.is_empty() {
        bail!("no media files found");
    }

    let report = season::analyze_paths(&files, cfg)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    for episode in &report.episodes {
        let span = |r: Option<chroma::Range>| match r {
            Some(r) => format!("{:8.2} → {:8.2}", r.start, r.end),
            // The same 19 columns as the arm above, or every label after a
            // boundary-less episode shifted by one against its neighbours.
            None => format!("{:^19}", "-"),
        };
        // Recap included — it was computed either way — and an episode whose
        // reads failed says UNREADABLE: printed as bare dashes, it was
        // indistinguishable from "analysed, nothing found", which is the
        // exact confusion the flag exists to prevent.
        println!(
            "{:<50} recap {}  intro {}  credits {}{}{}",
            elide(&episode.name, 50),
            span(episode.recap),
            span(episode.intro),
            span(episode.credits),
            episode
                .credits_source
                .map(|source| format!(" {source}"))
                .unwrap_or_default(),
            if episode.unreadable {
                "  UNREADABLE"
            } else {
                ""
            },
        );
    }
    println!(
        "{} episodes in {:.1}s",
        report.episodes.len(),
        report.seconds
    );
    Ok(())
}

fn elide(s: &str, width: usize) -> String {
    // Chars, not bytes: subtracting one from a BYTE index landed inside the
    // multibyte character before it and panicked mid-report, after a whole
    // season's analysis, on any 51-char name with an accent at the cut.
    match s.char_indices().nth(width) {
        None => s.to_string(),
        Some(_) => {
            let kept: String = s.chars().take(width.saturating_sub(1)).collect();
            format!("{kept}…")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::elide;

    #[test]
    fn elide_cuts_characters_not_bytes() {
        // 49 ASCII chars, then a two-byte char at the cut point: the old
        // byte-index arithmetic panicked mid-report, after a whole season
        // had been analysed.
        let name = format!("{}é and more", "x".repeat(49));
        assert_eq!(elide(&name, 50).chars().count(), 50);
        assert_eq!(elide("short", 50), "short");
        assert_eq!(elide("exactly-ten", 5), "exac…");
    }
}
