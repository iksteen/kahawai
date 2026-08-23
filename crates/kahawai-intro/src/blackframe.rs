//! Credits by black frames: binary search backwards from the end of a file for
//! the first frame that is mostly black.
//!
//! A port of `Analyzers/BlackFrameAnalyzer.cs`, including its habit of widening
//! the bracket when a probe lands on a limit. The probing itself is a closure so
//! the search can be tested without decoding anything — and so the comparison
//! rig can drive it from a recorded signal.

use anyhow::Result;

use crate::chroma::Range;
use crate::decode::{self, Media};

/// Their `BlackFrameMinimumPercentage` / `BlackFrameThreshold` /
/// `MinimumCreditsDuration`, plus the 4 s the binary search stops at.
#[derive(Clone, Copy, Debug)]
pub struct BlackFrameParams {
    pub minimum_percentage: f64,
    pub threshold: u8,
    pub minimum_credits_duration: f64,
    pub maximum_error: f64,
}

impl Default for BlackFrameParams {
    fn default() -> Self {
        Self {
            minimum_percentage: 85.0,
            threshold: 28,
            minimum_credits_duration: kahawai_core::segments::BLACK_CREDITS_MIN_MS as f64 / 1000.0,
            maximum_error: 4.0,
        }
    }
}

/// Probe a window and report, in seconds *from the window start*, the black
/// frames inside it.
pub trait BlackProbe {
    fn probe(&mut self, start: f64, end: f64) -> Result<Vec<f64>>;
}

impl<F: FnMut(f64, f64) -> Result<Vec<f64>>> BlackProbe for F {
    fn probe(&mut self, start: f64, end: f64) -> Result<Vec<f64>> {
        self(start, end)
    }
}

/// A probe backed by the real decoder.
pub struct DecodeProbe<'a> {
    pub media: &'a Media,
    pub params: BlackFrameParams,
}

impl BlackProbe for DecodeProbe<'_> {
    fn probe(&mut self, start: f64, end: f64) -> Result<Vec<f64>> {
        let frames = decode::luma_window(self.media, start, end, self.params.threshold)?;
        Ok(frames
            .into_iter()
            .filter(|f| f.black_percentage >= self.params.minimum_percentage)
            .map(|f| (f.time - start).max(0.0))
            .collect())
    }
}

/// Walk backwards from the end in `2 × minimum_credits_duration` steps until a
/// one-second window has fewer than three black frames, and return that
/// distance-from-the-end. Their `FindSearchStartAsync`: it keeps the binary
/// search out of a black stretch that is still part of the episode.
pub fn find_search_start(
    duration: f64,
    credits_start: f64,
    params: &BlackFrameParams,
    probe: &mut impl BlackProbe,
) -> Result<f64> {
    let mut search_start = 3.0 * params.minimum_credits_duration;
    let max_search_start = duration - credits_start;
    let step = 2.0 * params.minimum_credits_duration;

    while search_start < max_search_start {
        let scan_time = duration - search_start;
        if probe.probe(scan_time - 1.0, scan_time)?.len() < 3 {
            return Ok(search_start);
        }
        search_start += step;
    }
    Ok(max_search_start)
}

/// The credits segment, or `None` when no black frame was found in range.
pub fn find_credits(
    duration: f64,
    credits_start: f64,
    initial_start: f64,
    params: &BlackFrameParams,
    probe: &mut impl BlackProbe,
) -> Result<Option<Range>> {
    let search_distance = 2.0 * params.minimum_credits_duration;
    let mut upper_limit = initial_start.min(duration - credits_start);
    let mut lower_limit = (initial_start - search_distance).max(params.minimum_credits_duration);

    // Both are distances from the end of the file, so "start" is the later time.
    let mut search_start = upper_limit;
    let mut search_end = lower_limit;
    let mut first_black_frame: Option<f64> = None;

    while search_start - search_end > params.maximum_error {
        let midpoint = (search_start + search_end) / 2.0;
        let scan_time = duration - midpoint;
        let black = probe.probe(scan_time, scan_time + 2.0)?;

        if black.is_empty() {
            search_start = midpoint - 2.0;
            if midpoint - lower_limit < params.maximum_error {
                lower_limit =
                    (lower_limit - 0.5 * search_distance).max(params.minimum_credits_duration);
                search_end = lower_limit;
            }
        } else {
            search_end = midpoint;
            first_black_frame = Some(black[0] + scan_time);
            if upper_limit - midpoint < params.maximum_error {
                upper_limit = (upper_limit + 0.5 * search_distance).min(duration - credits_start);
                search_start = upper_limit;
            }
        }
    }

    Ok(match first_black_frame {
        Some(t) if t > 0.0 => Some(Range::new(t, duration)),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An episode whose credits start at `credits_at`: every frame from there on
    /// is black, at 25 fps.
    fn synthetic(credits_at: f64) -> impl BlackProbe {
        move |start: f64, end: f64| -> Result<Vec<f64>> {
            let mut times = Vec::new();
            let mut t = start.max(credits_at);
            while t < end {
                times.push(t - start);
                t += 0.04;
            }
            Ok(times)
        }
    }

    #[test]
    fn binary_search_lands_on_the_credits() {
        let params = BlackFrameParams::default();
        let (duration, credits_at) = (1440.0, 1300.0);
        let mut probe = synthetic(credits_at);

        let start = find_search_start(duration, duration - 450.0, &params, &mut probe).unwrap();
        let found = find_credits(duration, duration - 450.0, start, &params, &mut probe)
            .unwrap()
            .expect("credits");

        assert!(
            (found.start - credits_at).abs() <= params.maximum_error,
            "{found:?} vs {credits_at}"
        );
        assert_eq!(found.end, duration);
    }

    #[test]
    fn an_episode_without_black_frames_has_no_credits() {
        let params = BlackFrameParams::default();
        let mut probe = |_: f64, _: f64| Ok(Vec::new());
        let found = find_credits(1440.0, 990.0, 45.0, &params, &mut probe).unwrap();
        assert!(found.is_none());
    }
}
