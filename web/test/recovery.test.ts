import test from 'node:test'
import assert from 'node:assert/strict'
import {
  forgetRecoveries,
  isSessionDead,
  isSessionGone,
  isSourceOffline,
  mayRecover,
  SESSION_GONE,
  SOURCE_OFFLINE,
  startRetry,
} from '../src/recovery.ts'

test('only 410 means the session is gone', () => {
  assert.equal(isSessionGone({ status: SESSION_GONE }), true)
  // 404 on a session path is a missing sub-resource on a LIVE session
  // ("no such embedded track"). Restarting on it would kill playback
  // over a subtitle that was never there.
  assert.equal(isSessionGone({ status: 404 }), false)
  assert.equal(isSessionGone({ status: 200 }), false)
  assert.equal(isSessionGone({ status: 409 }), false)
  // Every case above sits BELOW 410, so `>= 410` passed them all: a rate
  // limit or a hub error would have been read as a session that is gone,
  // and answered by restarting the pipeline.
  assert.equal(isSessionGone({ status: 429 }), false)
  assert.equal(isSessionGone({ status: 500 }), false)
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

test('another key does not clear the guard on this one', () => {
  forgetRecoveries()
  // A film and a music queue are mounted together, and the queue alternates
  // between an active and a warmed idle slot — so recoveries for other keys
  // land between two for the same one as a matter of course. With a single
  // shared slot the second attempt below was allowed, and the loop this
  // guard exists to stop ran unbounded.
  assert.equal(mayRecover('film', 60_000, 0), true)
  assert.equal(mayRecover('queue-track-a', 10_000, 100), true)
  assert.equal(mayRecover('queue-track-b', 0, 200), true)
  assert.equal(mayRecover('film', 60_000, 300), false, 'same film, same position, still a spin')
})

test('the guard remembers more than one key at a time', () => {
  forgetRecoveries()
  assert.equal(mayRecover('a', 1_000, 0), true)
  assert.equal(mayRecover('b', 2_000, 10), true)
  assert.equal(mayRecover('a', 1_000, 20), false)
  assert.equal(mayRecover('b', 2_000, 30), false)
})

test('only 503 means the machine holding the file is away', () => {
  // The player branches on this to decide between standing by and reporting
  // that playback stopped. Reading 410 here — the other status the contract
  // names — turns a wait into "Playback stopped", which is the disagreement
  // the stand-by work exists to remove.
  assert.equal(isSourceOffline({ status: SOURCE_OFFLINE }), true)
  // 409 is the same endpoint refusing forever: no sources, unplayable, too
  // many streams. Waiting on it waits for ever.
  assert.equal(isSourceOffline({ status: 409 }), false)
  assert.equal(isSourceOffline({ status: SESSION_GONE }), false)
  // Not "any server error": a 500 or a 502 is the hub itself failing, and
  // standing by for a mediahost that was never the problem waits for ever.
  assert.equal(isSourceOffline({ status: 500 }), false)
  assert.equal(isSourceOffline({ status: 502 }), false)
  assert.equal(isSourceOffline(new Error('network')), false)
  assert.equal(isSourceOffline(undefined), false)
  assert.equal(isSourceOffline(null), false)
})

test('a thrown failure says the session is gone only on 410', () => {
  assert.equal(isSessionDead({ status: SESSION_GONE }), true)
  assert.equal(isSessionDead({ status: SOURCE_OFFLINE }), false)
  assert.equal(isSessionDead({ status: 404 }), false)
  // Nor "any 4xx from 410 up": a 429 restarts a pipeline that was only
  // being rate-limited, which is the conflation this module forbids.
  assert.equal(isSessionDead({ status: 429 }), false)
  assert.equal(isSessionDead({ status: 451 }), false)
  assert.equal(isSessionDead(undefined), false)
})

/// Shaped like the real one, so the predicate is pinned against what `api()`
/// throws rather than against a guess at it.
const offline = () => {
  const e = new Error('Could not reach the hub.')
  e.name = 'Offline'
  return e
}

test('a start is worth asking about again unless the item itself was refused', () => {
  // 503 and "no answer at all" are the two the album queue already waited out.
  assert.equal(startRetry({ status: SOURCE_OFFLINE }), 'wait')
  assert.equal(startRetry(offline()), 'wait')
  // A statusless throw that is NOT the network — a malformed body, a bug on
  // this side — is not weather to wait out.
  assert.equal(startRetry(new TypeError('x.map is not a function')), 'maybe')
  assert.equal(startRetry(undefined), 'maybe')
  // A hub restart behind a reverse proxy is a 502, not a dropped connection —
  // an answered status, and the case that made a narrower rule kill the queue.
  assert.equal(startRetry({ status: 502 }), 'wait')
  assert.equal(startRetry({ status: 504 }), 'wait')
  // 500 is the hub answering that it failed, which for one item does not clear
  // on its own — and the stand-by dialog it would select has no way out.
  assert.equal(startRetry({ status: 500 }), 'maybe')
  // 409 covers both "no sources, ever" and "too many streams, close one" —
  // bounded rather than endless, because the client cannot tell them apart.
  assert.equal(startRetry({ status: 409 }), 'maybe')
  assert.equal(startRetry({ status: 404 }), 'maybe')
})
