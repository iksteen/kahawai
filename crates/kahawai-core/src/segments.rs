//! Named chapter boundaries and protocol validation for inferred boundaries.
//!
//! intro-skipper's `ChapterAnalyzer` in one function: plenty of rips mark
//! their own opening, recap and credits, and where they do there is nothing
//! to detect — no fingerprints, no black-frame search, and on a remote
//! library not one byte across the LAN. It is also the only analyzer that
//! can answer for a season of one episode, because it compares nothing.
//!
//! What it will not do is guess. A chapter list of "Chapter 1..12" says
//! nothing about where the opening is, and a name it does not recognise is
//! left alone for the fingerprint pass rather than turned into a boundary
//! somebody's player would jump on.

/// A boundary and what it is: `recap`, `intro` or `credits`, in milliseconds
/// on the file's own timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Named {
    pub kind: &'static str,
    pub start_ms: u64,
    pub end_ms: u64,
}

pub const INTRO_MAX_MS: u64 = 120_000;
pub const RECAP_MAX_MS: u64 = 120_000;
pub const CREDITS_WINDOW_MS: u64 = 450_000;
pub const INTRO_WINDOW_LIMIT_MS: u64 = 600_000;
pub const INTRO_PERCENT_MIN_DURATION_MS: u64 = 300_000;
pub const END_REFINEMENT_OUTWARD_MS: u64 = 2_000;
pub const SHARED_REGION_MIN_MS: u64 = 15_000;
pub const RECAP_MIN_MS: u64 = 15_000;
pub const BLACK_CREDITS_MIN_MS: u64 = 15_000;
pub const END_REFINEMENT_INWARD_MS: u64 = 5_000;
pub const INTRO_START_SNAP_MS: u64 = 5_000;

/// Whether a generation-2 inferred answer could have been produced by the
/// detector's configured search windows. This is the hub's trust-boundary
/// check for mediahost replies, kept beside the constants the analyzer uses.
pub fn inferred_within_bounds(
    kind: &str,
    analyzer: &str,
    start_ms: u64,
    end_ms: u64,
    duration_ms: u64,
) -> bool {
    if end_ms <= start_ms || end_ms > duration_ms {
        return false;
    }
    let length = end_ms - start_ms;
    match (kind, analyzer) {
        ("recap", "blackframe") => start_ms == 0 && (RECAP_MIN_MS..=RECAP_MAX_MS).contains(&length),
        ("intro", "chromaprint") => {
            let head_end = if duration_ms >= INTRO_PERCENT_MIN_DURATION_MS {
                (duration_ms / 4).min(INTRO_WINDOW_LIMIT_MS)
            } else {
                duration_ms
            };
            let minimum = SHARED_REGION_MIN_MS - END_REFINEMENT_INWARD_MS;
            let raw_region_fits = start_ms.saturating_add(SHARED_REGION_MIN_MS) <= head_end;
            let start_is_representable = start_ms == 0 || start_ms >= INTRO_START_SNAP_MS;
            raw_region_fits
                && start_is_representable
                && length >= minimum
                && length <= INTRO_MAX_MS + END_REFINEMENT_OUTWARD_MS
                && start_ms <= head_end
                && end_ms <= (head_end + END_REFINEMENT_OUTWARD_MS).min(duration_ms)
        }
        ("credits", "blackframe") => {
            end_ms == duration_ms
                && length >= BLACK_CREDITS_MIN_MS
                && start_ms >= duration_ms.saturating_sub(CREDITS_WINDOW_MS)
        }
        ("credits", "chromaprint") => {
            start_ms.saturating_add(SHARED_REGION_MIN_MS) <= duration_ms
                && length >= SHARED_REGION_MIN_MS - END_REFINEMENT_INWARD_MS
                && start_ms >= duration_ms / 2
                && start_ms >= duration_ms.saturating_sub(CREDITS_WINDOW_MS)
        }
        _ => false,
    }
}

/// What the title says this chapter is, or `None` for the ones that are
/// just a scene.
///
/// Order matters twice over: "opening credits" is an opening, not the end
/// credits, and "end of intro" is a MARKER for where the intro stopped —
/// the chapter it names starts there, so treating it as the intro would
/// hand a viewer the whole body of the episode to skip.
///
/// WORDS, not substrings, with upstream's OWN boundaries — `(^|\s)` before
/// and `(\s|:|$)` after. A plain `contains` read "Recapture" as a recap and
/// "Introducing Dorothy Dandridge" as an opening; splitting on every
/// non-alphanumeric went too far the other way, reading "Preview/Recap" (a
/// SponsorBlock label upstream deliberately refuses) as a recap and
/// "Op. 9" as an opening. So: a needle matches a whitespace token exactly,
/// or as a prefix whose remainder starts with a colon ("Intro:Part 1" is a
/// match, "Introduce" is not) — the regex anchors, spelled out.
fn kind_of(title: &str) -> Option<&'static str> {
    let t = title.trim().to_ascii_lowercase();
    if t.starts_with("end of ") || t.ends_with(" end") || t.ends_with(" start") {
        return None;
    }
    let tokens: Vec<&str> = t.split_ascii_whitespace().collect();
    let word = |w: &str, needle: &str| {
        w == needle
            || w.strip_prefix(needle)
                .is_some_and(|rest| rest.starts_with(':'))
    };
    let has = |needle: &str| tokens.iter().any(|w| word(w, needle));
    let phrase = |a: &str, b: &str| tokens.windows(2).any(|w| w[0] == a && word(w[1], b));
    if has("recap") || has("recaps") || has("previously") {
        return Some("recap");
    }
    if has("intro")
        || has("introduction")
        || has("opening")
        || has("op")
        || phrase("main", "title")
        || phrase("main", "titles")
        || phrase("title", "sequence")
        || phrase("titles", "sequence")
    {
        return Some("intro");
    }
    if has("credit")
        || has("credits")
        || has("ending")
        || has("outro")
        || has("closing")
        || has("ed")
    {
        return Some("credits");
    }
    None
}

/// How long a named boundary is allowed to be, in milliseconds.
fn bounds(kind: &str) -> (u64, u64) {
    match kind {
        "credits" => (SHARED_REGION_MIN_MS, CREDITS_WINDOW_MS),
        _ => (SHARED_REGION_MIN_MS, INTRO_MAX_MS),
    }
}

/// The boundaries a file's chapters name, in start order.
///
/// A chapter ends where the container says it does, or where the next one
/// starts, or at the end of the file — in that order, because a stated end
/// can be earlier than the next start and that gap is a fact about the
/// file. One of each kind: recap and intro keep the FIRST in-bounds match,
/// credits keep the LAST — upstream scans reversed for credits precisely so
/// a double episode's mid-file credits lose to the ones at the end.
pub fn named(chapters: &[crate::media::Chapter], duration_ms: u64) -> Vec<Named> {
    let kinds: Vec<Option<&'static str>> = chapters
        .iter()
        .map(|c| c.title.as_deref().and_then(kind_of))
        .collect();
    let mut candidates: Vec<Named> = Vec::new();
    for (i, chapter) in chapters.iter().enumerate() {
        let Some(kind) = kinds[i] else {
            continue;
        };
        // Upstream's ambiguity net (`FindMatchingChapter`): a candidate
        // whose scan-direction neighbour names the SAME kind is half of a
        // split boundary, not the boundary. "Opening" then "Opening Theme"
        // must yield the pair's second span, and "Credits Part 1" /
        // "Part 2" must start the skip at Part 1 — the neighbour, not the
        // half, is what survives.
        let ambiguous = if kind == "credits" {
            i > 0 && kinds[i - 1] == Some(kind)
        } else {
            kinds.get(i + 1).copied().flatten() == Some(kind)
        };
        if ambiguous {
            continue;
        }
        // A zero duration is "unknown", not a clamp: min() against it
        // collapsed every chapter's end to its own start and dropped even
        // explicitly stated ends.
        let cap = if duration_ms > 0 {
            duration_ms.max(chapter.start_ms)
        } else {
            u64::MAX
        };
        let end_ms = chapter
            .end_ms
            .or_else(|| chapters.get(i + 1).map(|next| next.start_ms))
            .unwrap_or(duration_ms)
            .min(cap);
        if end_ms <= chapter.start_ms {
            continue;
        }
        let (shortest, longest) = bounds(kind);
        if !(shortest..=longest).contains(&(end_ms - chapter.start_ms)) {
            continue;
        }
        candidates.push(Named {
            kind,
            start_ms: chapter.start_ms,
            end_ms,
        });
    }
    let mut out: Vec<Named> = Vec::new();
    for kind in ["recap", "intro", "credits"] {
        let mut of_kind = candidates.iter().filter(|n| n.kind == kind);
        let pick = if kind == "credits" {
            of_kind.next_back()
        } else {
            of_kind.next()
        };
        out.extend(pick.cloned());
    }
    out.sort_by_key(|boundary| boundary.start_ms);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::Chapter;

    fn at(start_ms: u64, title: &str) -> Chapter {
        Chapter {
            start_ms,
            end_ms: None,
            title: Some(title.into()),
        }
    }

    #[test]
    fn a_named_chapter_runs_until_the_next_one() {
        // Andor S01E03 as the file states it.
        let found = named(
            &[
                at(4_000, "Recap"),
                at(69_708, "Scene 1"),
                at(69_750, "Intro"),
                at(93_417, "Scene 2"),
                at(2_198_667, "Credits"),
            ],
            2_417_270,
        );
        assert_eq!(
            found,
            [
                Named {
                    kind: "recap",
                    start_ms: 4_000,
                    end_ms: 69_708,
                },
                Named {
                    kind: "intro",
                    start_ms: 69_750,
                    end_ms: 93_417,
                },
                Named {
                    kind: "credits",
                    start_ms: 2_198_667,
                    end_ms: 2_417_270,
                },
            ]
        );
    }

    #[test]
    fn an_end_marker_is_not_the_thing_it_marks() {
        // The Plex convention: "End of Recap" starts where the recap ended
        // and runs to the credits. Read as a recap it would offer a viewer
        // the whole episode to skip.
        let found = named(
            &[
                at(0, "Recap"),
                at(64_022, "End of Recap"),
                at(470_512, "Intro"),
                at(512_679, "End of Intro"),
                at(2_093_592, "Credits"),
            ],
            2_142_599,
        );
        assert_eq!(
            found.iter().map(|n| (n.kind, n.end_ms)).collect::<Vec<_>>(),
            [
                ("recap", 64_022),
                ("intro", 512_679),
                ("credits", 2_142_599)
            ]
        );
    }

    #[test]
    fn opening_credits_are_an_opening() {
        let found = named(&[at(0, "Opening Credits"), at(60_000, "Act 1")], 1_200_000);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, "intro");
    }

    #[test]
    fn a_numbered_chapter_list_says_nothing() {
        // Most of this library. Silence here is what sends the season to
        // the fingerprint pass instead.
        let found = named(
            &[
                at(0, "Chapter 1"),
                at(300_000, "Chapter 2"),
                at(600_000, "Scene 3"),
            ],
            1_200_000,
        );
        assert!(found.is_empty());
    }

    #[test]
    fn an_unknown_duration_does_not_swallow_a_stated_end() {
        // duration 0 means "could not probe", not "everything ends at its
        // own start": min() against it dropped even explicit ends.
        let mut intro = at(30_000, "Intro");
        intro.end_ms = Some(120_000);
        let found = named(&[intro], 0);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].end_ms, 120_000);
    }

    #[test]
    fn a_stated_end_is_believed_over_the_next_start() {
        let mut intro = at(30_000, "Opening Theme");
        intro.end_ms = Some(120_000);
        let found = named(&[intro, at(600_000, "Part A")], 1_200_000);
        assert_eq!(found[0].end_ms, 120_000);
    }

    #[test]
    fn the_last_credits_chapter_wins() {
        // A double episode marks credits mid-file and again at the end;
        // upstream scans reversed for credits so the END ones win — the
        // first-match rule stored the mid-file pair and put the skip button
        // up twenty minutes early.
        let found = named(
            &[
                at(0, "Part One"),
                at(1_200_000, "Credits"),
                at(1_260_000, "Part Two"),
                at(2_520_000, "End Credits"),
            ],
            2_580_000,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].start_ms, 2_520_000);
    }

    #[test]
    fn a_marker_with_no_subject_names_nothing() {
        // "End of Recap" with no preceding "Recap": read as a recap it
        // spans the episode body. The other guard arms too — a "* End" /
        // "* Start" marker is a POINT someone labelled, not a span.
        for marker in ["End of Recap", "Intro End", "Credits Start"] {
            let found = named(&[at(0, "Scene 1"), at(64_022, marker)], 1_200_000);
            assert!(found.is_empty(), "{marker} must name nothing: {found:?}");
        }
    }

    #[test]
    fn a_word_inside_another_word_names_nothing() {
        // Substring matching read each of these as a boundary; they are
        // scenes. Word tokens, like upstream's regex anchors.
        for scene in [
            "Recapture",
            "Introducing Dorothy Dandridge",
            "Uncredited Cameo",
        ] {
            let found = named(&[at(0, scene), at(60_000, "Scene 2")], 1_200_000);
            assert!(found.is_empty(), "{scene} must name nothing: {found:?}");
        }
    }

    #[test]
    fn punctuation_glued_to_a_short_token_names_nothing() {
        // "op"/"ed" are words under upstream's anchors: only whitespace
        // before and whitespace/colon after count as boundaries, so the
        // period in "Op. 9" and the apostrophe in "Ed's Theme" disqualify
        // the token. Bare "OP"/"ED" still count.
        for scene in ["Op. 9", "Ed's Theme"] {
            let found = named(&[at(0, scene), at(60_000, "Scene 2")], 1_200_000);
            assert!(found.is_empty(), "{scene} must name nothing: {found:?}");
        }
        let found = named(&[at(0, "OP"), at(60_000, "Part A")], 1_200_000);
        assert_eq!(found.first().map(|n| n.kind), Some("intro"));
    }

    #[test]
    fn main_titles_plural_is_still_an_opening() {
        // One of the most common DVD chapter names; losing it broke the
        // whole season's byte-free all-named path, not just one label.
        for title in ["Main Titles", "Main Title", "Title Sequence"] {
            let found = named(&[at(0, title), at(60_000, "Act 1")], 1_200_000);
            assert_eq!(found.first().map(|n| n.kind), Some("intro"), "{title}");
        }
    }

    #[test]
    fn upstream_boundaries_are_space_and_colon_only() {
        // "Preview/Recap" is a SponsorBlock label upstream deliberately
        // refuses (ambiguous, mapped to Commercial only); splitting on
        // every non-alphanumeric read it as a recap.
        for scene in ["Preview/Recap", "Intro/Outro", "Hook+Intro"] {
            let found = named(&[at(0, scene), at(60_000, "Scene 2")], 1_200_000);
            assert!(found.is_empty(), "{scene} must name nothing: {found:?}");
        }
        // A colon after the needle is upstream's one allowed suffix,
        // spaced or not.
        for title in ["Intro: Part One", "Intro:Part One"] {
            let found = named(&[at(0, title), at(60_000, "Act 1")], 1_200_000);
            assert_eq!(found.first().map(|n| n.kind), Some("intro"), "{title}");
        }
    }

    #[test]
    fn a_split_boundary_keeps_the_scan_direction_neighbour() {
        // Upstream's ambiguity net: "Opening" + "Opening Theme" is one
        // opening split in two, and the FIRST half alone covers half the
        // theme — the forward scan keeps the second span.
        let found = named(
            &[
                at(0, "Opening"),
                at(30_000, "Opening Theme"),
                at(90_000, "Act 1"),
            ],
            1_200_000,
        );
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].start_ms, found[0].end_ms), (30_000, 90_000));

        // Credits scan reversed, so the PREVIOUS neighbour wins: the skip
        // must start at Part 1, not Part 2.
        let found = named(
            &[
                at(0, "Act 3"),
                at(1_000_000, "Credits Part 1"),
                at(1_060_000, "Credits Part 2"),
            ],
            1_200_000,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].start_ms, 1_000_000);
    }

    #[test]
    fn a_boundary_longer_than_its_kind_allows_is_a_scene() {
        // Upstream's duration bounds are the safety net for its own loose
        // patterns: "Opening Scene" running eight minutes is content, and a
        // skip button over it hands the viewer the first act.
        let found = named(&[at(0, "Opening Scene"), at(480_000, "Act 2")], 1_200_000);
        assert!(found.is_empty(), "{found:?}");
        // And too short is a marker, not a span.
        let found = named(&[at(0, "Intro"), at(3_000, "Act 1")], 1_200_000);
        assert!(found.is_empty(), "{found:?}");
    }
}
