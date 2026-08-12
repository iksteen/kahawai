/// What a sign-out has to survive.
///
/// Every case here is a race, and every one of them was live at some point in
/// this branch. They run against the real `api.ts` rather than a copy of its
/// logic — browser globals stubbed below — because all four bugs were in the
/// ORDER of storage reads and writes around an await, which is exactly what a
/// re-implementation would quietly get right.

import assert from 'node:assert/strict'
import test from 'node:test'

// Set before importing api.ts: it reads none of these at module scope, but the
// functions under test do, and a stub installed later would be racing the
// import in a way that has nothing to do with what is being tested.
const store = new Map<string, string>()
;(globalThis as Record<string, unknown>).localStorage = {
  getItem: (k: string) => store.get(k) ?? null,
  setItem: (k: string, v: string) => void store.set(k, v),
  removeItem: (k: string) => void store.delete(k),
}
;(globalThis as Record<string, unknown>).document = { cookie: '' }

const { refreshTokens, signOut, storeTokens } = await import('../src/api.ts')

type Call = { url: string; bearer: string | null; body: Record<string, string> }

/// A hub that rotates refresh tokens, as the real one does, and refuses
/// `/auth/logout` unless the access token it is given is the current one.
function hub(opts: { accessStale?: boolean } = {}) {
  const calls: Call[] = []
  let generation = 1
  let revoked: string | null = null
  const pair = () => ({
    access_token: `access-${generation}`,
    refresh_token: `refresh-${generation}`,
  })
  ;(globalThis as Record<string, unknown>).fetch = async (url: string, init: RequestInit) => {
    const headers = (init.headers ?? {}) as Record<string, string>
    const body = JSON.parse(String(init.body)) as Record<string, string>
    calls.push({ url, bearer: headers['Authorization'] ?? null, body })
    if (url.endsWith('/auth/refresh')) {
      if (body.refresh_token !== `refresh-${generation}`) return { ok: false, status: 401 }
      generation++
      return { ok: true, status: 200, json: async () => pair() }
    }
    // logout: authenticated, and it revokes only the family whose CURRENT
    // token it is handed — the hub's actual contract, and the reason a retry
    // with a pre-rotation body is answered 204 having done nothing.
    const stale = opts.accessStale && generation === 1
    if (stale || headers['Authorization'] !== `Bearer access-${generation}`) {
      return { ok: false, status: 401 }
    }
    if (body.refresh_token === `refresh-${generation}`) revoked = body.refresh_token
    return { ok: true, status: 204 }
  }
  return { calls, revoked: () => revoked, current: () => `refresh-${generation}` }
}

function signedIn(generation = 1) {
  store.clear()
  store.set('kahawai.access', `access-${generation}`)
  store.set('kahawai.refresh', `refresh-${generation}`)
}

test('a sign-out with a stale access token still revokes the family', async () => {
  // The failure this replaces: `logout` froze the refresh token into the body,
  // `api()` repaired the 401 with a refresh — rotating it — and retried the
  // SAME body. The hub matched nothing and answered 204, so the family stayed
  // usable for its full 30 days while the browser threw away its only copy of
  // the token. Nothing client-side could tell.
  const h = hub({ accessStale: true })
  signedIn()
  await signOut()
  assert.equal(
    h.revoked(),
    h.current(),
    'the family the hub still holds is the one that must be revoked, so the ' +
      'token in the final body has to be the rotated one',
  )
})

test('a sign-out is instant, and the hub is told from a captured pair', async () => {
  let release = () => {}
  const gate = new Promise<void>((r) => (release = r))
  const seen: string[] = []
  ;(globalThis as Record<string, unknown>).fetch = async (url: string) => {
    seen.push(url)
    await gate
    return { ok: true, status: 204 }
  }
  signedIn()
  const done = signOut()
  assert.equal(store.get('kahawai.access'), undefined, 'the tokens are gone before the hub answers')
  assert.equal(seen.length, 1, 'and the call went out anyway, from the copy taken first')
  release()
  await done
})

test('a sign-in during a slow sign-out is not wiped by its answer', async () => {
  // Awaiting `logout` before clearing put a whole round trip — unbounded,
  // against a hub that accepts and stalls — between the sign-in screen
  // appearing and the tokens going. Autofill and Enter fit in it easily, and
  // the late clear then killed the NEW session with no explanation, leaving
  // its family live on the hub.
  let release = () => {}
  const gate = new Promise<void>((r) => (release = r))
  ;(globalThis as Record<string, unknown>).fetch = async () => {
    await gate
    return { ok: true, status: 204 }
  }
  signedIn(1)
  const done = signOut()
  storeTokens({ access_token: 'access-2', refresh_token: 'refresh-2' })
  release()
  await done
  assert.equal(store.get('kahawai.access'), 'access-2', 'the new session survives the old sign-out')
})

test('a refresh that lands after a sign-out cannot resurrect it', async () => {
  let release = () => {}
  const gate = new Promise<void>((r) => (release = r))
  ;(globalThis as Record<string, unknown>).fetch = async () => {
    await gate
    return {
      ok: true,
      status: 200,
      json: async () => ({ access_token: 'access-9', refresh_token: 'refresh-9' }),
    }
  }
  signedIn()
  const refreshing = refreshTokens()
  storeTokens(null, true)
  release()
  assert.equal(await refreshing, false)
  assert.equal(store.get('kahawai.access'), undefined, 'nothing was written back')
  assert.equal(store.get('kahawai.refresh'), undefined)
})

test('a sign-out in another tab stops a refresh in this one', async () => {
  // The counter this replaces lived in module scope, so it could not see a
  // clear from another tab at all — and two tabs is the ordinary case here,
  // not the exotic one. The other tab's clear is just the keys going away.
  let release = () => {}
  const gate = new Promise<void>((r) => (release = r))
  ;(globalThis as Record<string, unknown>).fetch = async () => {
    await gate
    return {
      ok: true,
      status: 200,
      json: async () => ({ access_token: 'access-9', refresh_token: 'refresh-9' }),
    }
  }
  signedIn()
  const refreshing = refreshTokens()
  store.delete('kahawai.access')
  store.delete('kahawai.refresh')
  release()
  assert.equal(await refreshing, false)
  assert.equal(store.get('kahawai.access'), undefined, 'the other tab stays signed out')
})

test('a 401 about a token nobody holds does not sign out the account that does', async () => {
  // The rejection path wrote `storeTokens(null)` unconditionally, so a refresh
  // left over from a previous session could sign out the session that replaced
  // it — a real hazard, since the single-flight promise outlives the clear.
  let release = () => {}
  const gate = new Promise<void>((r) => (release = r))
  ;(globalThis as Record<string, unknown>).fetch = async () => {
    await gate
    return { ok: false, status: 401 }
  }
  signedIn(1)
  const refreshing = refreshTokens()
  storeTokens({ access_token: 'access-2', refresh_token: 'refresh-2' })
  release()
  assert.equal(await refreshing, false)
  assert.equal(store.get('kahawai.access'), 'access-2', 'the live session is untouched')
})

test('a refresh with nothing left in the slot does not call the hub', async () => {
  let called = false
  ;(globalThis as Record<string, unknown>).fetch = async () => {
    called = true
    return { ok: true, status: 200, json: async () => ({}) }
  }
  store.clear()
  assert.equal(await refreshTokens(), false)
  assert.equal(called, false)
})

test('a hub that could not end the session says so, instead of looking done', async () => {
  // A 502 from a proxy in front of a restarting hub is not a revocation. Read
  // as one, the refresh family lives its full thirty days and the viewer —
  // who may be on a shared machine — is told nothing.
  signedIn()
  const said: string[] = []
  ;(globalThis as Record<string, unknown>).fetch = async () => ({ ok: false, status: 502 })
  const { onNotice } = await import('../src/toast.ts')
  onNotice((m: string) => said.push(m))
  await signOut()
  onNotice(null)
  assert.equal(store.get('kahawai.refresh'), undefined, 'the browser still forgets its copies')
  assert.equal(said.length, 1, 'and the failure is reported')
  assert.match(said[0], /502/)
})

test('a rotation while queueing ends this attempt rather than adopting it', async () => {
  // The lock serialises refreshes across tabs; the token is captured before
  // queueing and checked again inside. A tab that rotated while we waited
  // leaves ours stale — asking with theirs would make this call about a
  // session it was never started for, and a 401 on it would sign that one out.
  signedIn(1)
  let asked = 0
  ;(globalThis as Record<string, unknown>).fetch = async () => {
    asked++
    return { ok: false, status: 401 }
  }
  const refreshing = refreshTokens()
  // Another tab's rotation lands while this one queues.
  storeTokens({ access_token: 'access-9', refresh_token: 'refresh-9' })
  assert.equal(await refreshing, false)
  assert.equal(asked, 0, 'the stale attempt never reaches the hub')
  assert.equal(store.get('kahawai.access'), 'access-9', 'and the live session is untouched')
})
