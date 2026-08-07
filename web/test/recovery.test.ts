import test from 'node:test'
import assert from 'node:assert/strict'
import { isSessionGone, mayRecover, forgetRecoveries, SESSION_GONE } from '../src/recovery.ts'

test('only 410 means the session is gone', () => {
  assert.equal(isSessionGone({ status: SESSION_GONE }), true)
  // 404 on a session path is a missing sub-resource on a LIVE session
  // ("no such embedded track"). Restarting on it would kill playback
  // over a subtitle that was never there.
  assert.equal(isSessionGone({ status: 404 }), false)
  assert.equal(isSessionGone({ status: 200 }), false)
  assert.equal(isSessionGone({ status: 409 }), false)
  // A network failure resolves to undefined — not a dead session.
  assert.equal(isSessionGone(undefined), false)
})

test('a recovery that made no progress does not get another', () => {
  forgetRecoveries()
  assert.equal(mayRecover('item-1', 60_000, 0), true, 'first attempt allowed')
  assert.equal(mayRecover('item-1', 60_000, 5_000), false, 'same position = the retry never played')
})

test('a recovery that played is allowed to recover again', () => {
  forgetRecoveries()
  assert.equal(mayRecover('item-1', 60_000, 0), true)
  assert.equal(mayRecover('item-1', 90_000, 30_000), true, 'position advanced: real progress')
})

test('a different item is never blocked by another item loop', () => {
  forgetRecoveries()
  assert.equal(mayRecover('item-1', 60_000, 0), true)
  assert.equal(mayRecover('item-2', 60_000, 1_000), true)
})

test('the same position much later is a fresh problem, not a spin', () => {
  forgetRecoveries()
  assert.equal(mayRecover('item-1', 60_000, 0), true)
  assert.equal(mayRecover('item-1', 60_000, 5_000), false)
  assert.equal(mayRecover('item-1', 60_000, 120_000), true, 'past the loop window')
})
