import assert from 'node:assert/strict'
import { beforeEach, test } from 'node:test'

type Call = {
  url: string
  bearer: string | null
  body: Record<string, unknown>
}

type Deferred = {
  promise: Promise<void>
  release: () => void
}

function deferred(): Deferred {
  let release = () => {}
  return {
    promise: new Promise<void>((resolve) => {
      release = resolve
    }),
    release: () => release(),
  }
}

class SerialLocks {
  private tail: Promise<unknown> = Promise.resolve()
  names: string[] = []
  active = 0
  maxActive = 0

  request<T>(_name: string, _options: LockOptions, run: () => Promise<T>): Promise<T> {
    this.names.push(_name)
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

  reset() {
    this.tail = Promise.resolve()
    this.names = []
    this.active = 0
    this.maxActive = 0
  }
}

type BroadcastListener = (event: MessageEvent<unknown>) => void

class TestBroadcastChannel {
  static instances: TestBroadcastChannel[] = []
  messages: unknown[] = []
  private listener: BroadcastListener | null = null
  readonly name: string

  constructor(name: string) {
    this.name = name
    TestBroadcastChannel.instances.push(this)
  }

  addEventListener(_type: 'message', listener: BroadcastListener) {
    this.listener = listener
  }

  postMessage(message: unknown) {
    this.messages.push(message)
  }

  receive(message: unknown) {
    this.listener?.({ data: message } as MessageEvent<unknown>)
  }
}

Object.defineProperty(globalThis, 'window', { configurable: true, value: {} })
Object.defineProperty(globalThis, 'BroadcastChannel', {
  configurable: true,
  value: TestBroadcastChannel,
})

const locks = new SerialLocks()
Object.defineProperty(globalThis, 'navigator', {
  configurable: true,
  value: { locks: locks as unknown as LockManager },
})

const storageOps: string[] = []
Object.defineProperty(globalThis, 'localStorage', {
  configurable: true,
  value: {
    getItem: (key: string) => {
      throw new Error(`credential storage read: ${key}`)
    },
    setItem: (key: string) => {
      throw new Error(`credential storage write: ${key}`)
    },
    removeItem: (key: string) => storageOps.push(`remove:${key}`),
  },
})
Object.defineProperty(globalThis, 'sessionStorage', {
  configurable: true,
  value: {
    getItem: (key: string) => {
      throw new Error(`session storage read: ${key}`)
    },
    setItem: (key: string) => {
      throw new Error(`session storage write: ${key}`)
    },
    removeItem: (key: string) => {
      throw new Error(`session storage removal: ${key}`)
    },
  },
})

const cookieWrites: string[] = []
const downloaded: Array<{ href: string; filename: string }> = []
const documentStub = {
  createElement: (tag: string) => {
    assert.equal(tag, 'a')
    const link = {
      href: '',
      download: '',
      click: () => downloaded.push({ href: link.href, filename: link.download }),
    }
    return link
  },
}
Object.defineProperty(documentStub, 'cookie', {
  get: () => {
    throw new Error('document.cookie was read')
  },
  set: (value: string) => cookieWrites.push(value),
})
Object.defineProperty(globalThis, 'document', { configurable: true, value: documentStub })
// Dynamic by design: api.ts must see the storage/document traps before its
// module body can be evaluated.

const {
  accessToken,
  browserLogin,
  downloadWithAuth,
  refreshTokens,
  restoreSession,
  scrubLegacyCredentials,
  signOut,
} = await import('../src/api.ts')

let calls: Call[] = []

function response(body: Record<string, unknown>, status = 200): Response {
  return new Response(status === 204 ? null : JSON.stringify(body), {
    status,
    headers: status === 204 ? undefined : { 'content-type': 'application/json' },
  })
}

function setHub(handle: (call: Call) => Response | Promise<Response>) {
  ;(globalThis as Record<string, unknown>).fetch = async (
    input: string | URL | Request,
    init: RequestInit = {},
  ) => {
    const headers = (init.headers ?? {}) as Record<string, string>
    const call: Call = {
      url: String(input),
      bearer: headers.Authorization ?? null,
      body: init.body ? (JSON.parse(String(init.body)) as Record<string, unknown>) : {},
    }
    calls.push(call)
    return handle(call)
  }
}

function assertBrowserBodies() {
  for (const call of calls) {
    assert.equal(call.body.client, 'browser', call.url)
    assert.equal('refresh_token' in call.body, false, call.url)
  }
}

beforeEach(async () => {
  setHub(() => response({}, 204))
  await signOut()
  TestBroadcastChannel.instances[0]?.messages.splice(0)
  calls = []
  storageOps.length = 0
  cookieWrites.length = 0
  downloaded.length = 0
  locks.reset()
})

test('sign-out clears access tokens in every open tab', async () => {
  setHub((call) =>
    call.url.endsWith('/auth/logout')
      ? response({}, 204)
      : response({ access_token: 'access-1', expires_in: 900 }),
  )
  await browserLogin('root', 'password-123')
  assert.equal(accessToken(), 'access-1')

  const channel = TestBroadcastChannel.instances[0]
  assert.equal(channel?.name, 'kahawai.auth')
  channel?.receive('sign-out')
  assert.equal(accessToken(), null, 'a peer-tab sign-out clears this tab immediately')

  await browserLogin('root', 'password-123')
  await signOut()
  assert.deepEqual(channel?.messages, ['sign-out'], 'local sign-out notifies peer tabs')
})

test('authenticated download uses the server attachment filename', async () => {
  setHub(
    () =>
      new Response('diagnostics', {
        headers: {
          'content-disposition': 'attachment; filename="kahawai-session-01ABC.log"',
          'content-type': 'text/plain; charset=utf-8',
        },
      }),
  )

  await downloadWithAuth('/admin/v1/sessions/01ABC/log')

  assert.equal(downloaded.length, 1)
  assert.equal(downloaded[0]?.filename, 'kahawai-session-01ABC.log')
})

test('concurrent 401 repair shares one same-tab refresh', async () => {
  setHub((call) =>
    call.url.endsWith('/auth/token')
      ? response({ access_token: 'access-1', expires_in: 900 })
      : response({}, 500),
  )
  await browserLogin('root', 'password-123')
  calls = []

  const gate = deferred()
  let refreshCalls = 0
  setHub(async (call) => {
    assert.match(call.url, /auth\/refresh$/)
    refreshCalls++
    await gate.promise
    return response({ access_token: 'access-2', expires_in: 900 })
  })
  const first = refreshTokens()
  const second = refreshTokens()
  assert.equal(first, second)
  gate.release()
  assert.equal(await first, true)
  assert.equal(await second, true)
  assert.equal(refreshCalls, 1)
  assert.equal(accessToken(), 'access-2')
  assertBrowserBodies()
})

test('refresh, logout, and a new login share one lock and preserve ordering', async () => {
  setHub(() => response({ access_token: 'access-old', expires_in: 900 }))
  await browserLogin('old', 'password-123')
  calls = []
  locks.reset()

  const refreshGate = deferred()
  const refreshStarted = deferred()
  const logoutGate = deferred()
  const logoutStarted = deferred()
  const loginGate = deferred()
  const loginStarted = deferred()
  setHub(async (call) => {
    if (call.url.endsWith('/auth/refresh')) {
      refreshStarted.release()
      await refreshGate.promise
      return response({ access_token: 'access-stale', expires_in: 900 })
    }
    if (call.url.endsWith('/auth/logout')) {
      logoutStarted.release()
      await logoutGate.promise
      return response({}, 204)
    }
    loginStarted.release()
    await loginGate.promise
    return response({ access_token: 'access-new', expires_in: 900 })
  })

  const refreshing = refreshTokens()
  await refreshStarted.promise
  const signingOut = signOut()
  const loggingIn = browserLogin('new', 'password-123')
  assert.equal(accessToken(), null, 'sign-out clears memory before awaiting its lock')

  refreshGate.release()
  await logoutStarted.promise
  assert.deepEqual(
    calls.map((call) => call.url),
    ['/api/v1/auth/refresh', '/api/v1/auth/logout'],
  )
  logoutGate.release()
  await loginStarted.promise
  loginGate.release()

  assert.equal(await refreshing, false, 'the stale refresh generation is rejected')
  await signingOut
  await loggingIn
  assert.equal(accessToken(), 'access-new')
  assert.equal(locks.maxActive, 1)
  assert.deepEqual(locks.names, ['kahawai.auth', 'kahawai.auth', 'kahawai.auth'])
  assertBrowserBodies()
})

test('definitive refresh rejection clears memory; transient failure preserves it', async () => {
  setHub(() => response({ access_token: 'access-1', expires_in: 900 }))
  await browserLogin('root', 'password-123')

  setHub(() => response({ error: 'restart' }, 503))
  assert.equal(await refreshTokens(), false)
  assert.equal(accessToken(), 'access-1')

  setHub(() => {
    throw new TypeError('offline')
  })
  assert.equal(await refreshTokens(), false)
  assert.equal(accessToken(), 'access-1')

  setHub(() => response({}, 401))
  assert.equal(await refreshTokens(), false)
  assert.equal(accessToken(), null)
  assertBrowserBodies()
})

test('boot restore distinguishes anonymous from transient failure', async () => {
  setHub(() => response({}, 401))
  assert.equal(await restoreSession(), 'anonymous')

  setHub(() => response({ error: 'restart' }, 503))
  await assert.rejects(
    restoreSession(),
    (error: unknown) => error instanceof Error && 'status' in error && error.status === 503,
  )

  setHub(() => response({ access_token: 'access-restored', expires_in: 900 }))
  assert.equal(await restoreSession(), 'authenticated')
  assert.equal(accessToken(), 'access-restored')
  assertBrowserBodies()
})

test('stale refresh response cannot resurrect a cleared session', async () => {
  setHub(() => response({ access_token: 'access-1', expires_in: 900 }))
  await browserLogin('root', 'password-123')
  calls = []

  const gate = deferred()
  const started = deferred()
  setHub(async (call) => {
    if (call.url.endsWith('/auth/refresh')) {
      started.release()
      await gate.promise
      return response({ access_token: 'access-stale', expires_in: 900 })
    }
    return response({}, 204)
  })
  const refreshing = refreshTokens()
  await started.promise
  const signingOut = signOut()
  gate.release()
  assert.equal(await refreshing, false)
  await signingOut
  assert.equal(accessToken(), null)
  assertBrowserBodies()
})

test('stale access logout refreshes inside the same lock without installing it', async () => {
  setHub(() => response({ access_token: 'access-old', expires_in: 900 }))
  await browserLogin('root', 'password-123')
  calls = []
  let logoutCalls = 0
  setHub((call) => {
    if (call.url.endsWith('/auth/refresh'))
      return response({ access_token: 'access-fresh', expires_in: 900 })
    logoutCalls++
    return response({}, logoutCalls === 1 ? 401 : 204)
  })
  await signOut()
  assert.equal(accessToken(), null)
  assert.deepEqual(
    calls.map((call) => call.url),
    ['/api/v1/auth/logout', '/api/v1/auth/refresh', '/api/v1/auth/logout'],
  )
  assert.equal(calls[2].bearer, 'Bearer access-fresh')
  assertBrowserBodies()
})

test('credentials are never read from or persisted to browser storage', async () => {
  scrubLegacyCredentials()
  setHub((call) =>
    call.url.endsWith('/auth/logout')
      ? response({}, 204)
      : response({ access_token: 'access-1', expires_in: 900 }),
  )
  await browserLogin('root', 'password-123')
  await refreshTokens()
  await signOut()

  assert.deepEqual(storageOps, ['remove:kahawai.access', 'remove:kahawai.refresh'])
  assert.deepEqual(cookieWrites, ['kahawai_token=; Path=/; Max-Age=0; SameSite=Lax'])
  assertBrowserBodies()
})
