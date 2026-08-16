import { expect, test } from 'vitest'
import {
  initialHealth,
  isFrozen,
  sessionHealth,
  type SessionEvent,
} from '../src/domain/player-session.ts'
import { playerPhase } from '../src/domain/player-phase.ts'

const run = (...events: SessionEvent[]) => events.reduce(sessionHealth, initialHealth())

test('a restart that answers late is ignored', () => {
  // Two nudges past the produced edge: the second restart owns the timeline, so
  // the first one settling must not clear it.
  const s = run(
    { type: 'timeline-taken', gen: 1 },
    { type: 'timeline-taken', gen: 2 },
    { type: 'restart-settled', gen: 1 },
  )
  expect(s.awaitingGen).toBe(2)
  expect(sessionHealth(s, { type: 'restart-settled', gen: 2 }).awaitingGen).toBe(0)
})

test('giving up on a superseded restart does not kill a live one', () => {
  // The bug this guard exists for: an older POST answering "no" pausing the
  // picture and marking the player dead while a newer restart is still coming.
  const s = run(
    { type: 'timeline-taken', gen: 1 },
    { type: 'timeline-taken', gen: 2 },
    { type: 'gave-up', gen: 1 },
  )
  expect(s.dead).toBe(false)
  expect(s.awaitingGen).toBe(2)
})

test('giving up on the current restart marks it dead and stops waiting', () => {
  const s = run({ type: 'timeline-taken', gen: 3 }, { type: 'gave-up', gen: 3 })
  expect(s.dead).toBe(true)
  expect(s.awaitingGen).toBe(0)
})

test('pressing play clears dead, so the next play can rebuild', () => {
  const dead = run({ type: 'died-while-paused' })
  expect(dead.dead).toBe(true)
  expect(sessionHealth(dead, { type: 'play-pressed' }).dead).toBe(false)
})

test('a second recovery is refused while one is running', () => {
  // Both detectors notice the same death; the second must not start its own.
  const first = run({ type: 'recovery-started' })
  const second = sessionHealth(first, { type: 'recovery-started' })
  expect(second).toBe(first)
  expect(sessionHealth(second, { type: 'recovery-ended' }).recovering).toBe(false)
})

test('an absent host is a wait that holds its position', () => {
  const s = run({ type: 'host-away', atMs: 812_000 })
  expect(s.standby).toBe(812_000)
  expect(s.gone).toBe('')
  expect(playerPhase({ ...s, paused: true })).toBe('standby')
})

test('a real failure during the wait replaces it, rather than sitting behind it', () => {
  // The stand-by loop talked itself out of standing by once; this pins the
  // other direction — when it IS a real failure, the wait must clear.
  const waiting = run({ type: 'host-away', atMs: 500 })
  const s = sessionHealth(waiting, { type: 'stopped', why: 'no such file' })
  expect(s.standby).toBe(null)
  expect(s.gone).toBe('no such file')
  expect(playerPhase({ ...s, paused: true })).toBe('gone')
})

test('try again lifts the stop and nothing else', () => {
  const stopped = run({ type: 'stopped', why: 'it broke' })
  const s = sessionHealth(stopped, { type: 'retry-by-hand' })
  expect(s.gone).toBe('')
  expect(playerPhase({ ...s, paused: false })).toBe('playing')
  // Reachable only from the stopped dialog, so it has no business clearing a
  // wait: the retry loop owns that. Pinned because the first version of this
  // reducer cleared it and no test could tell — `stopped` had already.
  const waiting = run({ type: 'host-away', atMs: 900 })
  expect(sessionHealth(waiting, { type: 'retry-by-hand' }).standby).toBe(900)
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
  expect(playerPhase({ ...both, paused: true })).toBe('standby')
})

test('the transport is frozen for exactly the three phases that outrank a pause', () => {
  expect(isFrozen(initialHealth())).toBe(false)
  expect(isFrozen(run({ type: 'host-away', atMs: 1 }))).toBe(true)
  expect(isFrozen(run({ type: 'stopped', why: 'broke' }))).toBe(true)
  expect(isFrozen(run({ type: 'timeline-taken', gen: 4 }))).toBe(true)
  // Settled: the picture is back and the viewer has it again.
  expect(
    isFrozen(run({ type: 'timeline-taken', gen: 4 }, { type: 'restart-settled', gen: 4 })),
  ).toBe(false)
  // A capability restart is NOT one of them — it keeps its own flag and the
  // transport stays live while the new session is fetched.
  expect(isFrozen(run({ type: 'caps-restart-started' }))).toBe(false)
})

test('a capability restart clears the previous reason before it tries again', () => {
  const failed = run({ type: 'caps-restart-failed', why: 'the mask left no video' })
  expect(failed.capsError).toBe('the mask left no video')
  expect(failed.restarting).toBe(false)
  const again = sessionHealth(failed, { type: 'caps-restart-started' })
  expect(again.capsError).toBe('')
  expect(again.restarting).toBe(true)
})
