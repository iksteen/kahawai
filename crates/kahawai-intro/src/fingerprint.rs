//! Chromaprint points for a time window.
//!
//! intro-skipper gets these from `ffmpeg -f chromaprint -fp_format raw`, which
//! is the same algorithm `fpcalc -raw` prints. We use `rusty-chromaprint`, a
//! port of the same library, with `preset_test2` — the algorithm both of those
//! default to. Feeding it the file's native rate and channel count is
//! deliberate: Chromaprint's own resampler is part of the fingerprint, and
//! resampling before it would produce points that no longer compare.

use anyhow::{Context, Result};
use rusty_chromaprint::{Configuration, Fingerprinter};

use crate::decode::{self, Media};

/// Fingerprint `[start, end)` of a media file.
pub fn fingerprint(media: &Media, start: f64, end: f64) -> Result<Vec<u32>> {
    let window = decode::audio_window(media, start, end)?;
    fingerprint_samples(&window)
}

pub fn fingerprint_samples(window: &decode::AudioWindow) -> Result<Vec<u32>> {
    let mut printer = Fingerprinter::new(&Configuration::preset_test2());
    printer
        .start(window.rate, window.channels)
        .map_err(|e| {
            anyhow::anyhow!(
                "chromaprint rejected {} Hz / {} ch: {e}",
                window.rate,
                window.channels
            )
        })
        .context("starting the fingerprinter")?;
    printer.consume(&window.samples);
    printer.finish();
    Ok(printer.fingerprint().to_vec())
}
