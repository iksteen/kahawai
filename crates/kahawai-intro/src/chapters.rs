//! Boundaries a file NAMES, rather than ones we infer.
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

/// A boundary and what it is: `recap`, `intro` or `credits`, in seconds on
/// the file's own timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Named {
    pub kind: &'static str,
    pub start: f64,
    pub end: f64,
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

/// How long a named boundary is allowed to be, in seconds — upstream's
/// `GetBounds`, and its real safety net: the name patterns also match
/// things like "The Opening Ceremony", and only the duration check keeps a
/// six-minute scene chapter from becoming a skip over real content.
fn bounds(kind: &str) -> (f64, f64) {
    match kind {
        "credits" => (15.0, 450.0),
        _ => (15.0, 120.0),
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
pub fn named(chapters: &[kahawai_core::media::Chapter], duration_ms: u64) -> Vec<Named> {
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
        let (start, end) = (chapter.start_ms as f64 / 1000.0, end_ms as f64 / 1000.0);
        let (shortest, longest) = bounds(kind);
        if end - start < shortest || end - start > longest {
            continue;
        }
        candidates.push(Named { kind, start, end });
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
    out.sort_by(|a, b| a.start.total_cmp(&b.start));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kahawai_core::media::Chapter;

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
                    start: 4.0,
                    end: 69.708
                },
                Named {
                    kind: "intro",
                    start: 69.75,
                    end: 93.417
                },
                Named {
                    kind: "credits",
                    start: 2198.667,
                    end: 2417.27
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
            found.iter().map(|n| (n.kind, n.end)).collect::<Vec<_>>(),
            [("recap", 64.022), ("intro", 512.679), ("credits", 2142.599)]
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
        assert_eq!(found[0].end, 120.0);
    }

    #[test]
    fn a_stated_end_is_believed_over_the_next_start() {
        let mut intro = at(30_000, "Opening Theme");
        intro.end_ms = Some(120_000);
        let found = named(&[intro, at(600_000, "Part A")], 1_200_000);
        assert_eq!(found[0].end, 120.0);
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
        assert_eq!(found[0].start, 2520.0);
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
        assert_eq!((found[0].start, found[0].end), (30.0, 90.0));

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
        assert_eq!(found[0].start, 1000.0);
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
