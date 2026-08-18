import { describe, expect, test } from 'vitest'

import type { Chapter } from '../src/api/generated/model/chapter.ts'
import { chapterTicks, chapterTitle } from '../src/domain/chapters.ts'

const at = (start_ms: number, title?: string): Chapter => ({ start_ms, title: title ?? null })

// A file as one arrives: named marks, the first at the very beginning.
const episode = [at(0, 'Recap'), at(64_022, 'End of Recap'), at(470_512, 'Intro')]

describe('chapter marks on a seek bar', () => {
  test('sit where the chapter starts', () => {
    const ticks = chapterTicks(episode, 2_142_599)
    expect(ticks.map((t) => t.title)).toEqual(['End of Recap', 'Intro'])
    expect(ticks[0]!.pct).toBeCloseTo(2.988, 3)
  })

  test('the one at zero is not drawn', () => {
    // Every file has it, and it marks the left edge of the bar.
    expect(chapterTicks([at(0, 'Opening Theme')], 600_000)).toEqual([])
  })

  test('nor anything past the end', () => {
    // A stale chapter list against a shorter file would otherwise stack
    // marks under the thumb at the finish.
    expect(chapterTicks([at(700_000, 'Credits')], 600_000)).toEqual([])
  })

  test('and none at all without a running time', () => {
    expect(chapterTicks(episode, 0)).toEqual([])
  })
})

describe('what a chapter is called', () => {
  test('its name', () => {
    expect(chapterTitle(at(0, ' Intro '), 0)).toBe('Intro')
  })

  test('its number, when the file gives it none', () => {
    expect(chapterTitle(at(0), 3)).toBe('Chapter 4')
    expect(chapterTitle(at(0, '   '), 0)).toBe('Chapter 1')
  })
})
