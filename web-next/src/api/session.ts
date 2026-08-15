/// The browser's half of authentication.
///
/// The hub issues a short-lived access token that lives ONLY in memory, plus
/// two HttpOnly cookies it manages itself: `kahawai_refresh` (path
/// `/api/v1/auth`) and `kahawai_media` (path `/api/v1`, the same lifetime as
/// the access token). Nothing here writes a credential to storage, and
/// `scrubLegacyCredentials` exists to remove ones an older build did.
///
/// The media cookie is why refresh timing matters beyond ordinary requests:
/// `<img>`, `<video>` and `EventSource` cannot set an Authorization header, so
/// they ride that cookie, and the hub re-sets it on every login and refresh.
/// Refreshing ahead of expiry rotates both credentials before either kind of
/// request fails — see `domain/token.ts`.
///
/// Everything below is ported behaviour-for-behaviour from the first
/// implementation, whose tests are the specification. What changed is that
/// starting is explicit: the channel and the transport wiring used to be
/// module side effects, which is why testing them needed a dynamic import
/// after stubbing globals.

import { configureApiClient } from './transport.ts'
import { REFRESH_RETRY_MS, refreshDelayMs } from '../domain/token.ts'

/// A refresh must not hang for ever: the app renders nothing until the boot
/// restore answers, and the player latches `recovering` across one.
const REFRESH_TIMEOUT_MS = 15_000
/// Long enough that a peer tab's slow refresh is waited for, short enough that
/// a wedged one does not freeze this tab's boot.
const LOCK_WAIT_MS = 20_000

export type RestoreResult = 'authenticated' | 'anonymous'

/// What the session needs from the hub. Injected rather than imported so this
/// module has no cycle with the generated bindings, and so the tests can drive
/// it without a fetch stub.
export type AuthWire = {
  login: (
    username: string,
    password: string,
  ) => Promise<{ access_token: string; expires_in: number }>
  refresh: () => Promise<{ access_token: string; expires_in: number }>
  logout: (bearer: string) => Promise<void>
}

let wire: AuthWire | null = null
let access: string | null = null
/// Bumped by every deliberate change of session. A response that was in flight
/// across one belongs to a session that no longer exists, and installing it
/// would resurrect a signed-out account — see `installAccess`.
let generation = 0
let refreshTimer: ReturnType<typeof setTimeout> | undefined
let inFlight: Promise<boolean> | null = null
let cleared: ((deliberate: boolean) => void) | null = null
let channel: BroadcastChannel | null = null

export function accessToken(): string | null {
  return access
}

/// Told when the session ends, with whether the person asked for it.
export function onTokensCleared(callback: ((deliberate: boolean) => void) | null) {
  cleared = callback
}

function installAccess(token: string, expiresInSecs: number, expected: number): boolean {
  if (generation !== expected) return false
  access = token
  scheduleRefresh(refreshDelayMs(expiresInSecs * 1000))
  return true
}

function clearAccess(deliberate = false, expected?: number): boolean {
  if (expected !== undefined && generation !== expected) return false
  const had = access !== null
  generation++
  access = null
  inFlight = null
  clearTimeout(refreshTimer)
  if (had) cleared?.(deliberate)
  return true
}

function scheduleRefresh(delayMs: number) {
  clearTimeout(refreshTimer)
  refreshTimer = setTimeout(() => {
    void refreshTokens().then((ok) => {
      // A transient failure leaves the token in place, so there is still
      // something to refresh; a definitive one cleared it and there is not.
      if (!ok && accessToken()) scheduleRefresh(REFRESH_RETRY_MS)
    })
  }, delayMs)
}

/// One tab at a time, across the whole origin.
///
/// Two tabs refreshing at once both spend the same rotating refresh token, and
/// the loser's is revoked as a replay — signing out a session that was working
/// a moment earlier. The Web Locks API is the only thing that orders them.
/// Absent (an old browser, a test), the callback simply runs.
async function alone<T>(run: () => Promise<T>): Promise<T> {
  const locks = typeof navigator === 'undefined' ? undefined : navigator.locks
  if (!locks) return run()
  return locks.request('kahawai.auth', { signal: AbortSignal.timeout(LOCK_WAIT_MS) }, run)
}

async function rotate(started: number, throwTransient: boolean): Promise<boolean> {
  if (generation !== started) return false
  try {
    const fresh = await withTimeout(wire!.refresh())
    return installAccess(fresh.access_token, fresh.expires_in, started)
  } catch (error) {
    // A refusal is the session being over. Anything else — a restart, a
    // dropped link — leaves the token alone, because it says nothing about
    // whether the session is still good.
    const status = (error as { status?: number }).status
    if (status === 401 || status === 403) {
      clearAccess(false, started)
      return false
    }
    if (throwTransient) throw error
    return false
  }
}

function withTimeout<T>(work: Promise<T>): Promise<T> {
  return Promise.race([
    work,
    new Promise<never>((_, reject) =>
      setTimeout(() => reject(new Error('the hub did not answer in time')), REFRESH_TIMEOUT_MS),
    ),
  ])
}

/// Refresh, sharing one request with any caller that asks while it is out.
export function refreshTokens(): Promise<boolean> {
  if (!access) return Promise.resolve(false)
  const started = generation
  inFlight ??= alone(() => rotate(started, false))
    .catch(() => false)
    .finally(() => {
      inFlight = null
    })
  return inFlight
}

/// What a reload does: no access token in memory, but the refresh cookie may
/// still be good.
///
/// A transient failure THROWS rather than answering "anonymous". Conflating
/// "the hub did not answer" with "you are not signed in" sends a signed-in
/// viewer to a password box over one blip, with their credentials perfectly
/// good — and there is nothing to sign in to while the hub is unreachable, so
/// the one thing that screen offers cannot work either.
export async function restoreSession(): Promise<RestoreResult> {
  const started = generation
  return (await alone(() => rotate(started, true))) ? 'authenticated' : 'anonymous'
}

export async function browserLogin(username: string, password: string): Promise<void> {
  const started = ++generation
  await alone(async () => {
    const session = await wire!.login(username, password)
    installAccess(session.access_token, session.expires_in, started)
  })
}

async function revoke(captured: string): Promise<void> {
  await alone(async () => {
    try {
      await wire!.logout(captured)
    } catch (error) {
      if ((error as { status?: number }).status !== 401) throw error
      // The captured bearer expired while we waited for the lock. Refresh
      // inside the same lock and revoke with the fresh one — WITHOUT
      // installing it, because this session is ending.
      const fresh = await withTimeout(wire!.refresh())
      await wire!.logout(fresh.access_token)
    }
  })
}

/// Sign out here, then everywhere, then on the hub.
///
/// The order is the whole subject. Memory is cleared FIRST and synchronously,
/// so nothing queued behind the lock can use the credential; the peers are
/// told next, so their tabs stop too; and only then does the revocation go
/// out, carrying the bearer captured before any of it.
/// Never rejects. The local session is already gone by the time the
/// revocation goes out, so a hub that cannot be reached changes nothing here —
/// but a rejection would reach `void signOut()` as an unhandled one, and an
/// awaiting caller would skip the navigation that follows. What it costs is
/// worth saying, so the failure is returned rather than thrown.
export async function signOut(): Promise<string | null> {
  const captured = access
  clearAccess(true)
  channel?.postMessage('sign-out')
  if (!captured) return null
  try {
    await revoke(captured)
    return null
  } catch (error) {
    return `Signed out here, but ${error}. The session may still work on other devices.`
  }
}

/// Remove credentials an older build persisted. This app writes none.
export function scrubLegacyCredentials() {
  localStorage.removeItem('kahawai.access')
  localStorage.removeItem('kahawai.refresh')
  document.cookie = 'kahawai_token=; Path=/; Max-Age=0; SameSite=Lax'
}

/// Wire the session up. Explicit, rather than the module side effects the
/// first implementation had: those are why its tests had to stub globals and
/// then dynamically import the module in the right order.
export function startAuthSession(auth: AuthWire) {
  wire = auth
  configureApiClient(accessToken, refreshTokens)
  if (typeof BroadcastChannel === 'undefined') return
  channel = new BroadcastChannel('kahawai.auth')
  // A peer signing out ends this tab too. The message carries no credential —
  // it is a fact, not a secret.
  channel.addEventListener('message', (event) => {
    if (event.data === 'sign-out') clearAccess(true)
  })
}

/// Tear down, for a test that wants a fresh module without re-importing it.
export function stopAuthSession() {
  channel?.close()
  channel = null
  wire = null
  clearTimeout(refreshTimer)
  access = null
  inFlight = null
  cleared = null
  generation = 0
}
