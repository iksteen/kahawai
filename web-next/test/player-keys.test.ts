/// The player's keys, as decisions rather than as a switch inside an effect.
///
/// Every case below behaves differently under a plausible wrong version: the
/// two nudges swapped, arrows swallowing the browser's default, Escape claimed
/// while already windowed, the typing guard missing, or a mode key that only
/// enters and never leaves.

import { expect, test } from 'vitest'
import { isTypingTarget, playerIntent, type PlayerMode } from '../src/domain/player-keys.ts'

const at = (key: string, mode: PlayerMode = 'window') => playerIntent(key, { typing: false, mode })

test('space and k both mean pause, and both take the key from the page', () => {
  for (const key of [' ', 'k']) {
    expect(at(key)).toEqual({ intent: { kind: 'toggle-pause' }, preventDefault: true })
  }
})

test('the arrows nudge by different amounts in each direction', () => {
  // Back ten, forward thirty — the same numbers as the transport buttons. A
  // symmetric version passes any test that only checks the sign.
  expect(at('ArrowLeft')?.intent).toEqual({ kind: 'nudge', seconds: -10 })
  expect(at('ArrowRight')?.intent).toEqual({ kind: 'nudge', seconds: 30 })
})

test('the arrows leave the browser its default', () => {
  // They are not prevented today, and prevention is what stops a horizontal
  // scroll — pinning it so nobody "tidies" the two cases into one.
  expect(at('ArrowLeft')?.preventDefault).toBe(false)
  expect(at('ArrowRight')?.preventDefault).toBe(false)
})

test('a size key enters from window and leaves from its own size', () => {
  expect(at('t', 'window')?.intent).toEqual({ kind: 'mode', to: 'theater' })
  expect(at('t', 'theater')?.intent).toEqual({ kind: 'mode', to: 'window' })
  expect(at('f', 'window')?.intent).toEqual({ kind: 'mode', to: 'full' })
  expect(at('f', 'full')?.intent).toEqual({ kind: 'mode', to: 'window' })
})

test('a size key from the OTHER size switches to it rather than to window', () => {
  // The case that tells "toggle against window" apart from "toggle against
  // whatever is current": t while full-screen must go to theater, not out.
  expect(at('t', 'full')?.intent).toEqual({ kind: 'mode', to: 'theater' })
  expect(at('f', 'theater')?.intent).toEqual({ kind: 'mode', to: 'full' })
})

test('Escape leaves a big picture and is not claimed from a small one', () => {
  expect(at('Escape', 'full')?.intent).toEqual({ kind: 'mode', to: 'window' })
  expect(at('Escape', 'theater')?.intent).toEqual({ kind: 'mode', to: 'window' })
  // Nothing to leave: a dialog over the player needs this key.
  expect(at('Escape', 'window')).toBe(null)
})

test('nothing is claimed while typing', () => {
  // Space is the one that bites: it opens a focused <select>, and the audio and
  // subtitle pickers are both selects.
  for (const key of [' ', 'k', 'ArrowLeft', 'ArrowRight', 't', 'f', 'Escape']) {
    expect(playerIntent(key, { typing: true, mode: 'full' })).toBe(null)
  }
})

test('keys the player has no use for are left alone', () => {
  for (const key of ['a', 'Enter', 'Tab', 'ArrowUp', 'ArrowDown', 'F5']) {
    expect(at(key)).toBe(null)
  }
})

test('typing means a field, and a <select> is one', () => {
  expect(isTypingTarget('SELECT')).toBe(true)
  expect(isTypingTarget('INPUT')).toBe(true)
  expect(isTypingTarget('TEXTAREA')).toBe(true)
  // The picture itself, the buttons, and nothing at all.
  expect(isTypingTarget('VIDEO')).toBe(false)
  expect(isTypingTarget('BUTTON')).toBe(false)
  expect(isTypingTarget(undefined)).toBe(false)
  expect(isTypingTarget(null)).toBe(false)
})
