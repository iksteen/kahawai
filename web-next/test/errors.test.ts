/// The refusal contract, from the client's side.
///
/// The hub answers every 4xx and 5xx with `{code, message}`, and the STATUS —
/// not the code — decides whether asking again could work. These pin that
/// split, because getting it wrong is invisible until a proxy or an outage
/// makes it visible.

import { describe, expect, test } from 'vitest'

import { ApiError, Offline, retry, retryAfter } from '../src/api/errors.ts'
import { apiFailure } from '../src/api/transport.ts'

const respond = (status: number, body: string, headers: Record<string, string> = {}) =>
  new Response(body, { status, headers: { 'content-type': 'application/json', ...headers } })

describe('reading a refusal', () => {
  test('the hub body becomes a status, a code and a message', async () => {
    const e = await apiFailure(respond(429, '{"code":"session_cap","message":"close one first"}'))
    expect(e.status).toBe(429)
    expect(e.code).toBe('session_cap')
    expect(String(e)).toBe('close one first')
  })

  test("a body that is not the hub's keeps its text and carries no code", async () => {
    const e = await apiFailure(
      respond(502, '<html>502 Bad Gateway</html>', { 'content-type': 'text/html' }),
    )
    expect(e.code).toBeUndefined()
    expect(e.message).toMatch(/502 Bad Gateway/)
  })

  test('JSON that is not an error body is not read as one', async () => {
    // A misconfigured proxy returning some other service's JSON must not have
    // one of its fields promoted into a code the app then branches on.
    const e = await apiFailure(respond(503, '{"code":"maintenance"}'))
    expect(e.code).toBeUndefined()
  })

  test('Retry-After comes through only when the hub sent one', async () => {
    const timed = await apiFailure(
      respond(429, '{"code":"login_throttled","message":"wait"}', { 'retry-after': '90' }),
    )
    expect(retryAfter(timed)).toBe(90)
    const untimed = await apiFailure(respond(429, '{"code":"session_cap","message":"x"}'))
    expect(retryAfter(untimed)).toBeUndefined()
  })
})

describe('whether asking again could work', () => {
  const refusal = (status: number, code?: string) => new ApiError(status, 'x', code)

  test('the status decides, not the code', () => {
    expect(retry(refusal(429, 'session_cap'))).toBe(true)
    expect(retry(refusal(500))).toBe(true)
    expect(retry(refusal(409, 'unplayable'))).toBe(false)
    expect(retry(refusal(404, 'not_found'))).toBe(false)
    expect(retry(new Offline())).toBe(true)
  })

  test("a 429 that is not the hub's is not retried", () => {
    // A reverse proxy or WAF rate-limiting the tab answers 429 with an HTML
    // body and no code. Asking again is what such a limiter extends its
    // window for, so an unrecognised 429 is final here.
    expect(retry(refusal(429))).toBe(false)
    expect(retry(refusal(429, 'maintenance'))).toBe(false)
  })

  test('the two 503s an operator has to clear are named, not inferred', () => {
    // A 503 with no code is an intermediary's — HAProxy and ingress-nginx
    // answer it for a backend that is down, which is an ordinary hub restart
    // and does clear. Inferring from the ABSENCE of `source_offline` made
    // every one of those final.
    expect(retry(refusal(503, 'source_offline'))).toBe(true)
    expect(retry(refusal(503))).toBe(true)
    expect(retry(refusal(503, 'setup_required'))).toBe(false)
    expect(retry(refusal(503, 'provider_unconfigured'))).toBe(false)
  })
})
