/// The player's keys, as decisions rather than as a switch inside an effect.
///
/// Every case below behaves differently under a plausible wrong version: the
/// two nudges swapped, arrows swallowing the browser's default, Escape claimed
/// while already windowed, the typing guard missing, or a mode key that only
/// enters and never leaves.

import assert from 'node:assert/strict'
import test from 'node:test'
import { isTypingTarget, playerIntent, type PlayerMode } from '../src/player-keys.ts'

const at = (key: string, mode: PlayerMode = 'window') => playerIntent(key, { typing: false, mode })

test('space and k both mean pause, and both take the key from the page', () => {
  for (const key of [' ', 'k']) {
    assert.deepEqual(at(key), { intent: { kind: 'toggle-pause' }, preventDefault: true })
  }
})

test('the arrows nudge by different amounts in each direction', () => {
  // Back ten, forward thirty — the same numbers as the transport buttons. A
  // symmetric version passes any test that only checks the sign.
  assert.deepEqual(at('ArrowLeft')?.intent, { kind: 'nudge', seconds: -10 })
  assert.deepEqual(at('ArrowRight')?.intent, { kind: 'nudge', seconds: 30 })
})

test('the arrows leave the browser its default', () => {
  // They are not prevented today, and prevention is what stops a horizontal
  // scroll — pinning it so nobody "tidies" the two cases into one.
  assert.equal(at('ArrowLeft')?.preventDefault, false)
  assert.equal(at('ArrowRight')?.preventDefault, false)
})

test('a size key enters from window and leaves from its own size', () => {
  assert.deepEqual(at('t', 'window')?.intent, { kind: 'mode', to: 'theater' })
  assert.deepEqual(at('t', 'theater')?.intent, { kind: 'mode', to: 'window' })
  assert.deepEqual(at('f', 'window')?.intent, { kind: 'mode', to: 'full' })
  assert.deepEqual(at('f', 'full')?.intent, { kind: 'mode', to: 'window' })
})

test('a size key from the OTHER size switches to it rather than to window', () => {
  // The case that tells "toggle against window" apart from "toggle against
  // whatever is current": t while full-screen must go to theater, not out.
  assert.deepEqual(at('t', 'full')?.intent, { kind: 'mode', to: 'theater' })
  assert.deepEqual(at('f', 'theater')?.intent, { kind: 'mode', to: 'full' })
})

test('Escape leaves a big picture and is not claimed from a small one', () => {
  assert.deepEqual(at('Escape', 'full')?.intent, { kind: 'mode', to: 'window' })
  assert.deepEqual(at('Escape', 'theater')?.intent, { kind: 'mode', to: 'window' })
  // Nothing to leave: a dialog over the player needs this key.
  assert.equal(at('Escape', 'window'), null)
})

test('nothing is claimed while typing', () => {
  // Space is the one that bites: it opens a focused <select>, and the audio and
  // subtitle pickers are both selects.
  for (const key of [' ', 'k', 'ArrowLeft', 'ArrowRight', 't', 'f', 'Escape']) {
    assert.equal(playerIntent(key, { typing: true, mode: 'full' }), null, key)
  }
})

test('keys the player has no use for are left alone', () => {
  for (const key of ['a', 'Enter', 'Tab', 'ArrowUp', 'ArrowDown', 'F5']) {
    assert.equal(at(key), null, key)
  }
})

test('typing means a field, and a <select> is one', () => {
  assert.equal(isTypingTarget('SELECT'), true)
  assert.equal(isTypingTarget('INPUT'), true)
  assert.equal(isTypingTarget('TEXTAREA'), true)
  // The picture itself, the buttons, and nothing at all.
  assert.equal(isTypingTarget('VIDEO'), false)
  assert.equal(isTypingTarget('BUTTON'), false)
  assert.equal(isTypingTarget(undefined), false)
  assert.equal(isTypingTarget(null), false)
})
