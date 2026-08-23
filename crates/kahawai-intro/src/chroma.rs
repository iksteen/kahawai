//! The shared-region search: given two Chromaprint fingerprints, find the
//! stretch of audio they have in common.
//!
//! A port of intro-skipper's `Analyzers/ChromaprintAnalyzer.cs` and
//! `Data/TimeRangeHelpers.cs` (GPL-3.0, read at branch `10.11`), deliberately
//! faithful down to the tie-breaks — the point of the exercise is that the two
//! implementations can be compared, and a "cleaner" search would only tell us
//! that two different algorithms disagree.
//!
//! Pure: no I/O, no GStreamer. Every argument is a fingerprint someone else
//! decoded, which is what lets the comparison rig feed *identical* points to
//! both implementations and isolate the search from the decoder.

use std::collections::{HashMap, HashSet};

/// Seconds of audio per fingerprint point: Chromaprint's 4096-sample frame at
/// 11025 Hz with 2/3 overlap. `Data/ChromaprintConstants.cs`.
pub const SAMPLE_DURATION: f64 = 4096.0 / 11025.0 / 3.0;
const START_SNAP_SECS: f64 = kahawai_core::segments::INTRO_START_SNAP_MS as f64 / 1000.0;

/// Search tuning. Defaults are intro-skipper's, from
/// `Configuration/PluginConfiguration.cs`.
#[derive(Clone, Copy, Debug)]
pub struct SearchParams {
    /// Bits that may differ between two points still called equal (6).
    pub max_point_differences: u32,
    /// How far around a point to look for its twin when collecting candidate
    /// shifts (±2).
    pub inverted_index_shift: i32,
    /// Largest gap inside one contiguous match, in seconds (3.5).
    pub max_time_skip: f64,
    /// Shortest match worth reporting, in seconds (15).
    pub min_region_duration: f64,
}

impl Default for SearchParams {
    fn default() -> Self {
        Self {
            max_point_differences: 6,
            inverted_index_shift: 2,
            max_time_skip: 3.5,
            min_region_duration: kahawai_core::segments::SHARED_REGION_MIN_MS as f64 / 1000.0,
        }
    }
}

/// A stretch of time, in seconds from the start of the fingerprinted window.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Range {
    pub start: f64,
    pub end: f64,
}

impl Range {
    pub fn new(start: f64, end: f64) -> Self {
        Self { start, end }
    }

    pub fn duration(&self) -> f64 {
        self.end - self.start
    }

    /// Their `Segment.Valid`: an end of zero means nothing was found.
    pub fn valid(&self) -> bool {
        self.end > 0.0
    }
}

/// Longest run of timestamps with no gap wider than `max_distance`.
///
/// The tie-break is theirs and is load-bearing: `TimeRangeHelpers.FindContiguous`
/// only replaces the best run on a *strictly* longer one, so the earliest of
/// several equally long runs wins.
pub fn find_contiguous(times: &[f64], max_distance: f64) -> Option<Range> {
    if times.is_empty() {
        return None;
    }
    let mut times = times.to_vec();
    times.sort_by(|a, b| a.partial_cmp(b).expect("timestamps are never NaN"));

    let mut current = Range::new(times[0], times[0]);
    let mut best: Option<Range> = None;

    for pair in times.windows(2) {
        let (current_time, next) = (pair[0], pair[1]);
        if next - current_time <= max_distance {
            current.end = next;
            continue;
        }
        best = Some(match best {
            Some(b) if b.duration() >= current.duration() => b,
            _ => current,
        });
        current = Range::new(next, next);
    }

    Some(match best {
        Some(b) if b.duration() >= current.duration() => b,
        _ => current,
    })
}

/// Point → the *last* index it appeared at. Theirs overwrites on repeat, so a
/// point that recurs is only ever found at its final position.
pub fn inverted_index(fingerprint: &[u32]) -> HashMap<u32, usize> {
    let mut index = HashMap::with_capacity(fingerprint.len());
    for (i, point) in fingerprint.iter().enumerate() {
        index.insert(*point, i);
    }
    index
}

/// Match two fingerprints at one fixed alignment, returning the shared range on
/// each side, or `None` when the overlap is too short to count.
fn contiguous_at_shift(
    lhs: &[u32],
    rhs: &[u32],
    shift: i64,
    params: &SearchParams,
) -> Option<(Range, Range)> {
    let (left_offset, right_offset) = if shift < 0 {
        ((-shift) as usize, 0)
    } else {
        (0, shift as usize)
    };

    let upper = (lhs.len().min(rhs.len()) as i64) - shift.abs();
    let mut lhs_times = Vec::new();
    let mut rhs_times = Vec::new();
    for i in 0..upper.max(0) as usize {
        let (lp, rp) = (i + left_offset, i + right_offset);
        if (lhs[lp] ^ rhs[rp]).count_ones() > params.max_point_differences {
            continue;
        }
        lhs_times.push(lp as f64 * SAMPLE_DURATION);
        rhs_times.push(rp as f64 * SAMPLE_DURATION);
    }

    let left = find_contiguous(&lhs_times, params.max_time_skip)?;
    if left.duration() < params.min_region_duration {
        return None;
    }
    // If the left side had a contiguous run the right side has one too: the
    // timestamps come in pairs.
    let right = find_contiguous(&rhs_times, params.max_time_skip)?;
    Some((left, right))
}

/// Every shared region found at any alignment the inverted indexes suggest.
pub fn search(lhs: &[u32], rhs: &[u32], params: &SearchParams) -> (Vec<Range>, Vec<Range>) {
    let lhs_index = inverted_index(lhs);
    let rhs_index = inverted_index(rhs);

    let mut shifts = HashSet::new();
    for (point, lhs_first) in &lhs_index {
        for i in -params.inverted_index_shift..=params.inverted_index_shift {
            let modified = point.wrapping_add_signed(i);
            if let Some(rhs_first) = rhs_index.get(&modified) {
                shifts.insert(*rhs_first as i64 - *lhs_first as i64);
            }
        }
    }

    let mut lhs_ranges = Vec::new();
    let mut rhs_ranges = Vec::new();
    // Sorted, because HashSet iteration order is seeded per process and both
    // selection rules break exact-duration ties on encounter order: unsorted,
    // the same season could report a different intro start on consecutive
    // runs, and the L2 parity comparison flapped with the hash seed.
    let mut shifts: Vec<i64> = shifts.into_iter().collect();
    shifts.sort_unstable();
    for shift in shifts {
        if let Some((l, r)) = contiguous_at_shift(lhs, rhs, shift, params)
            && l.end > 0.0
            && r.end > 0.0
        {
            lhs_ranges.push(l);
            rhs_ranges.push(r);
        }
    }
    (lhs_ranges, rhs_ranges)
}

/// Which shared region to report when the search finds several.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Select {
    /// Intros and credits: the longest shared stretch of audio.
    #[default]
    Longest,
    /// Recaps: the earliest, because what repeats is a short card at the front
    /// of a "previously on" and the longest match would be the theme behind it.
    Earliest,
}

/// The shared region between two episodes: the longest one found, with a start
/// inside the first 5 seconds pulled back to zero.
///
/// Returns a zero range per side when there is no match, matching their invalid
/// `Segment`.
pub fn compare(lhs: &[u32], rhs: &[u32], params: &SearchParams) -> (Range, Range) {
    compare_with(lhs, rhs, params, Select::Longest)
}

pub fn compare_with(
    lhs: &[u32],
    rhs: &[u32],
    params: &SearchParams,
    select: Select,
) -> (Range, Range) {
    let (mut lhs_ranges, mut rhs_ranges) = search(lhs, rhs, params);
    if lhs_ranges.is_empty() || rhs_ranges.is_empty() {
        return (Range::new(0.0, 0.0), Range::new(0.0, 0.0));
    }

    if select == Select::Earliest {
        // Their `GetEarliestTimeRange`: the pair whose left range starts first,
        // keeping the two sides paired rather than sorting them apart.
        let pairs = lhs_ranges.len().min(rhs_ranges.len());
        let earliest = (0..pairs)
            .min_by(|a, b| {
                lhs_ranges[*a]
                    .start
                    .partial_cmp(&lhs_ranges[*b].start)
                    .expect("timestamps are never NaN")
            })
            .expect("at least one pair");
        let (mut left, mut right) = (lhs_ranges[earliest], rhs_ranges[earliest]);
        if left.start <= START_SNAP_SECS {
            left.start = 0.0;
        }
        if right.start <= START_SNAP_SECS {
            right.start = 0.0;
        }
        return (left, right);
    }

    // Theirs sorts the two lists independently by descending duration and takes
    // the head of each, so the reported pair can in principle come from two
    // different alignments. Kept as-is.
    let by_duration = |a: &Range, b: &Range| {
        b.duration()
            .partial_cmp(&a.duration())
            .expect("durations are never NaN")
    };
    lhs_ranges.sort_by(by_duration);
    rhs_ranges.sort_by(by_duration);

    let mut left = lhs_ranges[0];
    let mut right = rhs_ranges[0];
    if left.start <= START_SNAP_SECS {
        left.start = 0.0;
    }
    if right.start <= START_SNAP_SECS {
        right.start = 0.0;
    }
    (left, right)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The vectors below are intro-skipper's own (IntroSkipper.Tests/TestContiguous.cs).
    // A port that passes its source's tests is the cheapest evidence there is.

    #[test]
    fn small_range() {
        let times = [
            1.0, 1.5, 2.0, 2.5, 3.0, 3.5, 4.0, 100.0, 100.5, 101.0, 101.5,
        ];
        assert_eq!(find_contiguous(&times, 2.0), Some(Range::new(1.0, 4.0)));
    }

    #[test]
    fn single_contiguous_range() {
        assert_eq!(
            find_contiguous(&[1.0, 2.0, 3.0, 4.0], 2.0),
            Some(Range::new(1.0, 4.0))
        );
    }

    #[test]
    fn last_contiguous_range_is_longest() {
        assert_eq!(
            find_contiguous(&[1.0, 2.0, 10.0, 11.0, 12.0, 13.0], 2.0),
            Some(Range::new(10.0, 13.0))
        );
    }

    #[test]
    fn large_range() {
        let times = [
            1.0, 1.5, 2.0, 2.8, 2.9, 2.995, 3.0, 3.01, 3.02, 3.4, 3.45, 3.48, 3.7, 3.77, 3.78,
            3.781, 3.782, 3.789, 3.85, 4.5, 5.3122, 5.3123, 5.3124, 5.3125, 5.3126, 5.3127, 5.3128,
            55.0, 55.5, 55.6, 55.7,
        ];
        assert_eq!(find_contiguous(&times, 2.0), Some(Range::new(1.0, 5.3128)));
    }

    #[test]
    fn empty_input_has_no_range() {
        assert_eq!(find_contiguous(&[], 2.0), None);
    }

    #[test]
    fn index_keeps_the_last_occurrence() {
        let index = inverted_index(&[1, 2, 3, 1, 5, 77, 42, 2]);
        assert_eq!(index[&1], 3);
        assert_eq!(index[&2], 7);
        assert_eq!(index[&3], 2);
        assert_eq!(index[&42], 6);
    }

    /// splitmix64, so "unrelated audio" in the tests below really is unrelated:
    /// arithmetic sequences of `u32` differ in far too few bits and match each
    /// other by accident.
    fn noise(seed: u64, n: usize) -> Vec<u32> {
        let mut state = seed;
        (0..n)
            .map(|_| {
                state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
                let mut z = state;
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                ((z ^ (z >> 31)) >> 32) as u32
            })
            .collect()
    }

    /// Two fingerprints sharing a 30 s head: the search should report it on both
    /// sides, snapped to zero, and ignore the differing tails.
    #[test]
    fn finds_a_shared_head() {
        let shared = noise(1, 250);
        let lhs: Vec<u32> = shared.iter().copied().chain(noise(2, 200)).collect();
        let rhs: Vec<u32> = shared.iter().copied().chain(noise(3, 200)).collect();

        let (l, r) = compare(&lhs, &rhs, &SearchParams::default());
        assert_eq!(l.start, 0.0);
        assert_eq!(r.start, 0.0);
        // 250 points at ~0.1238 s each, the range ending on the last of them.
        assert!((l.end - 249.0 * SAMPLE_DURATION).abs() < 1e-9, "{l:?}");
        assert!((r.end - 249.0 * SAMPLE_DURATION).abs() < 1e-9, "{r:?}");
    }

    #[test]
    fn unrelated_fingerprints_share_nothing() {
        let (l, r) = compare(&noise(4, 400), &noise(5, 400), &SearchParams::default());
        assert!(!l.valid() && !r.valid(), "{l:?} {r:?}");
    }
}
