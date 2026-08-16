import { beforeEach, describe, expect, test } from 'vitest'

import { ApiError, Offline } from '../src/api/errors.ts'
import {
  forgetRecoveries,
  isSessionGone,
  mayRecover,
  SESSION_GONE,
} from '../src/domain/recovery.ts'

const refusal = (status: number, code?: string) => new ApiError(status, 'no', code)

describe('what says a session is gone', () => {
  test('only 404', () => {
    expect(isSessionGone(refusal(SESSION_GONE))).toBe(true)
    // Do not read every later error as absence: a rate limit or a hub error
    // must not be answered by restarting the pipeline.
    expect(isSessionGone(refusal(409))).toBe(false)
    expect(isSessionGone(refusal(410))).toBe(false)
    expect(isSessionGone(refusal(429, 'session_cap'))).toBe(false)
    expect(isSessionGone(refusal(500))).toBe(false)
  })

  test('a network failure is not a dead session', () => {
    // Nothing was learned. Restarting on it spends a lease on a hub that never
    // said the session was gone, and does it again on the next ping.
    expect(isSessionGone(new Offline())).toBe(false)
    expect(isSessionGone(undefined)).toBe(false)
    expect(isSessionGone(new TypeError('x.map is not a function'))).toBe(false)
  })
})

describe('the loop guard', () => {
  beforeEach(forgetRecoveries)

  test('a recovery that made no progress does not get another', () => {
    expect(mayRecover('item-1', 60_000, 0)).toBe(true)
    expect(mayRecover('item-1', 60_000, 5_000)).toBe(false)
  })

  test('a recovery that played is allowed to recover again', () => {
    expect(mayRecover('item-1', 60_000, 0)).toBe(true)
    expect(mayRecover('item-1', 90_000, 30_000)).toBe(true)
  })

  test('the same position much later is a fresh problem, not a spin', () => {
    expect(mayRecover('item-1', 60_000, 0)).toBe(true)
    expect(mayRecover('item-1', 60_000, 5_000)).toBe(false)
    expect(mayRecover('item-1', 60_000, 120_000)).toBe(true)
  })

  test('another key does not clear the guard on this one', () => {
    // A film and a music queue are mounted together, and the queue alternates
    // between an active and a warmed idle slot — so recoveries for other keys
    // land between two for the same one as a matter of course. With a single
    // shared slot the last call below was allowed, and the loop this guard
    // exists to stop ran unbounded.
    expect(mayRecover('film', 60_000, 0)).toBe(true)
    expect(mayRecover('queue-track-a', 10_000, 100)).toBe(true)
    expect(mayRecover('queue-track-b', 0, 200)).toBe(true)
    expect(mayRecover('film', 60_000, 300)).toBe(false)
  })

  test('it remembers more than one key at a time', () => {
    expect(mayRecover('a', 1_000, 0)).toBe(true)
    expect(mayRecover('b', 2_000, 10)).toBe(true)
    expect(mayRecover('a', 1_000, 20)).toBe(false)
    expect(mayRecover('b', 2_000, 30)).toBe(false)
  })

  test('a key that keeps recovering is not evicted by newer ones', () => {
    // The bound is least-recently-USED, not first-inserted. A film playing all
    // evening recovers now and then while a queue churns through tracks, and
    // dropping the film's entry because it was recorded first is dropping the
    // guard on the one session long enough to spin.
    expect(mayRecover('film', 1_000, 0)).toBe(true)
    for (let n = 0; n < 31; n++) expect(mayRecover(`t${n}`, 1_000, n)).toBe(true)
    expect(mayRecover('film', 90_000, 100)).toBe(true) // it played on: recorded again
    for (let n = 100; n < 105; n++) expect(mayRecover(`u${n}`, 1_000, n)).toBe(true)
    expect(mayRecover('film', 90_000, 200)).toBe(false)
  })

  test('it is bounded, and the oldest key is the one that goes', () => {
    // A tab left open for a week must not grow this without limit. Thirty-two
    // is far above the per-user session cap, so nothing live is ever evicted.
    for (let n = 0; n < 40; n++) expect(mayRecover(`k${n}`, 1_000, n)).toBe(true)
    // The newest is still remembered...
    expect(mayRecover('k39', 1_000, 100)).toBe(false)
    // ...and the first one has been forgotten, so it is allowed again.
    expect(mayRecover('k0', 1_000, 101)).toBe(true)
  })
})
