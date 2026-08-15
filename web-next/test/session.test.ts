/// The auth session, and specifically the orderings that are invisible until
/// they are wrong: two tabs racing a refresh, a sign-out overtaking one, and a
/// response that lands after the session it belonged to has ended.
///
/// Ported from the first implementation's suite, which is the specification
/// for this — every case here is a bug that was found once.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

import type { AuthWire } from '../src/api/session.ts'
import {
  accessToken,
  browserLogin,
  onTokensCleared,
  refreshTokens,
  restoreSession,
  scrubLegacyCredentials,
  signOut,
  startAuthSession,
  stopAuthSession,
} from '../src/api/session.ts'

/// A promise somebody else decides when to settle, so a test can hold a
/// request open and start another one across it.
function deferred() {
  let release = () => {}
  const promise = new Promise<void>((resolve) => {
    release = resolve
  })
  return { promise, release: () => release() }
}

function refusal(status: number) {
  return Object.assign(new Error(`HTTP ${status}`), { status })
}

/// One lock, held in order — what `navigator.locks` gives a real browser, and
/// what the assertions about ordering depend on.
class SerialLocks {
  private tail: Promise<unknown> = Promise.resolve()
  names: string[] = []
  active = 0
  maxActive = 0

  request<T>(name: string, _options: unknown, run: () => Promise<T>): Promise<T> {
    this.names.push(name)
    const result = this.tail.then(async () => {
      this.active++
      this.maxActive = Math.max(this.maxActive, this.active)
      try {
        return await run()
      } finally {
        this.active--
      }
    })
    this.tail = result.catch(() => undefined)
    return result
  }
}

type Call = { kind: 'login' | 'refresh' | 'logout'; bearer?: string }

let calls: Call[] = []
let locks: SerialLocks

/// The hub, as three functions. Each test replaces the ones it cares about.
function hub(overrides: Partial<AuthWire> = {}): AuthWire {
  return {
    login: async () => {
      calls.push({ kind: 'login' })
      return { access_token: 'access-1', expires_in: 900 }
    },
    refresh: async () => {
      calls.push({ kind: 'refresh' })
      return { access_token: 'refreshed', expires_in: 900 }
    },
    logout: async (bearer) => {
      calls.push({ kind: 'logout', bearer })
    },
    ...overrides,
  }
}

beforeEach(() => {
  calls = []
  locks = new SerialLocks()
  vi.stubGlobal('navigator', { locks })
  vi.useFakeTimers()
})

afterEach(() => {
  stopAuthSession()
  vi.useRealTimers()
  vi.unstubAllGlobals()
})

describe('refresh timing', () => {
  test('the server lifetime refreshes an opaque token before expiry', async () => {
    startAuthSession(hub())
    await browserLogin('root', 'password-123')
    expect(accessToken()).toBe('access-1')

    // Nothing yet: the lifetime is 15 minutes and the lead is one.
    await vi.advanceTimersByTimeAsync(13 * 60_000)
    expect(calls.filter((c) => c.kind === 'refresh')).toHaveLength(0)

    await vi.advanceTimersByTimeAsync(60_000)
    expect(calls.filter((c) => c.kind === 'refresh')).toHaveLength(1)
    expect(accessToken()).toBe('refreshed')

    // And it schedules the next one from the new lifetime rather than stopping.
    await vi.advanceTimersByTimeAsync(14 * 60_000)
    expect(calls.filter((c) => c.kind === 'refresh')).toHaveLength(2)
  })

  test('a transient failure retries; a refusal ends the session', async () => {
    let fail: unknown = null
    startAuthSession(
      hub({
        refresh: async () => {
          calls.push({ kind: 'refresh' })
          if (fail) throw fail
          return { access_token: 'refreshed', expires_in: 900 }
        },
      }),
    )
    await browserLogin('root', 'password-123')

    fail = refusal(503)
    expect(await refreshTokens()).toBe(false)
    expect(accessToken()).toBe('access-1')

    fail = new TypeError('offline')
    expect(await refreshTokens()).toBe(false)
    expect(accessToken()).toBe('access-1')

    fail = refusal(401)
    expect(await refreshTokens()).toBe(false)
    expect(accessToken()).toBeNull()
  })
})

describe('one at a time', () => {
  test('concurrent callers share a single refresh', async () => {
    const gate = deferred()
    startAuthSession(
      hub({
        refresh: async () => {
          calls.push({ kind: 'refresh' })
          await gate.promise
          return { access_token: 'access-2', expires_in: 900 }
        },
      }),
    )
    await browserLogin('root', 'password-123')

    const first = refreshTokens()
    const second = refreshTokens()
    expect(first).toBe(second)
    gate.release()
    expect(await first).toBe(true)
    expect(calls.filter((c) => c.kind === 'refresh')).toHaveLength(1)
    expect(accessToken()).toBe('access-2')
  })

  test('a refresh, a sign-out and a login keep their order and never overlap', async () => {
    const refreshing = deferred()
    const started = deferred()
    startAuthSession(
      hub({
        refresh: async () => {
          calls.push({ kind: 'refresh' })
          started.release()
          await refreshing.promise
          return { access_token: 'access-stale', expires_in: 900 }
        },
      }),
    )
    await browserLogin('old', 'password-123')
    calls = []

    const refresh = refreshTokens()
    await started.promise
    const out = signOut()
    const login = browserLogin('new', 'password-123')
    // Memory is cleared before the lock is even asked for: nothing queued
    // behind it may use the credential this is destroying.
    expect(accessToken()).toBeNull()

    refreshing.release()
    // The stale refresh belongs to a session that has ended, so its answer is
    // dropped rather than installed.
    expect(await refresh).toBe(false)
    await out
    await login

    expect(accessToken()).toBe('access-1')
    expect(locks.maxActive).toBe(1)
    expect(calls.map((c) => c.kind)).toEqual(['refresh', 'logout', 'login'])
  })

  test('a response landing after a sign-out cannot resurrect the session', async () => {
    const gate = deferred()
    const started = deferred()
    startAuthSession(
      hub({
        refresh: async () => {
          started.release()
          await gate.promise
          return { access_token: 'access-stale', expires_in: 900 }
        },
      }),
    )
    await browserLogin('root', 'password-123')

    const refresh = refreshTokens()
    await started.promise
    const out = signOut()
    gate.release()
    expect(await refresh).toBe(false)
    await out
    expect(accessToken()).toBeNull()
  })
})

describe('signing out', () => {
  test('an expired bearer is refreshed inside the lock, and not installed', async () => {
    let logouts = 0
    startAuthSession(
      hub({
        logout: async (bearer) => {
          calls.push({ kind: 'logout', bearer })
          // The first attempt uses a bearer that expired while waiting.
          if (++logouts === 1) throw refusal(401)
        },
        refresh: async () => {
          calls.push({ kind: 'refresh' })
          return { access_token: 'access-fresh', expires_in: 900 }
        },
      }),
    )
    await browserLogin('root', 'password-123')
    calls = []

    await signOut()
    expect(calls.map((c) => c.kind)).toEqual(['logout', 'refresh', 'logout'])
    expect(calls[2]?.bearer).toBe('access-fresh')
    // Refreshed to revoke, never installed: this session is ending.
    expect(accessToken()).toBeNull()
  })

  test('the session reports WHY it ended', async () => {
    // Two different screens: signing out is something you did and needs no
    // explanation, while a session that ended by itself has to say so.
    const ended: boolean[] = []
    let fail: unknown = null
    startAuthSession(
      hub({
        refresh: async () => {
          if (fail) throw fail
          return { access_token: 'refreshed', expires_in: 900 }
        },
      }),
    )
    onTokensCleared((deliberate) => ended.push(deliberate))

    await browserLogin('root', 'password-123')
    await signOut()
    expect(ended).toEqual([true])

    await browserLogin('root', 'password-123')
    fail = refusal(401)
    await refreshTokens()
    expect(ended).toEqual([true, false])
  })

  test('a peer tab signing out ends this one', async () => {
    // The message carries no credential — it is a fact, not a secret — and it
    // has to arrive as a deliberate ending, because the other tab's person
    // asked for it.
    const ended: boolean[] = []
    startAuthSession(hub())
    onTokensCleared((deliberate) => ended.push(deliberate))
    await browserLogin('root', 'password-123')

    const peer = new BroadcastChannel('kahawai.auth')
    peer.postMessage('sign-out')
    await vi.waitFor(() => expect(accessToken()).toBeNull())
    expect(ended).toEqual([true])
    peer.close()
  })

  test('no credential is ever written to storage', async () => {
    const writes: string[] = []
    vi.stubGlobal('localStorage', {
      getItem: (k: string) => {
        throw new Error(`credential storage read: ${k}`)
      },
      setItem: (k: string) => {
        throw new Error(`credential storage write: ${k}`)
      },
      removeItem: (k: string) => writes.push(`remove:${k}`),
    })
    const cookies: string[] = []
    vi.stubGlobal('document', {
      set cookie(value: string) {
        cookies.push(value)
      },
      get cookie(): string {
        throw new Error('document.cookie was read')
      },
    })

    startAuthSession(hub())
    scrubLegacyCredentials()
    await browserLogin('root', 'password-123')
    await refreshTokens()
    await signOut()

    // Only removals, and only of what an older build left behind.
    expect(writes).toEqual(['remove:kahawai.access', 'remove:kahawai.refresh'])
    expect(cookies).toEqual(['kahawai_token=; Path=/; Max-Age=0; SameSite=Lax'])
  })
})

describe('restoring on a reload', () => {
  test('a refusal is anonymous; a hub that is down is not', async () => {
    let fail: unknown = refusal(401)
    startAuthSession(
      hub({
        refresh: async () => {
          if (fail) throw fail
          return { access_token: 'access-restored', expires_in: 900 }
        },
      }),
    )

    expect(await restoreSession()).toBe('anonymous')

    // Conflating these sends a signed-in viewer to a password box over one
    // blip, with credentials that are perfectly good.
    fail = refusal(503)
    await expect(restoreSession()).rejects.toMatchObject({ status: 503 })

    fail = null
    expect(await restoreSession()).toBe('authenticated')
    expect(accessToken()).toBe('access-restored')
  })
})
