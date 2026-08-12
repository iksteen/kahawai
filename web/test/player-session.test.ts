import assert from 'node:assert/strict'
import test from 'node:test'
import { initialHealth, isFrozen, sessionHealth, type SessionEvent } from '../src/player-session.ts'
import { playerPhase } from '../src/player-phase.ts'

const run = (...events: SessionEvent[]) => events.reduce(sessionHealth, initialHealth())

test('a restart that answers late is ignored', () => {
  // Two nudges past the produced edge: the second restart owns the timeline, so
  // the first one settling must not clear it.
  const s = run(
    { type: 'timeline-taken', gen: 1 },
    { type: 'timeline-taken', gen: 2 },
    { type: 'restart-settled', gen: 1 },
  )
  assert.equal(s.awaitingGen, 2)
  assert.equal(sessionHealth(s, { type: 'restart-settled', gen: 2 }).awaitingGen, 0)
})

test('giving up on a superseded restart does not kill a live one', () => {
  // The bug this guard exists for: an older POST answering "no" pausing the
  // picture and marking the player dead while a newer restart is still coming.
  const s = run(
    { type: 'timeline-taken', gen: 1 },
    { type: 'timeline-taken', gen: 2 },
    { type: 'gave-up', gen: 1 },
  )
  assert.equal(s.dead, false)
  assert.equal(s.awaitingGen, 2)
})

test('giving up on the current restart marks it dead and stops waiting', () => {
  const s = run({ type: 'timeline-taken', gen: 3 }, { type: 'gave-up', gen: 3 })
  assert.equal(s.dead, true)
  assert.equal(s.awaitingGen, 0)
})

test('pressing play clears dead, so the next play can rebuild', () => {
  const dead = run({ type: 'died-while-paused' })
  assert.equal(dead.dead, true)
  assert.equal(sessionHealth(dead, { type: 'play-pressed' }).dead, false)
})

test('a second recovery is refused while one is running', () => {
  // Both detectors notice the same death; the second must not start its own.
  const first = run({ type: 'recovery-started' })
  const second = sessionHealth(first, { type: 'recovery-started' })
  assert.equal(second, first, 'no new state, so nothing re-renders or re-enters')
  assert.equal(sessionHealth(second, { type: 'recovery-ended' }).recovering, false)
})

test('an absent host is a wait that holds its position', () => {
  const s = run({ type: 'host-away', atMs: 812_000 })
  assert.equal(s.standby, 812_000)
  assert.equal(s.gone, '', 'a wait is not a stop')
  assert.equal(playerPhase({ ...s, paused: true }), 'standby')
})

test('a real failure during the wait replaces it, rather than sitting behind it', () => {
  // The stand-by loop talked itself out of standing by once; this pins the
  // other direction — when it IS a real failure, the wait must clear.
  const waiting = run({ type: 'host-away', atMs: 500 })
  const s = sessionHealth(waiting, { type: 'stopped', why: 'no such file' })
  assert.equal(s.standby, null)
  assert.equal(s.gone, 'no such file')
  assert.equal(playerPhase({ ...s, paused: true }), 'gone')
})

test('try again lifts the stop and nothing else', () => {
  const stopped = run({ type: 'stopped', why: 'it broke' })
  const s = sessionHealth(stopped, { type: 'retry-by-hand' })
  assert.equal(s.gone, '')
  assert.equal(playerPhase({ ...s, paused: false }), 'playing')
  // Reachable only from the stopped dialog, so it has no business clearing a
  // wait: the retry loop owns that. Pinned because the first version of this
  // reducer cleared it and no test could tell — `stopped` had already.
  const waiting = run({ type: 'host-away', atMs: 900 })
  assert.equal(sessionHealth(waiting, { type: 'retry-by-hand' }).standby, 900)
})

// `restarting` here is the reducer's own caps flag. The component does not
// feed it to `playerPhase` — it passes `awaitingGen !== 0` — so this pins the
// ranking function, not a state the app can be in.
test('a wait outranks a stop, and a stop outranks a restart', () => {
  // The ranking lives in playerPhase; this pins that the machine can produce
  // the combinations it ranks.
  const both = run(
    { type: 'stopped', why: 'broke' },
    { type: 'host-away', atMs: 1 },
    { type: 'caps-restart-started' },
  )
  assert.equal(playerPhase({ ...both, paused: true }), 'standby')
})

test('the transport is frozen for exactly the three phases that outrank a pause', () => {
  assert.equal(isFrozen(initialHealth()), false)
  assert.equal(isFrozen(run({ type: 'host-away', atMs: 1 })), true)
  assert.equal(isFrozen(run({ type: 'stopped', why: 'broke' })), true)
  assert.equal(isFrozen(run({ type: 'timeline-taken', gen: 4 })), true)
  // Settled: the picture is back and the viewer has it again.
  assert.equal(
    isFrozen(run({ type: 'timeline-taken', gen: 4 }, { type: 'restart-settled', gen: 4 })),
    false,
  )
  // A capability restart is NOT one of them — it keeps its own flag and the
  // transport stays live while the new session is fetched.
  assert.equal(isFrozen(run({ type: 'caps-restart-started' })), false)
})

test('a capability restart clears the previous reason before it tries again', () => {
  const failed = run({ type: 'caps-restart-failed', why: 'the mask left no video' })
  assert.equal(failed.capsError, 'the mask left no video')
  assert.equal(failed.restarting, false)
  const again = sessionHealth(failed, { type: 'caps-restart-started' })
  assert.equal(again.capsError, '', 'the old reason is not the answer to the new attempt')
  assert.equal(again.restarting, true)
})
