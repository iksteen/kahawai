//! Print the silence ranges in a window: `silence_probe FILE START END [dBFS]`.
//!
//! The counterpart of `introref silence`: same file, same window, two silence
//! detectors, so a boundary that lands differently can be traced to the signal
//! rather than guessed at.

use anyhow::{Context, Result};
use kahawai_intro::silence;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .context("usage: silence_probe FILE START END [dBFS]")?;
    let start: f64 = args.next().context("START")?.parse()?;
    let end: f64 = args.next().context("END")?.parse()?;
    let noise: f64 = args.next().unwrap_or_else(|| "-50".into()).parse()?;

    for range in silence::detect(&std::path::Path::new(&path).into(), start, end, noise, 0.1)? {
        println!("{:.3} {:.3}", range.start, range.end);
    }
    Ok(())
}
