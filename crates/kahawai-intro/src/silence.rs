//! Silence detection, the part of ffmpeg's `silencedetect` that intro-skipper
//! uses: runs where every channel stays below a noise floor for long enough.
//!
//! Used to pull the end of an intro back to the pause after the theme, exactly
//! as `Analyzers/TimeAdjustmentHelper.cs` does.

use anyhow::Result;

use crate::chroma::Range;
use crate::decode::{self, AudioWindow, Media};

/// Silence runs inside `[start, end)` of a file.
///
/// `noise_db` is intro-skipper's `SilenceDetectionMaximumNoise` (-50 dBFS) and
/// `minimum` its filter argument (0.1 s); the caller applies the longer
/// `SilenceDetectionMinimumDuration` afterwards, as they do.
pub fn detect(
    media: &Media,
    start: f64,
    end: f64,
    noise_db: f64,
    minimum: f64,
) -> Result<Vec<Range>> {
    let window = decode::audio_window(media, start, end)?;
    Ok(detect_samples(&window, start, noise_db, minimum))
}

pub fn detect_samples(
    window: &AudioWindow,
    offset: f64,
    noise_db: f64,
    minimum: f64,
) -> Vec<Range> {
    // Floats, not a truncated integer: below about -90 dB the integer
    // threshold became 0 and even pure digital silence read as "loud" —
    // the knob silently stopped meaning anything at the 16-bit floor.
    let threshold = 10f64.powf(noise_db / 20.0) * f64::from(i16::MAX);
    let channels = window.channels.max(1) as usize;
    let rate = window.rate as f64;

    let mut ranges = Vec::new();
    let mut run_start: Option<usize> = None;
    let frames = window.samples.len() / channels;

    for frame in 0..frames {
        let quiet = window.samples[frame * channels..(frame + 1) * channels]
            .iter()
            .all(|s| f64::from(*s).abs() < threshold);
        match (quiet, run_start) {
            (true, None) => run_start = Some(frame),
            (false, Some(begin)) => {
                push_run(&mut ranges, begin, frame, rate, offset, minimum);
                run_start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = run_start {
        push_run(&mut ranges, begin, frames, rate, offset, minimum);
    }
    ranges
}

fn push_run(out: &mut Vec<Range>, begin: usize, end: usize, rate: f64, offset: f64, minimum: f64) {
    let (start, stop) = (begin as f64 / rate, end as f64 / rate);
    if stop - start >= minimum {
        out.push(Range::new(offset + start, offset + stop));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(samples: Vec<i16>) -> AudioWindow {
        AudioWindow {
            rate: 1000,
            channels: 1,
            samples,
        }
    }

    #[test]
    fn finds_a_gap_between_two_tones() {
        // 0.5 s loud, 0.4 s quiet, 0.5 s loud.
        let mut samples = vec![20_000i16; 500];
        samples.extend(std::iter::repeat_n(1i16, 400));
        samples.extend(std::iter::repeat_n(-20_000i16, 500));

        let found = detect_samples(&window(samples), 10.0, -50.0, 0.33);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!((found[0].start - 10.5).abs() < 1e-9, "{found:?}");
        assert!((found[0].end - 10.9).abs() < 1e-9, "{found:?}");
    }

    #[test]
    fn ignores_a_gap_that_is_too_short() {
        let mut samples = vec![20_000i16; 500];
        samples.extend(std::iter::repeat_n(0i16, 100));
        samples.extend(std::iter::repeat_n(20_000i16, 500));
        assert!(detect_samples(&window(samples), 0.0, -50.0, 0.33).is_empty());
    }
}
