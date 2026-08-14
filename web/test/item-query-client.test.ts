import assert from 'node:assert/strict'
import test from 'node:test'

import { configureApiClient } from '../src/api-client.ts'
import { endSession, itemQuery, listLibraries, postProgress } from '../src/generated/kahawai.ts'

test('generated item binding sends RFC 10008 QUERY through the app transport', async () => {
  const originalFetch = globalThis.fetch
  let request: { input: RequestInfo | URL; init?: RequestInit } | undefined
  globalThis.fetch = async (input, init) => {
    request = { input, init }
    return new Response(JSON.stringify({ id: '01POC' }), {
      status: 200,
      headers: { 'content-type': 'application/json' },
    })
  }

  try {
    const item = await itemQuery('01POC', { video_track: 2 })
    assert.equal(item.id, '01POC')
    assert.equal(String(request?.input), '/api/v1/items/01POC')
    assert.equal(request?.init?.method, 'QUERY')
    assert.deepEqual(JSON.parse(String(request?.init?.body)), { video_track: 2 })
  } finally {
    globalThis.fetch = originalFetch
  }
})

test('generated operations share auth retry, empty-body, and raw-status handling', async () => {
  const originalFetch = globalThis.fetch
  let access = 'expired'
  let refreshes = 0
  const calls: { url: string; method: string; bearer: string | null }[] = []
  configureApiClient(
    () => access,
    async () => {
      refreshes++
      access = 'fresh'
      return true
    },
  )
  globalThis.fetch = async (input, init = {}) => {
    const call = {
      url: String(input),
      method: init.method ?? 'GET',
      bearer: new Headers(init.headers).get('authorization'),
    }
    calls.push(call)
    if (call.url === '/api/v1/libraries' && call.bearer === 'Bearer expired')
      return new Response('expired', { status: 401 })
    if (call.url === '/api/v1/libraries')
      return Response.json({ libraries: [{ id: 'L', name: 'Films', media_type: 'movies' }] })
    if (call.url.endsWith('/progress')) return new Response('gone', { status: 404 })
    return new Response(null, { status: 204 })
  }

  try {
    const libraries = await listLibraries()
    assert.equal(libraries.libraries[0]?.id, 'L')
    assert.equal(refreshes, 1)
    assert.deepEqual(
      calls.slice(0, 2).map((call) => call.bearer),
      ['Bearer expired', 'Bearer fresh'],
    )

    const raw = (await postProgress(
      'SESSION',
      { position_ms: 12 },
      { rawResponse: true },
    )) as unknown as Response
    assert.equal(raw.status, 404)
    assert.equal(await endSession('SESSION'), undefined)
  } finally {
    configureApiClient(
      () => null,
      () => Promise.resolve(false),
    )
    globalThis.fetch = originalFetch
  }
})
