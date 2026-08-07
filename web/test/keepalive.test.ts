import test from 'node:test'
import assert from 'node:assert/strict'
import { keepSessionAlive, PING_MS, IDLE_LIMIT_MS } from '../src/keepalive.ts'

/// Drive the interval by hand — capture the callback setInterval is
/// handed and call it — so a half-hour bound is checked in no time.
function run(position: () => number, ticks: number): number[] {
  const realSet = globalThis.setInterval
  const realClear = globalThis.clearInterval
  let fire = () => {}
  globalThis.setInterval = ((fn: () => void) => {
    fire = fn
    return 1
  }) as unknown as typeof setInterval
  globalThis.clearInterval = (() => {}) as unknown as typeof clearInterval
  const pings: number[] = []
  try {
    keepSessionAlive(position, (ms) => pings.push(ms))
    for (let i = 0; i < ticks; i++) fire()
  } finally {
    globalThis.setInterval = realSet
    globalThis.clearInterval = realClear
  }
  return pings
}

const HELD = IDLE_LIMIT_MS / PING_MS

test('a moving playhead is pinged for as long as it moves', () => {
  let t = 0
  const pings = run(() => (t += 1000), HELD * 3)
  assert.equal(pings.length, HELD * 3)
})

test('a frozen playhead is held to the bound, then let go', () => {
  const pings = run(() => 5000, HELD + 50)
  // Held for exactly the bound and not one tick longer: past that the
  // viewer has gone and the session should be reaped as the orphan it
  // is. Every ping reports the real position, not a placeholder.
  assert.equal(pings.length, HELD)
  assert.ok(pings.every((p) => p === 5000))
})

test('a preload that never advances still gets its window', () => {
  // Position 0 forever is the gapless preload: it must survive a pause
  // long enough to be swapped in, without a rule of its own.
  const pings = run(() => 0, HELD + 50)
  assert.equal(pings.length, HELD)
  assert.ok(pings.every((p) => p === 0))
})

test('movement resets the bound', () => {
  let n = 0
  let pos = 1000
  const pings = run(() => {
    if (++n === HELD) pos += 1000 // a nudge just as the window closes
    return pos
  }, HELD * 2)
  assert.ok(pings.length > HELD, `expected the nudge to buy a fresh window, got ${pings.length}`)
})
