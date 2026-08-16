/// A sideways-scrolling row: which arrows are live, and when to ask for more.
/// None of this is testable through a component — happy-dom does no layout, so
/// every scrollWidth it reports is zero.

import { describe, expect, test } from 'vitest'

import { askAgain, edges, nearEnd } from '../src/domain/lane.ts'

const STEP = 450
/// A lane 600 wide holding 1500, so there is 900 to travel.
const at = (scrollLeft: number) => ({ scrollLeft, clientWidth: 600, scrollWidth: 1500 })

describe('which arrows can move anything', () => {
  test('neither, when the contents fit', () => {
    expect(edges({ scrollLeft: 0, clientWidth: 600, scrollWidth: 600 })).toEqual({
      left: false,
      right: false,
    })
  })

  test('right only, at the start', () => {
    expect(edges(at(0))).toEqual({ left: false, right: true })
  })

  test('left only, at the end', () => {
    expect(edges(at(900))).toEqual({ left: true, right: false })
  })

  test('and a fraction of a pixel is not more to see', () => {
    // Fractional layout widths mean scrollLeft never quite reaches the
    // arithmetic end, and an arrow that cannot move anything is worse than no
    // arrow.
    expect(edges(at(899.6)).right).toBe(false)
    expect(edges({ scrollLeft: 0.5, clientWidth: 600, scrollWidth: 1500 }).left).toBe(false)
  })
})

describe('when to ask for more', () => {
  test('one press from the end, not at it', () => {
    // Fetch before the viewer arrives rather than when they hit the wall.
    expect(nearEnd(at(0), STEP)).toBe(false)
    expect(nearEnd(at(460), STEP)).toBe(true)
    expect(nearEnd(at(900), STEP)).toBe(true)
  })

  test('once per width, however many times it is asked', () => {
    // The edges are re-read on every render, and a lane sitting at its end is
    // near it on all of them: without this, a shelf scrolled to the end
    // fetched another page on every keystroke in the search box. A page that
    // FAILED was worse — its notice re-rendered the shell, which asked again.
    let firedAt = -1
    const ask = () => {
      const r = askAgain(firedAt, at(900), STEP)
      firedAt = r.firedAt
      return r.ask
    }
    expect(ask()).toBe(true)
    expect(ask()).toBe(false)
    expect(ask()).toBe(false)
  })

  test('and again once new cards have arrived', () => {
    // The width changes exactly when new cards arrive, which is exactly when
    // asking again is meaningful.
    const first = askAgain(-1, at(900), STEP)
    expect(first.ask).toBe(true)
    const wider = { scrollLeft: 900, clientWidth: 600, scrollWidth: 3000 }
    // Not near the end of the wider lane, so nothing is asked...
    expect(askAgain(first.firedAt, wider, STEP).ask).toBe(false)
    // ...and once scrolled to ITS end, it asks again.
    const atEnd = { scrollLeft: 2400, clientWidth: 600, scrollWidth: 3000 }
    expect(askAgain(first.firedAt, atEnd, STEP).ask).toBe(true)
  })

  test('scrolling away and back asks again; sitting still does not', () => {
    const fired = askAgain(-1, at(900), STEP)
    expect(fired.ask).toBe(true)
    // Away: the guard is released.
    const away = askAgain(fired.firedAt, at(0), STEP)
    expect(away.ask).toBe(false)
    expect(away.firedAt).toBe(-1)
    // And back.
    expect(askAgain(away.firedAt, at(900), STEP).ask).toBe(true)
  })
})
