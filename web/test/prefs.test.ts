/// The settings' rules about a stored string. Each of these mirrors a parse
/// the hub already performs — the client's job is to write back something the
/// hub reads the same way it reads what it sent.

import { describe, expect, test } from 'vitest'

import {
  assLadder,
  bandwidthValue,
  ORIGINAL,
  stored,
  validToken,
  wishlist,
} from '../src/domain/prefs.ts'

describe('a language token', () => {
  test('is a two- or three-letter code, or the backstop', () => {
    expect(validToken('en')).toBe(true)
    expect(validToken('nld')).toBe(true)
    expect(validToken('original')).toBe(true)
  })

  test('and nothing else', () => {
    expect(validToken('e')).toBe(false)
    expect(validToken('engl')).toBe(false)
    expect(validToken('EN!')).toBe(false)
    expect(validToken('')).toBe(false)
  })

  test('typed with spaces or capitals, it is still one', () => {
    expect(validToken(' EN ')).toBe(true)
  })
})

describe('a wishlist', () => {
  test('reads as the list it was stored as', () => {
    expect(wishlist('en,nl', 'subs')).toEqual(['en', 'nl'])
  })

  test('and an empty one is empty rather than one blank entry', () => {
    expect(wishlist('', 'subs')).toEqual([])
  })

  test('audio always carries the backstop', () => {
    // It resolves to the file's own language, so it is what makes the list
    // total — and it is not removable.
    expect(wishlist('en', 'audio')).toEqual(['en', ORIGINAL])
  })

  test('wherever the viewer has put it', () => {
    // Reorderable on purpose: another language may be preferred above it, and
    // moving it to the end would rewrite an order somebody chose.
    expect(wishlist('original,en', 'audio')).toEqual([ORIGINAL, 'en'])
  })

  test('and subtitles do not get one', () => {
    // "The file's own language" is not a subtitle anybody asked for.
    expect(wishlist('en', 'subs')).toEqual(['en'])
  })

  test('and it goes back in the shape it came out', () => {
    expect(stored(['en', 'nl'])).toBe('en,nl')
    expect(stored([])).toBe('')
  })
})

describe('the styled-subtitle ladder', () => {
  test('is always every rung, in the order stored', () => {
    expect(assLadder('burn,overlay,flatten')).toEqual(['burn', 'overlay', 'flatten'])
  })

  test('and whatever is missing is appended, as the hub does', () => {
    // A ladder short of a rung would offer a preference the hub does not have.
    expect(assLadder('burn')).toEqual(['burn', 'flatten', 'overlay'])
    expect(assLadder('')).toEqual(['flatten', 'overlay', 'burn'])
  })

  test('junk in the stored value is ignored rather than shown', () => {
    expect(assLadder('burn,nonsense')).toEqual(['burn', 'flatten', 'overlay'])
  })

  test('and a rung named twice is one rung', () => {
    expect(assLadder('burn,burn')).toEqual(['burn', 'flatten', 'overlay'])
  })
})

describe('the bandwidth ceiling', () => {
  test('is a whole number of kbps', () => {
    expect(bandwidthValue('4000')).toBe('4000')
    expect(bandwidthValue(' 4000 ')).toBe('4000')
  })

  test('and empty is no ceiling, which is a real answer', () => {
    // Not a missing one: stored as an empty string rather than as a zero.
    expect(bandwidthValue('')).toBe('')
    expect(bandwidthValue('   ')).toBe('')
  })

  test('anything that is not a number is refused rather than stored', () => {
    expect(bandwidthValue('lots')).toBeNull()
    expect(bandwidthValue('-1')).toBeNull()
    expect(bandwidthValue('0')).toBeNull()
    expect(bandwidthValue('4000.5')).toBeNull()
  })
})
