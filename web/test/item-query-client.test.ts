import assert from 'node:assert/strict'
import test from 'node:test'

import { itemQuery } from '../src/generated/kahawai.ts'

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
