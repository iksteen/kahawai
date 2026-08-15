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
  SESSION_CAP,
  startRetry,
} from '../src/recovery.ts'

test('only 404 means the session is gone', () => {
  assert.equal(isSessionGone({ status: SESSION_GONE }), true)
  assert.equal(isSessionGone({ status: 200 }), false)
  assert.equal(isSessionGone({ status: 409 }), false)
  assert.equal(isSessionGone({ status: 410 }), false)
  // Do not read every later error as absence: a rate limit or a hub error
  // must not be answered by restarting the pipeline.
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
  // that playback stopped. Reading 404 here — the other status the contract
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

test('a thrown failure says the session is gone only on 404', () => {
  assert.equal(isSessionDead({ status: SESSION_GONE }), true)
  assert.equal(isSessionDead({ status: SOURCE_OFFLINE }), false)
  assert.equal(isSessionDead({ status: 410 }), false)
  // Nor "any later 4xx": a 429 restarts a pipeline that was only being
  // rate-limited, which is the conflation this module forbids.
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
  assert.equal(startRetry({ status: SOURCE_OFFLINE, code: 'source_offline' }), 'wait')
  // Two 503s clear only when an operator acts, and standing by on them names
  // an unreachable machine that is answering perfectly well.
  assert.equal(startRetry({ status: SOURCE_OFFLINE, code: 'setup_required' }), 'maybe')
  assert.equal(startRetry({ status: SOURCE_OFFLINE, code: 'provider_unconfigured' }), 'maybe')
  // A 503 with no code is an intermediary's — HAProxy and ingress-nginx answer
  // it for a backend that is down — which is the ordinary hub restart this
  // branch exists to wait out. Requiring the hub's own code made those final.
  assert.equal(startRetry({ status: SOURCE_OFFLINE }), 'wait')
  assert.equal(isSourceOffline({ status: SOURCE_OFFLINE }), true)
  assert.equal(isSourceOffline({ status: SOURCE_OFFLINE, code: 'setup_required' }), false)
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
  // The pair this split exists for. The per-account session cap clears the
  // moment a session ends; 409 is about the item and will refuse again
  // forever. They used to be the same status with the difference in the prose,
  // and the queue guessed at three tries.
  assert.equal(startRetry({ status: SESSION_CAP, code: 'session_cap' }), 'busy')
  // A 429 that is not the hub's — a reverse proxy or WAF rate-limiting the
  // tab, with an HTML body and so no code — must not buy `BUSY_TRIES` worth
  // of asking at a rate limiter.
  assert.equal(startRetry({ status: SESSION_CAP }), 'maybe')
  assert.equal(startRetry({ status: SESSION_CAP, code: 'login_throttled' }), 'maybe')
  assert.equal(startRetry({ status: 409 }), 'maybe')
  assert.equal(startRetry({ status: 404 }), 'maybe')
})

/// `busy` is not `wait`, and the difference is what is on screen.
///
/// A background queue asks again for both. A player stands by only for `wait`:
/// the standby dialog says the machine holding the file has stopped answering
/// and offers one button, which for a stream cap is a false cause and the
/// wrong single option — the viewer can fix that one by closing something, and
/// the hub's own message says so.
test('both self-clearing verdicts retry, and only one of them stands by', () => {
  const asksAgain = (e: unknown) => startRetry(e) !== 'maybe'
  assert.equal(asksAgain({ status: SOURCE_OFFLINE, code: 'source_offline' }), true)
  assert.equal(asksAgain({ status: SESSION_CAP, code: 'session_cap' }), true)
  assert.equal(asksAgain({ status: 409 }), false)
  assert.equal(asksAgain({ status: 500 }), false)

  // The player's stand-by dialog, entering AND leaving, is `wait` alone. It
  // says the machine holding the file has stopped answering and offers one
  // button; a viewer at the stream cap would sit in front of a false cause
  // with nothing to press, so `busy` neither enters it nor keeps them in it.
  // The album queue, which has no dialog and nobody reading it, retries both.
  const standsBy = (e: unknown) => startRetry(e) === 'wait'
  assert.equal(standsBy({ status: SOURCE_OFFLINE, code: 'source_offline' }), true)
  assert.equal(standsBy({ status: SESSION_CAP, code: 'session_cap' }), false)
  assert.equal(asksAgain({ status: SESSION_CAP, code: 'session_cap' }), true)
})
