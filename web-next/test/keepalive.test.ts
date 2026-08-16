import { describe, expect, test, vi } from 'vitest'

import { IDLE_LIMIT_MS, keepSessionAlive, PING_MS } from '../src/domain/keepalive.ts'

/// Drive the interval by hand — fake timers, then advance — so a half-hour
/// bound is checked in no time.
function run(position: () => number, ticks: number): number[] {
  vi.useFakeTimers()
  const pings: number[] = []
  const stop = keepSessionAlive(position, (ms) => pings.push(ms))
  try {
    vi.advanceTimersByTime(PING_MS * ticks)
  } finally {
    stop()
    vi.useRealTimers()
  }
  return pings
}

const HELD = IDLE_LIMIT_MS / PING_MS

describe('keeping a session alive', () => {
  test('a moving playhead is pinged for as long as it moves', () => {
    let t = 0
    expect(run(() => (t += 1000), HELD * 3)).toHaveLength(HELD * 3)
  })

  test('a frozen playhead is held to the bound, then let go', () => {
    const pings = run(() => 5000, HELD + 50)
    // Held for exactly the bound and not one tick longer: past that the viewer
    // has gone and the session should be reaped as the orphan it is. Every
    // ping reports the real position, not a placeholder.
    expect(pings).toHaveLength(HELD)
    expect(pings.every((p) => p === 5000)).toBe(true)
  })

  test('a preload that never advances still gets its window', () => {
    // Position 0 for ever is the gapless preload: it must survive long enough
    // to be swapped in, without a rule of its own.
    const pings = run(() => 0, HELD + 50)
    expect(pings).toHaveLength(HELD)
    expect(pings.every((p) => p === 0)).toBe(true)
  })

  test('movement buys a whole fresh window, not the rest of the old one', () => {
    let n = 0
    let pos = 1000
    const pings = run(() => {
      if (++n === HELD) pos += 1000 // a nudge just as the window closes
      return pos
    }, HELD * 3)
    // A nudge that only postpones the reaping by a tick is not a reset. Off by
    // one is exactly what a weaker assertion here missed: without clearing the
    // stall count the nudge bought ONE more ping, and `> HELD` was still true.
    expect(pings.length).toBeGreaterThan(HELD * 1.5)
  })

  test('cancelling stops the pings', () => {
    vi.useFakeTimers()
    const pings: number[] = []
    const stop = keepSessionAlive(
      () => 0,
      (ms) => pings.push(ms),
    )
    vi.advanceTimersByTime(PING_MS * 2)
    stop()
    vi.advanceTimersByTime(PING_MS * 10)
    vi.useRealTimers()
    expect(pings).toHaveLength(2)
  })
})
