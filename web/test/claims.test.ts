/// Reading the access token's claims. Every case here is one the old
/// implementation got wrong or could get wrong: base64url, UTF-8, and a
/// truthy value that is not `true`.

import { describe, expect, test } from 'vitest'

import { claimsFrom } from '../src/domain/claims.ts'

/// A token whose payload is exactly this, encoded the way a JWT encodes it.
function token(payload: unknown): string {
  const json = new TextEncoder().encode(JSON.stringify(payload))
  const base64 = btoa(String.fromCharCode(...json))
  const url = base64.replaceAll('+', '-').replaceAll('/', '_').replace(/=+$/, '')
  return `header.${url}.signature`
}

describe('a readable token', () => {
  test('gives the name and the admin flag', () => {
    expect(claimsFrom(token({ username: 'claude', admin: true }))).toEqual({
      username: 'claude',
      admin: true,
    })
  })

  test('survives the characters base64url uses and base64 does not', () => {
    // `atob` on the raw segment throws on `-` and `_`, which appear in most
    // payloads of any length. The throw was caught and became "no claims", so
    // an administrator intermittently lost the Admin menu.
    // Both characters, and asserted separately: `[-_]` was satisfied by the
    // `_` alone, so deleting the `-` half of the replacement passed. `>?`
    // produces one of each.
    const payload = { username: 'claude', admin: true, jti: '?\\>' }
    const segment = token(payload).split('.')[1]!
    expect(segment).toMatch(/-/)
    expect(segment).toMatch(/_/)
    expect(claimsFrom(token(payload)).admin).toBe(true)
  })

  test('and a name that is not ASCII', () => {
    // `atob` gives a binary string; reading it directly turns this into
    // mojibake in the header.
    expect(claimsFrom(token({ username: 'Ingmár', admin: false })).username).toBe('Ingmár')
  })

  test('padding is not required, because a JWT does not carry it', () => {
    const segment = token({ username: 'ab', admin: false }).split('.')[1]!
    expect(segment).not.toContain('=')
    expect(claimsFrom(token({ username: 'ab', admin: false })).username).toBe('ab')
  })
})

describe('anything else is nobody', () => {
  test('no token at all', () => {
    expect(claimsFrom(null)).toEqual({ username: '', admin: false })
    expect(claimsFrom(undefined)).toEqual({ username: '', admin: false })
    expect(claimsFrom('')).toEqual({ username: '', admin: false })
  })

  test('a token that is not one', () => {
    expect(claimsFrom('not-a-token')).toEqual({ username: '', admin: false })
    expect(claimsFrom('a..c')).toEqual({ username: '', admin: false })
    expect(claimsFrom('a.!!!!.c')).toEqual({ username: '', admin: false })
  })

  test('a payload that is not an object', () => {
    expect(claimsFrom(token('claude'))).toEqual({ username: '', admin: false })
    expect(claimsFrom(token(null))).toEqual({ username: '', admin: false })
    expect(claimsFrom(token([1, 2]))).toEqual({ username: '', admin: false })
  })

  test('and claims of the wrong shape are not read as claims', () => {
    // Exactly `true`: a token carrying "admin": "no" is not an admin, and any
    // truthiness test says it is.
    expect(claimsFrom(token({ admin: 'no' })).admin).toBe(false)
    expect(claimsFrom(token({ admin: 1 })).admin).toBe(false)
    expect(claimsFrom(token({ username: 7 })).username).toBe('')
  })
})
