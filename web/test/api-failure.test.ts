/// The hub answers every 4xx and 5xx with `{code, message}`. What reaches a
/// view is an `ApiError` carrying both — and it has to survive a body that is
/// not the hub's, because a reverse proxy in front of the hub answers its own
/// failures with HTML and that text is still the only clue on screen.

import { strict as assert } from 'node:assert'
import { test } from 'node:test'
import { apiFailure } from '../src/api-client.ts'

const respond = (status: number, body: string, contentType = 'application/json') =>
  new Response(body, { status, headers: { 'content-type': contentType } })

test('the hub body becomes a status, a code and a message', async () => {
  const error = await apiFailure(
    respond(429, '{"code":"session_cap","message":"too many concurrent streams; close one first"}'),
  )
  assert.equal(error.status, 429)
  assert.equal(error.code, 'session_cap')
  assert.equal(error.message, 'too many concurrent streams; close one first')
  // What a view prints. The code decides what to SAY, never what is shown raw.
  assert.equal(String(error), 'too many concurrent streams; close one first')
})

test("a body that is not the hub's keeps its text and carries no code", async () => {
  const error = await apiFailure(
    respond(502, '<html><body>502 Bad Gateway</body></html>', 'text/html'),
  )
  assert.equal(error.status, 502)
  assert.equal(error.code, undefined)
  assert.match(error.message, /502 Bad Gateway/)
})

test('an empty body still says something', async () => {
  const error = await apiFailure(respond(500, '', 'text/plain'))
  assert.equal(error.message, '500')
  assert.equal(error.code, undefined)
})

test('JSON that is not an error body is not read as one', async () => {
  // A misconfigured proxy returning some other service's JSON must not have
  // one of its fields promoted into a code the app then branches on.
  const error = await apiFailure(respond(503, '{"code":"maintenance"}'))
  assert.equal(error.code, undefined)
  assert.equal(error.message, '{"code":"maintenance"}')
})

test('a code that is not a string is dropped, and the message survives', async () => {
  const error = await apiFailure(respond(400, '{"code":7,"message":"bad track id"}'))
  assert.equal(error.code, undefined)
  assert.equal(error.message, 'bad track id')
})

/// The distinction the whole change exists for: two refusals of the same
/// playback request, one of which clears on its own. A client decides that
/// from the STATUS, so it needs no table of kahawai's codes.
test('transience is readable from the status alone', async () => {
  const cap = await apiFailure(respond(429, '{"code":"session_cap","message":"close one first"}'))
  const dead = await apiFailure(
    respond(409, '{"code":"unplayable","message":"no playable source"}'),
  )
  const retryable = (e: { status: number }) =>
    e.status === 429 || e.status === 503 || e.status >= 500
  assert.equal(retryable(cap), true)
  assert.equal(retryable(dead), false)
})
