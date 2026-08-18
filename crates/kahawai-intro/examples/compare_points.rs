//! Run the shared-region search over two files of Chromaprint points:
//! `compare_points lhs.txt rhs.txt`, one unsigned integer per line.
//!
//! The rig's level-2 measurement — the same points go to this and to
//! `introref compare`, so anything that differs in the answer is the search
//! itself and not the decoder.

use anyhow::{Context, Result};
use kahawai_intro::chroma::{self, SearchParams};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let lhs = points(
        &args
            .next()
            .context("usage: compare_points lhs.txt rhs.txt")?,
    )?;
    let rhs = points(
        &args
            .next()
            .context("usage: compare_points lhs.txt rhs.txt")?,
    )?;

    let (l, r) = chroma::compare(&lhs, &rhs, &SearchParams::default());
    println!(
        "{}",
        serde_json::json!({
            "lhs": l.valid().then_some(l),
            "rhs": r.valid().then_some(r),
        })
    );
    Ok(())
}

fn points(path: &str) -> Result<Vec<u32>> {
    std::fs::read_to_string(path)
        .with_context(|| format!("reading {path}"))?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().parse::<u32>().context("not a fingerprint point"))
        .collect()
}
