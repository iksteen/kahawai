/// HUB-37: what to offer to skip, and where pressing it lands.
///
/// The hub says where the recap, the opening and the credits are; the player
/// only decides which one the playhead is inside and where the jump goes. Kept
/// out of the component because those two decisions are the whole feature, and
/// a wrong one either hides the button or throws the viewer past the film.

import type { Segment } from '../api/generated/model/segment.ts'

/// How close to a segment's end still counts as inside it. A button that
/// appears for the last half second of an opening is a button nobody can press
/// and everybody sees.
export const SKIP_TAIL_MS = 1500

const LABELS: Record<string, string> = {
  recap: 'Skip recap',
  intro: 'Skip intro',
  credits: 'Skip credits',
}

/// The segment the playhead is inside, if any. The first match wins: the
/// detector clamps ITS recap to the opening's start, but a chapter-named
/// boundary is stored as written and can overlap an inferred one, so inside
/// an overlap the button follows whichever segment started first. An
/// unknown kind is ignored rather than offered as "Skip".
export function skippable(
  segments: Segment[],
  posMs: number,
  tailMs = SKIP_TAIL_MS,
): Segment | null {
  return (
    segments.find(
      (s) => Object.hasOwn(LABELS, s.kind) && posMs >= s.start_ms && posMs < s.end_ms - tailMs,
    ) ?? null
  )
}

export function skipLabel(segment: Segment | null): string {
  // hasOwn, not `in`: a kind like 'toString' walks the prototype chain and
  // would render a function's source as the button text.
  return segment && Object.hasOwn(LABELS, segment.kind) ? LABELS[segment.kind]! : ''
}

/// Where the button lands: the end of the segment, but never the very last
/// millisecond of the file. A seek to the duration stalls on some browsers and
/// ends playback on others, and credits usually end exactly there — so land
/// just inside and let the up-next countdown do what it does at the end of any
/// episode.
export function skipTarget(segment: Segment, durationMs: number): number {
  const end = durationMs > 0 ? Math.min(segment.end_ms, durationMs - 1000) : segment.end_ms
  return Math.max(0, end)
}
