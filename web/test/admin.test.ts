/// What the admin panel shows about the fleet, and the two things it must
/// never offer: a Delete on something that cannot be deleted, and a Create
/// that the hub will refuse.

import { describe, expect, test } from 'vitest'

import {
  andList,
  canCreate,
  demotesSelf,
  enrolled,
  IN_PROCESS,
  longEnough,
  measuredPair,
  MIN_PASSWORD,
  multiple,
  seesEverything,
} from '../src/domain/admin.ts'

describe('the satellites an operator can act on', () => {
  test('do not include the hub’s own mediahost', () => {
    // It has no certificate to show, nothing to enable or disable, and
    // nothing to revoke — and the Delete it was offered would wipe the index
    // of everything it serves, which on an all-in-one is the whole library.
    const fleet = [{ cert_fingerprint: IN_PROCESS }, { cert_fingerprint: 'ab12' }]
    expect(enrolled(fleet)).toEqual([{ cert_fingerprint: 'ab12' }])
  })

  test('and an empty fleet is empty rather than missing', () => {
    expect(enrolled([])).toEqual([])
  })
})

describe('what a transcoder was measured doing', () => {
  test('is a multiple, to one place', () => {
    expect(multiple(6.234)).toBe('6.2×')
  })

  test('and nothing at all when it was never measured', () => {
    // Zero is "not measured", not "infinitely slow".
    expect(multiple(0)).toBeNull()
    expect(multiple(null)).toBeNull()
    expect(multiple(undefined)).toBeNull()
  })

  test('two resolutions read as a pair', () => {
    expect(measuredPair(6.2, 2.1)).toBe('6.2× / 2.1×')
  })

  test('and one of them alone reads alone', () => {
    expect(measuredPair(6.2, null)).toBe('6.2×')
    expect(measuredPair(null, 2.1)).toBe('2.1×')
    expect(measuredPair(null, null)).toBeNull()
  })
})

describe('creating an account', () => {
  test('needs a name and a long enough password', () => {
    expect(canCreate('claude', 'a'.repeat(MIN_PASSWORD))).toBe(true)
    expect(canCreate('', 'a'.repeat(MIN_PASSWORD))).toBe(false)
    expect(canCreate('   ', 'a'.repeat(MIN_PASSWORD))).toBe(false)
    expect(canCreate('claude', 'short')).toBe(false)
  })

  test('and the length is counted the way the hub counts it', () => {
    // '🔑'.length is 2, so six emoji counted as twelve.
    expect(longEnough('🔑'.repeat(6))).toBe(false)
    expect(longEnough('🔑'.repeat(MIN_PASSWORD))).toBe(true)
  })
})

describe('what an account can see', () => {
  test('an admin has every library, whatever its grants say', () => {
    // Saying so with everyone else's toggle, held on, beats a sentence
    // explaining why there is no toggle here.
    expect(seesEverything({ is_admin: true, all_libraries: false })).toBe(true)
    expect(seesEverything({ is_admin: false, all_libraries: true })).toBe(true)
    expect(seesEverything({ is_admin: false, all_libraries: false })).toBe(false)
  })
})

describe('changing a role', () => {
  test('demoting yourself is the one that ends your session here', () => {
    // The write invalidates the token that authorised it.
    expect(demotesSelf({ username: 'me' }, false, 'me')).toBe(true)
  })

  test('and nothing else is', () => {
    expect(demotesSelf({ username: 'me' }, true, 'me')).toBe(false)
    expect(demotesSelf({ username: 'someone' }, false, 'me')).toBe(false)
  })
})

describe('naming what could not be read', () => {
  test('reads as a sentence, because it is one', () => {
    // "enrolments, satellites and users", not a comma-separated dump: this is
    // the line an operator reads to find out which half of the panel is
    // telling the truth.
    expect(andList([])).toBe('')
    expect(andList(['users'])).toBe('users')
    expect(andList(['satellites', 'users'])).toBe('satellites and users')
    expect(andList(['enrolments', 'satellites', 'users'])).toBe('enrolments, satellites and users')
  })
})
