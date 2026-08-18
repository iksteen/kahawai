//! Print the keyframe timestamps in a window: `keyframe_probe FILE START END`.
//!
//! Pairs with `introref keyframes`. An intro's end is snapped to the nearest of
//! these, so a list that differs moves the boundary.

use anyhow::{Context, Result};
use kahawai_intro::decode;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .context("usage: keyframe_probe FILE START END")?;
    let start: f64 = args.next().context("START")?.parse()?;
    let end: f64 = args.next().context("END")?.parse()?;

    for time in decode::keyframes_window(&std::path::Path::new(&path).into(), start, end)? {
        println!("{time:.3}");
    }
    Ok(())
}
