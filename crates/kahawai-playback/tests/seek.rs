use kahawai_playback::seek::{PendingSeek, TimelinePart, part_index, reads_from};

struct Part {
    base_ms: u64,
    host: &'static str,
}

impl TimelinePart for Part {
    fn base_ms(&self) -> u64 {
        self.base_ms
    }
    fn module_id(&self) -> &str {
        self.host
    }
}

fn part(base_ms: u64) -> Part {
    Part { base_ms, host: "A" }
}

fn on(host: &'static str) -> Part {
    Part { base_ms: 0, host }
}

/// A part's playlist ends and the client seeks to `end + 250 ms`; which
/// file that lands in is decided here. Boundaries measured against a real
/// two-part rip (part 2 based at 3_752_711).
#[test]
fn a_timestamp_lands_in_the_part_that_contains_it() {
    let parts = [part(0), part(3_752_711)];
    assert_eq!(part_index(&parts, 0), 0, "start of part one");
    assert_eq!(part_index(&parts, 3_752_710), 0, "last ms of part one");
    // base_ms is inclusive: the boundary itself is already part two,
    // which is why the client's `+250` cannot land back in part one.
    assert_eq!(part_index(&parts, 3_752_711), 1, "the boundary");
    assert_eq!(part_index(&parts, 3_752_961), 1, "end-of-part-one + 250");
    assert_eq!(part_index(&parts, 7_511_677), 1, "last ms of the film");
    // Past the end clamps to the final part rather than panicking.
    assert_eq!(part_index(&parts, u64::MAX), 1, "past the end");
    assert_eq!(part_index(&[part(0)], 999), 0);
    assert_eq!(part_index(&[part(0)], 10_000), 0);
    // No parts at all is not reachable today, but must not panic.
    assert_eq!(part_index::<Part>(&[], 42), 0);
}

#[test]
fn a_session_reads_from_every_host_its_parts_live_on() {
    // CD1 on A, CD2 on B, started on CD1.
    let parts = [on("A"), on("B")];
    assert!(reads_from("A", &parts, "A"), "the host it started on");
    // B going away used to leave this session alive on a dead lease,
    // which is the stall AR-6 exists to prevent.
    assert!(reads_from("A", &parts, "B"), "a later part's host");
    assert!(!reads_from("A", &parts, "C"), "a host it never reads from");
}

#[test]
fn a_part_already_passed_still_counts() {
    // A session STARTED on CD2 still reads from CD1's host: the viewer can
    // seek back into it, and ending the session when its host leaves is
    // the honest answer.
    let parts = [on("A"), on("B")];
    assert!(reads_from("B", &parts, "A"));
}

#[test]
fn a_single_part_session_is_unchanged() {
    let parts = [on("A")];
    assert!(reads_from("A", &parts, "A"));
    assert!(!reads_from("A", &parts, "B"));
}

/// A scrub must not silently discard a queued track switch: the newest
/// position wins, explicit track choices carry forward.
#[test]
fn scrub_keeps_the_queued_track_switch() {
    let switch = PendingSeek {
        replan_subs: false,
        generation: 1,
        position_ms: 1000,
        audio_track: Some(1),
        video_track: None,
    };
    let scrub = PendingSeek {
        replan_subs: false,
        generation: 2,
        position_ms: 9000,
        audio_track: None,
        video_track: None,
    };
    let merged = PendingSeek::merge(Some(switch), scrub);
    assert_eq!(merged.generation, 2);
    assert_eq!(merged.position_ms, 9000, "newest position wins");
    assert_eq!(merged.audio_track, Some(1), "track choice survives");
    let re_switch = PendingSeek {
        replan_subs: true,
        generation: 3,
        position_ms: 9000,
        audio_track: Some(0),
        video_track: None,
    };
    let again = PendingSeek::merge(Some(merged), re_switch);
    assert_eq!(
        again.audio_track,
        Some(0),
        "an explicit newer choice overrides"
    );
    assert!(
        again.replan_subs,
        "a re-plan request is never lost by a later scrub"
    );
    assert_eq!(PendingSeek::merge(None, scrub), scrub);
}
