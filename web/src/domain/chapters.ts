/// The file's own chapters, as the two places that show them need
/// them — ticks on the seek bar, and a list to jump from.
///
/// Kept out of the components because both have to agree about what a
/// chapter with no name is called and which ones are worth drawing, and
/// because the arithmetic that turns a millisecond into a position on a
/// bar is the kind that is wrong by a factor of a thousand.

import type { Chapter } from '../api/generated/model/chapter.ts'

/// What a nameless chapter is called. Plenty of rips number them and say
/// nothing else, and "Chapter 4" is still somewhere to jump to.
export function chapterTitle(chapter: Chapter, index: number): string {
  return chapter.title?.trim() || `Chapter ${index + 1}`
}

export interface Tick {
  startMs: number
  title: string
  /// Where it sits on the bar, 0–100.
  pct: number
}

/// Chapter marks to draw on a seek bar of `durationMs`.
///
/// A chapter at zero is dropped: every file has one and it marks the left
/// edge of the bar, where there is nothing to find. So is anything at or
/// past the end, which would sit under the thumb at the finish.
export function chapterTicks(chapters: Chapter[], durationMs: number): Tick[] {
  if (!(durationMs > 0)) return []
  return chapters.flatMap((chapter, index) =>
    chapter.start_ms > 0 && chapter.start_ms < durationMs
      ? [
          {
            startMs: chapter.start_ms,
            title: chapterTitle(chapter, index),
            pct: (chapter.start_ms / durationMs) * 100,
          },
        ]
      : [],
  )
}
