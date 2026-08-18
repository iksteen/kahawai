//! Print the black-pixel share of every frame in a window:
//! `luma_probe FILE START END [THRESHOLD]`.
//!
//! The video counterpart of the fingerprint comparison: run it beside
//! `ffmpeg -vf blackframe=amount=0:threshold=28` over the same window and the
//! two black-frame detectors can be compared frame by frame, instead of only
//! through the binary search that consumes them.

use anyhow::{Context, Result};
use kahawai_intro::decode;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .context("usage: luma_probe FILE START END [THRESHOLD]")?;
    let start: f64 = args.next().context("START")?.parse()?;
    let end: f64 = args.next().context("END")?.parse()?;
    let threshold: u8 = args.next().unwrap_or_else(|| "28".into()).parse()?;

    for frame in decode::luma_window(&std::path::Path::new(&path).into(), start, end, threshold)? {
        println!(
            "{:.3} {:.1} {:.1}",
            frame.time, frame.black_percentage, frame.mean_luma
        );
    }
    Ok(())
}
