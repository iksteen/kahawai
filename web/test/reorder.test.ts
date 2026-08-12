/// Where a newly added language goes in an ordered wishlist.
///
/// `resolveTracks` takes the FIRST language in the list that the file actually
/// has, and the audio list carries a pinned `original` that resolves to the
/// file's own language — so it matches almost everything. Appending after it
/// meant a language the viewer had just added was never reached: a "saved"
/// flash, a tooltip calling original the final fallback, and playback ignoring
/// the setting.

import assert from 'node:assert/strict'
import test from 'node:test'
import { addAbove, moved } from '../src/reorder.ts'

test('the first language added to a bare list outranks the backstop', () => {
  // The default state of a fresh account: nothing stored, so the list is the
  // pin alone. This is the case that was completely inert.
  assert.deepEqual(addAbove(['original'], 'nl', 'original'), ['nl', 'original'])
})

test('an added language goes above the backstop, wherever the backstop sits', () => {
  // `original` is reorderable on purpose, so it is not always last. Moving it
  // to the end to make room would rewrite an order somebody chose.
  assert.deepEqual(addAbove(['nl', 'original', 'en'], 'sv', 'original'), [
    'nl',
    'sv',
    'original',
    'en',
  ])
})

test('a list with no backstop just appends', () => {
  // Subtitle lists have no pin: there is no "original subtitles".
  assert.deepEqual(addAbove(['nl', 'en'], 'sv', 'original'), ['nl', 'en', 'sv'])
})

test('moved is unchanged by all of this', () => {
  assert.deepEqual(moved(['a', 'b', 'c'], 0, 2), ['b', 'c', 'a'])
  assert.equal(moved(['a', 'b'], 1, 1), null)
  assert.equal(moved(['a', 'b'], 0, 5), null)
})

test('a drag from outside the list is refused, not applied', () => {
  // `to` was bounded and `from` was not, so this spliced out nothing and put
  // `undefined` back: ['a','b'] became [undefined,'a','b'] and was saved.
  assert.equal(moved(['a', 'b'], 5, 0), null)
  assert.equal(moved(['a', 'b'], 2, 0), null)
  assert.equal(moved(['a', 'b'], -1, 0), null)
})
