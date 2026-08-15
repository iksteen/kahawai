/// The error taxonomy, which is one table and therefore one place to be wrong.
///
/// Each case here is a distinction the old client had to make and got wrong at
/// least once: a 403 that signed people out, a proxy's 503 treated as final, a
/// blocked condition dressed up as something to wait for.

import { describe, expect, test } from 'vitest'

import { ApiError, Offline } from '../src/api/errors.ts'
import { endsSession, kindOf, reportFor } from '../src/domain/refusal.ts'

const refusal = (status: number, code?: string) => new ApiError(status, 'x', code)

describe('what a refusal is', () => {
  test('the hub not answering is not a fact about the request', () => {
    expect(kindOf(new Offline())).toBe('offline')
  })

  test('403 is denied, not signed out', () => {
    // Re-authenticating as the same person changes nothing, so signing them
    // out for it is a loop on a page they simply may not open.
    expect(kindOf(refusal(403, 'forbidden'))).toBe('denied')
    expect(kindOf(refusal(403, 'admin_required'))).toBe('denied')
    expect(endsSession(refusal(403, 'forbidden'))).toBe(false)
    expect(endsSession(refusal(401, 'unauthenticated'))).toBe(true)
  })

  test('the two 503s an operator must clear are blocked, not transient', () => {
    // Waiting never resolves these, and a stand-by dialog about a machine
    // that is answering perfectly well is worse than saying nothing.
    expect(kindOf(refusal(503, 'setup_required'))).toBe('blocked')
    expect(kindOf(refusal(503, 'provider_unconfigured'))).toBe('blocked')
    // Everything else on 503 does clear, including an intermediary's own,
    // which carries no code at all.
    expect(kindOf(refusal(503, 'source_offline'))).toBe('transient')
    expect(kindOf(refusal(503))).toBe('transient')
  })

  test('a stream cap waits and an unplayable item does not', () => {
    expect(kindOf(refusal(429, 'session_cap'))).toBe('transient')
    expect(kindOf(refusal(409, 'unplayable'))).toBe('refused')
  })

  test("a 429 that is not the hub's is not something to wait out", () => {
    // A proxy or WAF rate-limiting the tab. Asking again is what extends its
    // window.
    expect(kindOf(refusal(429))).toBe('refused')
  })

  test('a 5xx is ours, and 503 is the exception because it clears', () => {
    expect(kindOf(refusal(500, 'internal'))).toBe('broken')
    expect(kindOf(refusal(502, 'provider_error'))).toBe('broken')
    expect(kindOf(refusal(503, 'source_offline'))).toBe('transient')
  })

  test('anything that is not a refusal at all is ours', () => {
    expect(kindOf(new TypeError('x.map is not a function'))).toBe('broken')
    expect(kindOf(undefined)).toBe('broken')
  })
})

describe('where a report belongs', () => {
  test('UI-21: the control that caused it decides', () => {
    // A button the person just pressed is still there, and pressing it again
    // IS the retry — so a toast with its own action would duplicate it.
    expect(reportFor('action')).toBe('notice')
    // Content that failed to arrive has no control to press, so the retry has
    // to be anchored where the content is absent.
    expect(reportFor('content')).toBe('inline')
  })
})
