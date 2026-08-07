import test from 'node:test'
import assert from 'node:assert/strict'
import { refreshDelayMs } from '../src/token.ts'

const MIN = 60_000

test('a fresh token refreshes a minute before it expires', () => {
  // 15-minute token (auth.rs ACCESS_TTL_SECS), just issued.
  assert.equal(refreshDelayMs(15 * MIN, 0), 14 * MIN)
})

test('a token already inside its lead time refreshes at once', () => {
  assert.equal(refreshDelayMs(15 * MIN, 14.5 * MIN), 0)
})

test('an already-expired token refreshes at once, never in the past', () => {
  // A laptop that slept through the expiry wakes to a negative delay;
  // scheduling that as-is would be a timer that never fires sanely.
  assert.equal(refreshDelayMs(15 * MIN, 60 * MIN), 0)
})
