import test from 'node:test'
import assert from 'node:assert/strict'
import { playerPhase } from '../src/player-phase.ts'

/// The four overlays are ranked, so a state that satisfies two of them shows
/// only the higher. These are the pairs that were reachable together before
/// the rule was written down.

test('a wait outranks a stop', () => {
  // The stand-by retry loop sets `standby` and clears `gone` in the same
  // batch, but a render between the two used to show both dialogs stacked.
  assert.equal(
    playerPhase({
      standby: 90_000,
      gone: 'the host stopped answering',
      restarting: false,
      paused: true,
    }),
    'standby',
  )
})

test('a stop outranks a restart, and hides the play button', () => {
  // The bug this rule exists for: the play veil's condition tested
  // `standby === null` and never `gone`, so an unrecoverable failure rendered
  // a play circle behind its own dialog — visible through the scrim, and
  // unclickable under a z-index of 40.
  assert.equal(
    playerPhase({
      standby: null,
      gone: 'it restarted once and stopped again',
      restarting: true,
      paused: true,
    }),
    'gone',
  )
})

test('a restart outranks the pause it performed itself', () => {
  // Every restart pauses the element before asking the hub. Reporting that as
  // the viewer's pause is what put a play button over a restarting picture.
  assert.equal(
    playerPhase({ standby: null, gone: '', restarting: true, paused: true }),
    'restarting',
  )
})

test('the element decides only when nothing else is happening', () => {
  assert.equal(playerPhase({ standby: null, gone: '', restarting: false, paused: true }), 'paused')
  assert.equal(
    playerPhase({ standby: null, gone: '', restarting: false, paused: false }),
    'playing',
  )
})

test('standing by at position zero is still standing by', () => {
  // `standby` holds a resume position, so 0 is a legitimate value and the
  // check has to be against null rather than falsiness.
  assert.equal(playerPhase({ standby: 0, gone: '', restarting: false, paused: true }), 'standby')
})

test('a restart outranks playing, not just pausing', () => {
  // The gap: every other assertion pairs `restarting` with `paused: true`, so
  // `restarting && paused` would satisfy them all — and would drop the veil the
  // moment `playing` fires while a restart is still outstanding.
  assert.equal(
    playerPhase({ standby: null, gone: '', restarting: true, paused: false }),
    'restarting',
  )
})
