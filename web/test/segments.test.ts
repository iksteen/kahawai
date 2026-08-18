import { describe, expect, test } from 'vitest'

import type { Segment } from '../src/api/generated/model/segment.ts'
import { SKIP_TAIL_MS, skipLabel, skipTarget, skippable } from '../src/domain/segments.ts'

const seg = (kind: string, start_ms: number, end_ms: number): Segment => ({
  kind,
  start_ms,
  end_ms,
  source: 'chromaprint',
})

// An episode as the detector describes one: a recap, then the opening, then the
// credits running to the end of the file.
const recap = seg('recap', 0, 30_000)
const intro = seg('intro', 30_000, 61_000)
const credits = seg('credits', 541_000, 586_000)
const episode = [recap, intro, credits]

describe('what the player offers to skip', () => {
  test('the segment the playhead is inside', () => {
    expect(skipLabel(skippable(episode, 0))).toBe('Skip recap')
    expect(skipLabel(skippable(episode, 29_000 - SKIP_TAIL_MS))).toBe('Skip recap')
    expect(skipLabel(skippable(episode, 45_000))).toBe('Skip intro')
    expect(skipLabel(skippable(episode, 550_000))).toBe('Skip credits')
  })

  test('nothing between segments', () => {
    expect(skippable(episode, 200_000)).toBe(null)
    expect(skipLabel(null)).toBe('')
  })

  test('the offer withdraws before the segment ends', () => {
    // Otherwise the button is on screen for the last moment of the opening,
    // where pressing it does nothing a viewer would notice.
    expect(skippable(episode, 61_000 - SKIP_TAIL_MS)).toBe(null)
    expect(skippable(episode, 60_999)).toBe(null)
  })

  test('a kind this build does not know is not offered', () => {
    // A hub that grows a fourth kind must not put an unlabelled button on
    // screen in an older player.
    expect(skippable([seg('commercial', 0, 30_000)], 1_000)).toBe(null)
  })

  test('a kind off the prototype chain is not offered either', () => {
    // The lookups use Object.hasOwn, not `in`: with `in`, a hostile or
    // buggy kind like "toString" resolves up the prototype chain and a
    // function's source lands on the button. "commercial" above cannot
    // tell the two apart.
    expect(skippable([seg('toString', 0, 30_000)], 1_000)).toBe(null)
    expect(skipLabel(seg('toString', 0, 30_000))).toBe('')
  })
})

describe('where pressing it lands', () => {
  test('the end of the segment', () => {
    expect(skipTarget(intro, 586_000)).toBe(61_000)
  })

  test('never the last millisecond of the file', () => {
    // Credits end where the film does; seeking exactly there stalls on some
    // browsers and ends playback on others.
    expect(skipTarget(credits, 586_000)).toBe(585_000)
  })

  test('an unknown duration is not a reason to refuse the jump', () => {
    expect(skipTarget(credits, 0)).toBe(586_000)
  })
})
