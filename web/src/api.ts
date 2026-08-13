// Same-origin client for the kahawai API. Access tokens ride the
// Authorization header for fetches and a cookie for <video>/HLS requests
// (media elements cannot set headers). 401s trigger one refresh + retry.

import { buildProfile } from './capabilities.ts'

import { notify } from './toast.ts'
import { SerialQueue } from './serial.ts'
import { REFRESH_RETRY_MS, refreshDelayMs } from './token.ts'

const LS_ACCESS = 'kahawai.access'
const LS_REFRESH = 'kahawai.refresh'

export type Tokens = { access_token: string; refresh_token: string }

function syncCookie(token: string | null) {
  document.cookie = token
    ? `kahawai_token=${token}; path=/; SameSite=Lax`
    : 'kahawai_token=; path=/; Max-Age=0'
}

/// Called when the tokens are cleared — a refresh definitively rejected, or
/// an explicit sign-out.
///
/// Which screen you are on is decided once, from `/bootstrap`, at startup.
/// Nothing revisited it, so a session that died an hour in left the shell up
/// with every panel showing its own 401 and a Try again that could never
/// work. The useful error is "sign in again", and only the shell can say it.
let onCleared: ((deliberate: boolean) => void) | null = null
export function onTokensCleared(cb: ((deliberate: boolean) => void) | null) {
  onCleared = cb
}

export function storeTokens(t: Tokens | null, deliberate = false) {
  if (t) {
    localStorage.setItem(LS_ACCESS, t.access_token)
    localStorage.setItem(LS_REFRESH, t.refresh_token)
    syncCookie(t.access_token)
    // Every path that lands a token — login, refresh — re-arms the
    // timer from the new expiry, so the schedule is never stale.
    keepTokenFresh()
  } else {
    // A refresh already out there must not land on top of this. It checks the
    // slot before storing, and removing the token is what tells it to stop —
    // which works across tabs too, where module state would not.
    refreshInFlight = null
    const had = localStorage.getItem(LS_ACCESS) !== null
    localStorage.removeItem(LS_ACCESS)
    localStorage.removeItem(LS_REFRESH)
    syncCookie(null)
    clearTimeout(refreshTimer)
    // Only when something was actually lost: clearing an already-empty slot
    // happens on the sign-in screen itself, and bouncing it back to sign-in
    // would be a loop.
    if (had) onCleared?.(deliberate)
  }
}

/// Revoke the refresh family on the hub, from a pair captured before the
/// browser's copies were dropped.
///
/// It deliberately touches storage at neither end. It cannot read the tokens,
/// because sign-out has already forgotten them — that is the point: the screen
/// changes at once, and a hub that accepts the call and then stalls cannot hold
/// it up. And it must not write them, because by the time it answers somebody
/// may have signed in on the screen it left behind.
///
/// `/auth/logout` sits behind the hub's auth layer, so a stale access token is
/// answered 401. The repair happens here rather than in `api()` because the hub
/// matches the family on the hash of the refresh token it is HANDED, and
/// repairing a 401 ROTATES that token: `api()` would retry with the body built
/// from the pre-rotation token, revoke nothing, and be answered 204 all the
/// same. The family would outlive the sign-out by its full 30 days, which is
/// the one thing this call exists to prevent, and nothing in the response says
/// so.
async function revoke(access: string, refresh: string): Promise<void> {
  const post = (a: string, r: string) =>
    fetch('/api/v1/auth/logout', {
      method: 'POST',
      headers: { 'content-type': 'application/json', Authorization: `Bearer ${a}` },
      body: JSON.stringify({ refresh_token: r }),
    })
  // Only 401 is repairable here. A 2xx is necessary but not sufficient — the
  // hub's logout is deliberately non-oracular and answers 204 for a token it
  // did nothing with — so this can only report that the call did not LAND, not
  // that a family was revoked. That is still worth reporting: returning on
  // every non-401 counted a 500, a 502 from a proxy in front of a restarting
  // hub, or a 403 as a completed sign-out, and `signOut` swallowed it, so the
  // family lived its full thirty days with nobody told. On a shared machine
  // that is the whole point of the button.
  const first = await post(access, refresh)
  if (first.ok) return
  if (first.status !== 401) throw new Error(`the hub did not end the session (${first.status})`)
  // Inside the lock, like every other rotation. Signing out while the scheduled
  // refresh is in flight posted the same token twice, and the hub reads a
  // replay as theft: the family died — which is what sign-out wanted — but this
  // call was answered 401 and told the viewer the opposite.
  const again = await alone(() =>
    fetch('/api/v1/auth/refresh', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ refresh_token: refresh }),
    }),
  )
  if (!again.ok) throw new Error(`the hub did not end the session (${again.status})`)
  const fresh: Tokens = await again.json()
  const second = await post(fresh.access_token, fresh.refresh_token)
  if (!second.ok) throw new Error(`the hub did not end the session (${second.status})`)
}

/// Sign out: drop the browser's copies now, tell the hub afterwards.
///
/// Dropping them only ended the session in this browser — the refresh token
/// stayed usable on the hub for the rest of its life. Telling the hub needs
/// credentials the clear has just destroyed, so the pair is captured first and
/// handed over explicitly.
///
/// One revoked family, not every session the account has: other devices stay
/// signed in, which is what you want. The access token and the cookie are not
/// invalidated by it either — they are good for their remaining life, and
/// dropping this browser's copies is the whole of what makes them unreachable.
export async function signOut(): Promise<void> {
  const access = localStorage.getItem(LS_ACCESS)
  const refresh = localStorage.getItem(LS_REFRESH)
  storeTokens(null, true)
  // Said out loud rather than swallowed: the screen has already changed, so
  // this is the only place the viewer can learn that the session they just
  // ended is still usable somewhere else.
  if (access && refresh) {
    await revoke(access, refresh).catch((e: unknown) => {
      notify(`Signed out here, but ${e}. The session may still work on other devices.`)
    })
  }
}

export function accessToken(): string | null {
  return localStorage.getItem(LS_ACCESS)
}

/// Put the stored token back on the cookie the media elements read.
///
/// The cookie is a SESSION cookie and the token outlives the session it was
/// written in, so a browser reopened on a still-valid token came back with the
/// header credential and without the cookie one. `syncCookie` had exactly one
/// caller — `storeTokens` — and nothing on the boot path stores anything: the
/// shell renders signed in from `/bootstrap`, which is a Bearer request, and
/// then every poster, the event stream and hls.js's own XHR 401, because none
/// of them can set a header. It repaired itself when the scheduled refresh
/// landed — at most fourteen minutes of a signed-in app with no artwork, since
/// the access token lives fifteen (`auth.rs` ACCESS_TTL_SECS) and the refresh
/// is scheduled a minute ahead of expiry (`token.ts` REFRESH_LEAD_MS). Narrow,
/// and every second of it is a home screen of empty frames.
///
/// Called from `main.tsx` before the first render rather than run on import:
/// an effect is too late (the browser has already requested what it painted),
/// and an import-time side effect made `api.ts` unimportable under `node
/// --test`, where there is no localStorage — which is how the pure modules
/// that import from it are tested.
///
/// Still a session cookie, because no `Max-Age` can state the truth: the value
/// is a fifteen-minute token, so any persistent expiry either outlives what it
/// carries or has to be rewritten on every refresh — which is the session
/// cookie again, with one more thing to get wrong.
export function restoreCookie() {
  syncCookie(accessToken())
}

function claims(): { username?: string; admin?: boolean; exp?: number } {
  const t = accessToken()
  if (!t) return {}
  try {
    return JSON.parse(atob(t.split('.')[1]))
  } catch {
    return {}
  }
}

let refreshTimer: ReturnType<typeof setTimeout> | undefined

/// Keep the access token valid ahead of time, rather than repairing it
/// after something fails.
///
/// `api()` refreshes on a 401 and retries, but it is not the only thing
/// carrying this token: the SAME token rides the kahawai_token cookie
/// for <video>, <img> and EventSource, and hls.js puts it in a Bearer
/// header from its own XHR. None of those pass through api(), so none
/// of them can repair themselves — an expired token simply fails the
/// media request, hls.js stops loading, and the session it was reading
/// goes idle and is reaped. Observed 2026-08-07: a paused film died
/// this way, and because the expired token makes the hub answer 401
/// where it would have answered 404, session recovery could not see
/// its own trigger either.
export function keepTokenFresh() {
  clearTimeout(refreshTimer)
  const exp = claims().exp
  if (!exp) return
  refreshTimer = setTimeout(
    () => {
      void refreshTokens().then((ok) => {
        // On success storeTokens() reschedules from the NEW expiry.
        if (!ok && accessToken()) refreshTimer = setTimeout(keepTokenFresh, REFRESH_RETRY_MS)
      })
    },
    refreshDelayMs(exp * 1000, Date.now()),
  )
}

export function username(): string {
  return claims().username ?? ''
}

export function isAdmin(): boolean {
  return claims().admin === true
}

/// Is the credential slot still the one this refresh was started for?
///
/// A refresh is out for as long as a round trip, and anything can happen in
/// that window: a sign-out, another tab's refresh rotating the token, or a
/// different account signing in on this screen. Storing the answer then would
/// resurrect a session that had ended — sign-in screen on top of a live cookie
/// that `/bootstrap` accepts, so the next reload signs you back in as the
/// account that just left.
///
/// The token we sent, still in the slot, is the test. It beats a module-scoped
/// generation counter on two counts: a clear in ANOTHER TAB is visible to it,
/// and a fresh sign-in — which bumps no counter — is too.
function mine(rt: string): boolean {
  return localStorage.getItem(LS_REFRESH) === rt
}

// Single-flight: refresh tokens rotate server-side, so concurrent 401s
// must share ONE refresh — the losers of the race would otherwise send
// the already-rotated token, fail, and wipe the fresh session.
let refreshInFlight: Promise<boolean> | null = null

/// A refresh that never answers must not hold the rest of the app behind it.
/// `refreshInFlight` is shared by design, so a fetch with no deadline poisons
/// every later 401 too, and the caller waiting on it has no way out: nothing
/// times out a `fetch` on its own.
const REFRESH_TIMEOUT_MS = 15000

/// How long to queue for the cross-tab refresh lock before giving up and
/// letting the caller's own 401 handling deal with it. Longer than the request
/// it is serialising, so a tab that is genuinely first is never cut off.
const LOCK_WAIT_MS = 20000

/// One refresh at a time across the whole ORIGIN, not just this tab.
///
/// `refreshInFlight` is module state, so it cannot see another tab, and the
/// deadline every tab computes is the same instant — `exp` comes from the same
/// stored token. Two tabs open, or one laptop resumed past that instant, and
/// both post the same refresh token. The hub rotates exactly once and treats
/// the replay as theft: it revokes the whole family (`auth.rs`, "Replaying a
/// consumed token revokes its family"), and which of the two symptoms you get
/// is decided by which response lands first — an app left signed in on a
/// revoked family, or both tabs bounced to the sign-in screen while holding a
/// perfectly good access token.
///
/// The Web Locks API is exactly this problem, and it is in every browser this
/// app supports. Where it is missing the behaviour is what it was before: one
/// lock per tab, and the loser repairs itself on the next 401.
async function alone<T>(run: () => Promise<T>): Promise<T> {
  const locks = navigator.locks
  if (!locks) return run()
  // Bounded like everything else on this path. `AbortSignal.timeout` inside the
  // callback covers the fetch, not the wait for the lock, and a waiter that
  // never acquires it leaves the single-flight promise pending for ever.
  return locks.request('kahawai.refresh', { signal: AbortSignal.timeout(LOCK_WAIT_MS) }, run)
}

export function refreshTokens(): Promise<boolean> {
  // The clear is HERE as well as in the callback's `finally`, because the
  // callback is not guaranteed to run: `locks.request` rejects on its own for a
  // document that is not fully active, for an opaque origin, and now for the
  // wait timing out. A rejection left `refreshInFlight` holding it for the life
  // of the page, and `api()` awaits that promise on every 401 — so one failure
  // to take the lock turned every later 401 in the tab into a DOMException out
  // of `api()`, with no path back.
  // Captured SYNCHRONOUSLY, before any queueing: this call is about the session
  // that was live when it was made. Signing in again while it waits produces a
  // different session, and a refresh that then acted on that one could sign out
  // an account it was never asked about.
  const started = localStorage.getItem(LS_REFRESH)
  if (!started) return Promise.resolve(false)
  refreshInFlight ??= alone(async () => {
    try {
      // And re-read inside the lock, because the tab that went first has
      // rotated it. Asking with the token we queued on is precisely the replay
      // the lock exists to prevent, and asking with THEIRS would make this call
      // about a session it was not started for — so a rotation that happened
      // while we waited simply ends this attempt. The caller's next 401 asks
      // again, with whatever is current by then.
      const rt = localStorage.getItem(LS_REFRESH)
      if (!rt || rt !== started) return false
      const r = await fetch('/api/v1/auth/refresh', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ refresh_token: rt }),
        signal: AbortSignal.timeout(REFRESH_TIMEOUT_MS),
      })
      if (!r.ok) {
        // Only a definitive rejection means the session is dead; transient
        // failures keep the tokens for a later retry. And only if it is still
        // the session we asked about — see `mine()` — or a 401 about a token
        // nobody holds any more would take a live one down with it.
        if ((r.status === 401 || r.status === 403) && mine(rt)) storeTokens(null)
        return false
      }
      const fresh = await r.json()
      if (!mine(rt)) return false
      storeTokens(fresh)
      return true
    } catch {
      return false
    } finally {
      refreshInFlight = null
    }
  })
    .catch(() => false)
    .finally(() => {
      refreshInFlight = null
    })
  return refreshInFlight
}

/// The hub could not be reached at all: no response, rather than a bad one.
/// Distinct from `ApiError`, which means the hub answered and said no.
///
/// It exists to be readable. A dead hub surfaced as `TypeError: Failed to
/// fetch` in every error banner in the app — true, and no use to anybody
/// deciding whether to look at their wifi or at the server.
export class Offline extends Error {
  constructor() {
    super('Could not reach the hub.')
    this.name = 'Offline'
  }
  override toString() {
    return this.message
  }
}

export async function api(path: string, init?: RequestInit): Promise<Response> {
  const go = () => {
    const headers: Record<string, string> = { ...(init?.headers as Record<string, string>) }
    const t = accessToken()
    if (t) headers['Authorization'] = `Bearer ${t}`
    if (init?.body) headers['content-type'] = 'application/json'
    return fetch(path, { ...init, headers })
  }
  // Both attempts, because the retry can be the one that finds the hub gone.
  const send = async () => {
    try {
      return await go()
    } catch {
      throw new Offline()
    }
  }
  let r = await send()
  if (r.status === 401 && (await refreshTokens())) r = await send()
  return r
}

/// A failed request, with the status still attached.
///
/// The status is the part a caller can act on: 503 from a session start
/// means the source is on a host that is not answering and the same request
/// may work in a minute, where 409 means it never will. That difference
/// cannot be read out of the message — the message is the server's to
/// reword, and a client that greps it breaks the day somebody does.
///
/// `toString` is the message alone, because these are shown to people and
/// an `Error:` prefix has never told anybody anything.
export class ApiError extends Error {
  // A field, not a constructor parameter property: `erasableSyntaxOnly` is
  // on so that `node --test` can strip the types natively, and a parameter
  // property is syntax that has to be compiled rather than erased.
  status: number
  constructor(status: number, message: string) {
    super(message)
    this.name = 'ApiError'
    this.status = status
  }
  override toString() {
    return this.message
  }
}

export async function json<T>(path: string, init?: RequestInit): Promise<T> {
  const r = await api(path, init)
  if (!r.ok) throw new ApiError(r.status, (await r.text()) || `${r.status}`)
  return r.json()
}

/// A mutation whose answer is only pass or fail.
///
/// `api` resolves for EVERY status — it is the transport, and only `json`
/// turns a refusal into a rejection. A mutation built on `api` therefore ran
/// its caller's `.then()` on a 403 or a 500: the list reloaded, no error was
/// shown, and the operator was told the thing had happened. These have no
/// body worth reading, so they cannot use `json`, which would then choke on
/// the empty one.
async function ok(path: string, init?: RequestInit): Promise<void> {
  const r = await api(path, init)
  if (!r.ok) throw new ApiError(r.status, (await r.text()) || `${r.status}`)
}

export type Item = {
  id: string
  kind: 'movie' | 'show' | 'episode' | 'album' | 'track'
  title: string
  artist?: string | null
  year: number | null
  season: number | null
  episode: number | null
  episode_end?: number | null
  /// HUB-31: TVDB-style projection of absolute numbering (anime).
  proj_season?: number | null
  proj_episode?: number | null
  /// The show an episode belongs to — search can return episodes, and a
  /// hit called "Pilot" needs to say which of its namesakes it is. For a
  /// track hit, `parent_id` is the album to open: tracks have no detail
  /// view of their own.
  parent_id?: string | null
  parent_title?: string | null
  /// A library this item is in, as navigation context: item URLs live
  /// under a library, and a row from a cross-library browse (search,
  /// continue watching) has no other way to know one. Membership is
  /// many-to-many, so this is "a library it is in", not "its library".
  /// Null for an item in no library, which only an unrestricted account
  /// can be looking at. Browse rows only — children carry their parent's
  /// context instead.
  library_id?: string | null
  sources: number
  /// HUB-19 ReplayGain, as the file states it. Gains are dB to apply;
  /// peaks are linear sample values where 1.0 is full scale. Absent for
  /// anything untagged.
  replay_gain?: {
    track_gain_db?: number | null
    track_peak?: number | null
    album_gain_db?: number | null
    album_peak?: number | null
    reference_level_db?: number | null
  } | null
  /// Enrichment state (movie/show): null = never enriched,
  /// miss/rejected = unmatched, weak = uncertain, auto/manual = good.
  match_confidence?: string | null
  /// Metadata timestamp, used to bust the artwork cache on a re-match.
  art_version?: number | null
  /// The filename-derived identity (title/year as parsed from disk) and
  /// the provider's matched title — for the review dialog.
  file_title?: string | null
  file_year?: number | null
  matched_title?: string | null
  premiered?: string | null
  resume_position_ms: number | null
  resume_duration_ms?: number | null
  played: boolean
  play_count: number
}

export type StreamInfo = {
  container?: string
  duration_ms?: number
  video?: {
    codec: string
    width: number
    height: number
    display_width?: number | null
    display_height?: number | null
    orientation?: string | null
    pixel_aspect_ratio?: [number, number] | null
    profile?: string | null
    level?: string | null
  }[]
  audio?: { codec: string; channels: number; language?: string | null }[]
  subtitles?: { format: string; language?: string | null }[]
}

export type Source = {
  path_rel: string
  size: number
  available: boolean
  /// Release revision: 1 plain, 2+ for v2 / REPACK / PROPER. Higher
  /// outranks lower within the same resolution when playback picks.
  revision: number
  streams: StreamInfo | null
}

export type ItemMetadata = {
  overview: string | null
  rating: number | null
  premiered: string | null
  confidence: 'auto' | 'weak'
  provider: string
  /// ISO 639-1 original language of the matched title (feeds the
  /// future default-track mechanism).
  original_language?: string | null
  /// HUB-6. Both describe the work, so an episode shows its show's.
  genres?: string[] | null
  cast?: { name: string; character: string | null }[] | null
}

export type ItemDetail = Item & {
  negotiated?: Negotiated
  sources_detail: Source[]
  show_title?: string | null
  parent_id?: string | null
  metadata?: ItemMetadata
  related?: { kind: string; title: string | null; item_id: string | null }[]
}

/// Local artwork (cover.jpg etc). <img> requests authenticate with the
/// media cookie; 404 = no artwork (hide the img).
///
/// The response is cached hard (a day), so pass the item's `art_version`
/// — its metadata timestamp — or a re-match leaves the old poster on
/// screen until the cache expires.
/// `size` names one of the hub's sizes (`thumb`, `card`); omitting it
/// serves the original, which is what the detail view wants. Ask for the
/// size the element actually displays — a grid of 34px rows pulling
/// 600px covers is the reason this parameter exists.
export const artworkUrl = (
  id: string,
  version?: number | null,
  size?: 'thumb' | 'card1x' | 'card',
) => {
  const p = new URLSearchParams()
  if (size) p.set('size', size)
  if (version) p.set('v', String(version))
  const q = p.toString()
  return `/api/v1/items/${id}/artwork${q ? `?${q}` : ''}`
}

/// One poster at both densities, for the `srcset` of anything that shows a
/// card. What varies between clients here is the display, not the layout —
/// the CSS widths are fixed — so these are `x` descriptors and there is no
/// `sizes` to get wrong. A 1× display stops being sent 6× the pixels it can
/// show; a 2× one is unaffected.
export const artworkSrcSet = (id: string, version?: number | null) =>
  `${artworkUrl(id, version, 'card1x')} 1x, ${artworkUrl(id, version, 'card')} 2x`

export const fetchChildren = (id: string) =>
  json<{ children: Item[] }>(`/api/v1/items/${id}/children`)

/// One unified track row (subtitle unification): the id is THE key for
/// serving, selection, OCR, deletion and preference memory. `delivery`
/// is computed per request from the capability bits — capability never
/// filters the list, it changes what a track means for this client.
export type SubtitleDelivery = 'text' | 'ass' | 'overlay' | 'burn' | 'none'
export type Subtitle = {
  id: number
  origin: 'embedded' | 'sidecar' | 'downloaded' | 'ocr' | 'raster'
  stream_index: number | null
  format: string
  language: string | null
  label: string | null
  machine: boolean
  derived_from: number | null
  delivery: SubtitleDelivery
  note: string
  /// HUB-32c/d: may THIS user remove it? Only downloaded tracks, and
  /// only their creator or an admin — the other hub-stored origins are
  /// caches that rebuild themselves.
  deletable: boolean
}

export const isImageSub = (s: Subtitle) => ['pgs', 'vobsub', 'dvdsub'].includes(s.format)

/// HUB-32d: a styled script rendered server-side to display sets. It
/// is delivered as an overlay like PGS, but sourced item-level rather
/// than from the live session tap — see `overlayUrl`.
export const isRasterSub = (s: Subtitle) => s.origin === 'raster'

/// Where a track's display sets come from. An embedded image track is
/// decoded by the running pipeline and tail-followed off the session;
/// a rasterised one is a finished artefact on the item.
export const overlayUrl = (s: Subtitle, itemId: string, streamUrl: string) =>
  isRasterSub(s)
    ? `/api/v1/items/${itemId}/subtitles/${s.id}.jsonl`
    : `${streamUrl.replace(/[^/]*$/, '')}subs-${s.id}.jsonl`

/// HUB-21/22/24: external subtitle search + download.
export type SubtitleCandidate = {
  provider: string
  file_id: string
  language: string | null
  release_name: string | null
  hash_match: boolean
  downloads: number
  uploader: string | null
  rating: number | null
  fps: number | null
}

/// HUB-21/24: download entitlement. per_account = false means the
/// budget is shared by everyone on this server, which the UI must say.
export type SubtitleQuota = {
  remaining: number | null
  total: number | null
  resets_in_secs: number | null
  per_account: boolean
}

export const searchSubtitles = (itemId: string, languages: string[]) =>
  json<{ candidates: SubtitleCandidate[]; quota: SubtitleQuota }>(
    `/api/v1/items/${itemId}/subtitles/search`,
    { method: 'POST', body: JSON.stringify({ languages }) },
  )

export const downloadSubtitle = (itemId: string, fileId: string, language: string | null) =>
  json<{ track_id: number; quota: SubtitleQuota }>(`/api/v1/items/${itemId}/subtitles/download`, {
    method: 'POST',
    body: JSON.stringify({ file_id: fileId, language }),
  })

/// "3 of 5 downloads left today (shared by everyone on this server)"
export function quotaLabel(q: SubtitleQuota | null): string {
  if (!q || q.remaining === null) return ''
  const scope = q.per_account ? '' : ' — shared by everyone on this server'
  const resets =
    q.resets_in_secs && q.resets_in_secs > 0
      ? `, resets in ${Math.max(1, Math.round(q.resets_in_secs / 3600))} h`
      : ''
  return `${q.remaining}${q.total ? ` of ${q.total}` : ''} downloads left today${resets}${scope}`
}

/// Hub-stored tracks (downloaded/OCR) only; scan-owned rows refuse.
export const deleteSubtitle = (id: number) =>
  json<{ removed: boolean }>(`/api/v1/subtitles/${id}`, { method: 'DELETE' })

/// HUB-32c: OCR an image track (embedded or VobSub sidecar) into a new
/// text track. Synchronous — a feature film takes ~30 s; cached, and

export const fetchFonts = (itemId: string) =>
  json<{ fonts: string[] }>(`/api/v1/items/${itemId}/fonts`)

// One uniform label across origins; delivery adds the honest suffix
// (a burn restarts the session; 'none' renders disabled).
export const subtitleLabel = (s: Subtitle) =>
  `${s.language ?? 'unknown'} · ${s.format}` +
  (s.origin === 'sidecar'
    ? ' · file'
    : s.origin === 'downloaded'
      ? ' · downloaded'
      : s.origin === 'ocr'
        ? ' · ocr'
        : s.origin === 'raster'
          ? ' · typeset'
          : '') +
  (s.delivery === 'burn' ? ' · burn-in' : s.delivery === 'none' ? ' · unavailable' : '')

export type LibrarySummary = { id: string; name: string; media_type: string }

export const fetchLibraries = () => json<{ libraries: LibrarySummary[] }>('/api/v1/libraries')

/// How a library is browsed. `sort` is one of the names the hub knows
/// (`title`, `-title`, `year`, `-year`, `added`, `-added`); anything else
/// falls back to title there rather than erroring.
export type ItemsPage = {
  library?: string
  q?: string
  sort?: string
  limit?: number
  offset?: number
  /// Started and not finished, most recently watched first — the home
  /// screen's continue-watching row. Its own order, so `sort` and `q` do
  /// not apply; `library` still scopes it.
  in_progress?: boolean
}

/// The hub pages this endpoint — 200 items unless asked otherwise, capped
/// at 1000. Sending no window is not "give me everything", it is "give me
/// the first 200 and do not mention the rest", which is how the browser
/// spent three commits showing 200 of 881 films. Always read `total`.
export const fetchItems = (page: ItemsPage) => {
  const p = new URLSearchParams()
  if (page.library) p.set('library', page.library)
  if (page.q) p.set('q', page.q)
  if (page.sort) p.set('sort', page.sort)
  if (page.limit !== undefined) p.set('limit', String(page.limit))
  if (page.offset) p.set('offset', String(page.offset))
  if (page.in_progress) p.set('in_progress', 'true')
  return json<{ items: Item[]; total: number; limit: number; offset: number }>(
    `/api/v1/items?${p.toString()}`,
  )
}

/// Which screen to open on. Public: needs no token, and answers before
/// setup has happened.
export type Bootstrap = { setup_required: boolean; authenticated: boolean }

/// Which screen to open on. The app renders NOTHING until this answers, so it
/// is the one request that must not be able to hang: no timeout meant a hub
/// that accepted the connection and then wedged left a permanently blank page
/// with no header, no message and nothing to press.
export const fetchBootstrap = () =>
  json<Bootstrap>('/api/v1/bootstrap', { signal: AbortSignal.timeout(BOOTSTRAP_TIMEOUT_MS) })

/// Shorter than a session start's ceiling: this is a database read and a token
/// check, and the page is blank until it lands.
const BOOTSTRAP_TIMEOUT_MS = 10_000

/// What this client would actually be served, for the profile it asked
/// with — the converged half of the item resource.
export type Negotiated = {
  source: {
    module_id: string
    collection_id: string
    path_rel: string
    display_width?: number | null
    display_height?: number | null
    orientation?: string | null
  } | null
  /// What negotiation decided. A `remux` may still be dispatched to a
  /// transcoder when the session starts; that is placement, which a
  /// safe method does not do.
  mode: string
  cost: string
  streams: { video: string; audio: string; subtitles: SubtitleVerdict[] }
  /// The unified track list for the source negotiation chose, each with
  /// the delivery it would get.
  subtitles: Subtitle[]
}

/// Ask what we would be served, not merely what was discovered
/// (RFC 10008 QUERY). One round trip: the answer carries the announced
/// streams AND the negotiated verdict.
///
/// The profile sent here is the UNREFINED probe. `refineForSources`
/// needs the announced streams this call returns, and it can only ever
/// ADD precise caps beside the family floor — which `cap_admits`
/// already treats as unknown-permissive — so the only thing lost is a
/// slightly pessimistic preview on a device whose generic high-end
/// probe fails but whose per-stream one passes. `startPlaybackSession`
/// still refines, and by then the streams are in hand.
export async function fetchItem(id: string): Promise<ItemDetail> {
  const raw = await json<Item & { sources: Source[] | number; negotiated?: Negotiated }>(
    `/api/v1/items/${id}`,
    {
      method: 'QUERY',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ profile: buildProfile() }),
    },
  )
  const sources = Array.isArray(raw.sources) ? raw.sources : []
  return { ...(raw as Item), sources: sources.length, sources_detail: sources }
}

export type SubtitleVerdict = {
  index: number
  track_id?: number | null
  format: string
  language?: string | null
  tier: 'text' | 'convert' | 'graphics' | 'ocr' | 'burn' | 'unavailable'
  note: string
}
export type StreamVerdict = {
  /// Aggregate semantic work. Unlike session `mode`, this says whether an
  /// elementary stream is encoded and follows track-switch re-plans.
  cost: 'direct' | 'copy' | 'audio_encode' | 'video_encode'
  video: string
  audio: string
  subtitles?: SubtitleVerdict[]
}

export type Session = {
  session_id: string
  mode: 'direct' | 'remux' | 'transcode'
  stream_url: string
  content_type: string
  size: number
  duration_ms: number | null
  /// Timeline base of the part the pipeline started in (multi-part
  /// CD1/CD2 sources; 0 for single files).
  part_base_ms?: number
  parts?: number
  streams: StreamVerdict | null
}

/// HUB-14: no `mode` — the hub negotiates from the capability profile.
/// (An explicit mode remains available on the wire for scripts/debug.)
/// A session start that never answers must not be waited on for ever.
///
/// `fetch` has no timeout, and the player latches `recovering` across this
/// call: a hub that accepts the connection and then wedges left the latch set,
/// so the give-up dialog's "press play to try again" was a permanent no-op —
/// the handback it exists for, dead on arrival.
///
/// A minute, because the only job of this number is to be FINITE. Starting a
/// session is the slowest thing the hub does — it can be waiting on a subtitle
/// index walk, a playlist runway or a pipeline coming up — and a ceiling tight
/// enough to cut across that turns "slow" into "impossible", with the hub
/// building a session nobody collects. It is a patience bound, not a mirror of
/// anything the hub knows: if the hub's own waits ever exceed this, the symptom
/// is a retry, not a wrong answer.
const START_TIMEOUT_MS = 60000

export function startSession(
  itemId: string,
  profile: import('./capabilities').CapabilityProfile,
  startMs = 0,
  audioTrack = 0,
  videoTrack = 0,
  subtitleTrack?: number,
): Promise<Session> {
  return json('/api/v1/playback/sessions', {
    method: 'POST',
    signal: AbortSignal.timeout(START_TIMEOUT_MS),
    body: JSON.stringify({
      item_id: itemId,
      profile,
      start_ms: Math.round(startMs),
      audio_track: audioTrack,
      video_track: videoTrack,
      // An IMAGE track id forces its burn-in from the first segment;
      // text tracks need no session involvement.
      subtitle_track: subtitleTrack ?? null,
    }),
  })
}

/// One place builds a play request: bandwidth pref → probed profile →
/// source-aware refinements → debug mask → session. Shared by the player
/// route's start and the player's own capability restart, so
/// both negotiate from an identical profile.
export async function startPlaybackSession(
  item: ItemDetail,
  startMs = 0,
  audioTrack = 0,
  videoTrack = 0,
  known?: Pref[],
  quiet = false,
  /// Where a prefs failure should be said. The default is a toast, which the
  /// player cannot use: its Try again is reachable with the picture fullscreen,
  /// and the toast host is a sibling of the element that goes fullscreen. The
  /// player's other two prefs reads already pass `playerNote` for that reason;
  /// this one read through here and was invisible.
  report?: (message: string) => void,
): Promise<Session> {
  // Prefs from the caller when it has them. the player route and the auto-advance both
  // resolved the audio track from this same read a moment earlier, so fetching
  // again here was a second round trip and — once the read started reporting —
  // a second copy of the same sentence, in a second host the first could not
  // replace.
  //
  // `quiet` for the callers that must not speak: the stand-by retry runs every
  // five seconds for as long as a host is away, and a report there is a toast
  // on a timer about the weather its own dialog is already describing. It is
  // opt-IN, and the default reports, because the first cut of this had it the
  // other way round and quietly took the report away from three deliberate
  // presses — a Play from any page, an Apply in the capability dialog and
  // a Try again — that read prefs through here and nowhere else. This is the
  // only reader of `bandwidth_kbps` in the app, so silence here is the cap
  // dropped with nothing said anywhere.
  const prefs =
    known ??
    (quiet ? await fetchPrefs().catch(() => ({ prefs: [] })) : await prefsOrNone(report)).prefs
  const v = prefs.find((x) => x.scope === '' && x.key === 'bandwidth_kbps')?.value
  const cap = v ? Number(v) : undefined
  // Source-aware precision: probe the exact strings the announced
  // streams call for (profile/level from the hub's own probing).
  const announced = item.sources_detail.flatMap((s) => s.streams?.video ?? [])
  return startSession(item.id, buildProfile(cap, announced), startMs, audioTrack, videoTrack)
}

/// Music plays direct by operator contract (browsers decode flac/mp3
/// natively; HUB-19 owns future music delivery shapes).
export function startSessionDirect(itemId: string, signal?: AbortSignal): Promise<Session> {
  return json('/api/v1/playback/sessions', {
    method: 'POST',
    body: JSON.stringify({ item_id: itemId, mode: 'direct' }),
    signal,
  })
}

export type Pref = { scope: string; key: string; value: string }

/// Preferences, or none — with the failure SAID rather than swallowed.
///
/// Five call sites resolved tracks from prefs and fell back to an empty list,
/// each with its own silent catch: the audio track the viewer last chose, the
/// remembered subtitle track, the per-media-type language wishlist, the anime
/// view, and the bandwidth cap they typed into Settings. Every one of them is
/// something a person picked and would simply not be getting, with nothing on
/// screen to say why — and a cap silently dropped is the expensive one, since
/// it is the setting that exists to keep a session off a metered link.
///
/// The fallback stays: a page that renders on source order beats one that does
/// not render. What changes is that it stops pretending nothing happened.
///
/// `report` because the player cannot use `notify`: the toast host is a sibling
/// of `.videobox`, which is what goes fullscreen, so a toast raised while the
/// picture fills the screen is painted nowhere. Player call sites pass
/// `showNote`.
export async function prefsOrNone(
  report: (msg: string) => void = notify,
): Promise<{ prefs: Pref[] }> {
  try {
    return await fetchPrefs()
  } catch (e) {
    report(`Could not load your playback preferences: ${e}`)
    return { prefs: [] }
  }
}

/// HUB-33, resolved entirely client-side from /api/v1/prefs.
/// Precedence: per-series/movie memory (what the user last set) >
/// per-media-type settings (ordered language lists, 'original' resolves
/// via the item's original_language) > track 0 / no subs.
/// Settings live user-global: key `audio.{media_type}` = "nl,original,en",
/// key `subs.{media_type}` = "nl,en" ('' or absent = no subs).
export function resolveTracks(
  prefs: Pref[],
  seriesId: string,
  itemId: string,
  mediaType: string,
  originalLanguage: string | null | undefined,
  audio: { language?: string | null }[],
): { audioTrack: number; subs: string[]; subTrack: number | null } {
  const get = (scope: string, key: string) =>
    prefs.find((p) => p.scope === scope && p.key === key)?.value
  const list = (v?: string) =>
    (v ?? '')
      .split(',')
      .map((s) => s.trim().toLowerCase())
      .filter(Boolean)
  const langEq = (l: string | null | undefined, want: string) => {
    if (!l) return false
    const a = l.toLowerCase()
    const b = want.toLowerCase()
    return a === b || a.slice(0, 2) === b.slice(0, 2)
  }

  // Audio, most specific first: THIS item's exact track (two English
  // tracks — feature + commentary — are common, so language cannot
  // express the choice), then the series language memory, then the
  // ordered per-type list.
  let audioTrack: number | undefined
  const exact = get(itemId, 'audio.track')
  if (exact?.startsWith('#')) {
    const i = Number(exact.slice(1))
    if (i >= 0 && i < audio.length) audioTrack = i
  }
  const remembered = get(seriesId, 'audio')
  if (audioTrack !== undefined) {
    // exact item pref already decided
  } else if (remembered?.startsWith('#')) {
    const i = Number(remembered.slice(1))
    if (i >= 0 && i < audio.length) audioTrack = i
  } else if (remembered) {
    const i = audio.findIndex((a) => langEq(a.language, remembered))
    if (i >= 0) audioTrack = i
  }
  if (audioTrack === undefined) {
    // 'original' is the standing backstop: implicit final entry of
    // every audio wishlist (and the whole list when none is set).
    const wish = list(get('', `audio.${mediaType}`))
    if (!wish.includes('original')) wish.push('original')
    for (const want of wish) {
      const lang = want === 'original' ? originalLanguage : want
      if (!lang) continue
      const i = audio.findIndex((a) => langEq(a.language, lang))
      if (i >= 0) {
        audioTrack = i
        break
      }
    }
  }

  // Subs: memory ('off' | 'any' | lang), else the per-type list.
  // Returned as the ordered language wishlist ([] = subtitles off).
  const subsMem = get(seriesId, 'subs')
  const subs = subsMem === 'off' ? [] : subsMem ? [subsMem] : list(get('', `subs.${mediaType}`))
  // Top precedence (subtitle unification): THIS item's exact track id
  // — the only spelling that can name a specific downloaded/OCR row.
  // Callers honor it iff the id is still in the fetched list.
  const exactSub = get(itemId, 'subs.track')
  const subTrack = exactSub && /^\d+$/.test(exactSub) ? Number(exactSub) : null
  return { audioTrack: audioTrack ?? 0, subs, subTrack }
}

/// First subtitle matching the wishlist, in wishlist order ('any'
/// matches the first text sub). null = leave subtitles off.
export function pickSubtitle(wishlist: string[], subs: Subtitle[]): Subtitle | null {
  // Language wishes auto-pick only client-rendered tracks: silently
  // forcing a burn (a video encode restart) is never what a language
  // preference means — burns are explicit picks.
  const auto = (s: Subtitle) =>
    s.delivery === 'text' || s.delivery === 'ass' || s.delivery === 'overlay'
  // The server's fidelity order (HUB-32a/d): the client's own ASS
  // renderer first, then a server-rasterised overlay, then flattened
  // text. Within one language the BEST reading wins, not whichever row
  // the listing happened to put first — otherwise a client with ASS
  // masked off would take the flattened VTT and never notice the
  // rasterised track sitting right behind it.
  const rank = (s: Subtitle) => (s.delivery === 'ass' ? 0 : s.delivery === 'overlay' ? 1 : 2)
  const best = (cs: Subtitle[]) =>
    cs.length === 0 ? null : cs.reduce((a, b) => (rank(b) < rank(a) ? b : a))
  for (const want of wishlist) {
    const eligible = subs.filter((s) => auto(s) && !isImageSub(s))
    const hit =
      want === 'any'
        ? best(eligible)
        : best(
            eligible.filter(
              (s) => (s.language ?? '').toLowerCase().slice(0, 2) === want.slice(0, 2),
            ),
          )
    if (hit) return hit
  }
  return null
}

export const fetchPrefs = () => json<{ prefs: Pref[] }>('/api/v1/prefs')

/// HUB-11 event channel: invalidation hints ({kind, ...}). Authenticates
/// via the kahawai_token cookie (EventSource cannot set headers). The
/// browser auto-reconnects; callers just react to hints.
export function openEvents(onEvent: (e: { kind: string } & Record<string, unknown>) => void) {
  const es = new EventSource('/api/v1/events')
  es.onmessage = (m) => {
    try {
      onEvent(JSON.parse(m.data))
    } catch {
      // malformed hint: ignore
    }
  }
  return es
}

// Preferences are whole-value writes. One queue per key preserves the order
// the viewer changed that value while unrelated controls still save in
// parallel. Filtering stale responses in a component cannot provide this: an
// older request can commit last and only reveal the rollback after a reload.
const prefWrites = new Map<string, SerialQueue>()
export const putPref = (scope: string, key: string, value: string) => {
  const target = `${scope}\0${key}`
  const queue = prefWrites.get(target) ?? new SerialQueue()
  prefWrites.set(target, queue)
  return queue.run(() =>
    json<{ ok: boolean }>('/api/v1/prefs', {
      method: 'PUT',
      body: JSON.stringify({ scope, key, value }),
    }),
  )
}

/// Seek-restart: the pipeline restarts at the offset; re-attach the
/// player. An audio_track switches tracks during the restart (HUB-27).
export function seekSession(
  sessionId: string,
  positionMs: number,
  audioTrack?: number,
  videoTrack?: number,
  subtitleTrack?: number,
): Promise<{ part_base_ms: number; streams?: StreamVerdict | null }> {
  return json(`/api/v1/playback/sessions/${sessionId}/seek`, {
    method: 'POST',
    body: JSON.stringify({
      position_ms: Math.round(positionMs),
      audio_track: audioTrack ?? null,
      video_track: videoTrack ?? null,
      // An image track id switches the burn mid-session; 0 withdraws
      // an explicit burn; absent = keep as is.
      subtitle_track: subtitleTrack ?? null,
    }),
  })
}

export type WatchedRow = {
  item_id: string
  position_ms: number
  played: boolean
  play_count: number
}

/// Mark an item watched, or not, without playing it — something seen on
/// another television, or a tick undone. Either direction clears the
/// resume position; the play count only ever climbs.
///
/// `items` marks a batch in one call: a season, or a whole show. Every id
/// must be `itemId` itself or one of its children, which is what lets the
/// hub cover the batch with one access check. Ids outside that are not
/// marked and are not reported — the returned rows say what was, so a
/// caller that cares can compare.
///
/// One statement server-side, so a season either applies or does not.
/// Looping here instead meant 26 round trips for an anime season and a
/// half-applied mark whenever one of them failed.
export const setWatched = (itemId: string, played: boolean, items?: string[]) =>
  json<{ updated: WatchedRow[] }>(`/api/v1/items/${itemId}/watched`, {
    method: 'PUT',
    body: JSON.stringify(items ? { played, items } : { played }),
  })

export function postProgress(sessionId: string, positionMs: number, keepalive = false) {
  return api(`/api/v1/playback/sessions/${sessionId}/progress`, {
    method: 'POST',
    body: JSON.stringify({ position_ms: Math.round(positionMs) }),
    keepalive,
  }).catch(() => undefined)
}

export function endSession(sessionId: string, keepalive = false) {
  return api(`/api/v1/playback/sessions/${sessionId}`, {
    method: 'DELETE',
    keepalive,
  }).catch(() => undefined)
}

// ---- admin ----

/// OPS-10: URLs for the session/item diagnostic bundles. Plain hrefs on
/// purpose — `storeTokens` mirrors the access token into the
/// `kahawai_token` cookie, which is how <video>/<img>/EventSource
/// already authenticate, so an <a download> needs no blob plumbing.
export function sessionLogUrl(sessionId: string): string {
  return `/admin/v1/sessions/${encodeURIComponent(sessionId)}/log`
}

export function itemLogUrl(itemId: string): string {
  return `/admin/v1/items/${encodeURIComponent(itemId)}/log`
}

export type PendingEnrollment = {
  csr_fingerprint: string
  module_type: string
  module_id: string
  name: string
}

/// One verified encoder and what it was measured doing (HUB-36).
/// Speeds are realtime multiples; null = never measured, which is not
/// the same as slow.
export type EncoderCap = {
  codec: string
  element: string
  hardware: boolean
  speed_1080?: number | null
  speed_2160?: number | null
}

export type SatelliteCaps = {
  encoders?: EncoderCap[]
  max_sessions?: number
  tonemap?: boolean
  tonemap_speed_1080?: number | null
  tonemap_speed_2160?: number | null
}

/// What a box has ACHIEVED on a kind of work, as opposed to what its
/// benchmark claims. `class` is `{res}|{src}|{dst}[|tm]`.
export type PaceRow = { class: string; multiple: number }

/// The in-process mediahost's stand-in for a certificate fingerprint —
/// AR-5 replaces the link's transport with channels, so there is no TLS
/// identity to pin or revoke. Mirrors Registry::IN_PROCESS; it is part
/// of the admin API's shape, not a private detail.
export const IN_PROCESS = 'in-process'

export type Satellite = {
  module_id: string
  module_type: string
  name: string
  cert_fingerprint: string
  connected: boolean
  disabled: boolean
  capabilities?: SatelliteCaps | null
  pace?: PaceRow[]
  link_bytes_per_sec?: number | null
}

export type AdminSession = {
  session_id: string
  username: string | null
  title: string | null
  mode: string
  module_id: string
  idle_secs: number
  streams: StreamVerdict | null
}

export const adminEnrollments = () =>
  json<{ pending: PendingEnrollment[] }>('/admin/v1/enrollments')
export const adminApprove = (code: string) =>
  json<{ approved: string }>('/admin/v1/enrollments/approve', {
    method: 'POST',
    body: JSON.stringify({ code }),
  })
export const adminSatellites = () => json<{ satellites: Satellite[] }>('/admin/v1/satellites')
export const adminDeleteSatellite = (id: string) =>
  json<unknown>(`/admin/v1/satellites/${id}`, { method: 'DELETE' })

export type LibraryCollection = {
  module_id: string
  collection_id: string
  host_name: string | null
}

export type Library = {
  id: string
  name: string
  media_type: string
  collections: LibraryCollection[]
}

export type ScanState = {
  scanned: number
  failed: number
  skipped: number
  complete: boolean
}

export type CollectionInfo = LibraryCollection & {
  media_type: string
  connected: boolean
  scan: ScanState | null
}

export const adminLibraries = () => json<{ libraries: Library[] }>('/admin/v1/libraries')

export const adminCollections = () =>
  json<{ collections: CollectionInfo[] }>('/admin/v1/collections')

export type ProviderChain = { order: string[]; default: string[] }

export const adminProviders = () =>
  json<{
    tmdb: { configured: boolean }
    tvdb: { configured: boolean }
    anidb: { configured: boolean }
    chains: Record<string, ProviderChain>
  }>('/admin/v1/providers')

/// HUB-5: precedence per media type. Earlier wins a field; later ones
/// fill what it left empty. Applying re-merges from stored answers, so
/// it is instant and sends no provider a request.
export const adminSetChain = (mediaType: string, order: string[]) =>
  json<{ ok: boolean }>(`/admin/v1/providers/chains/${mediaType}`, {
    method: 'POST',
    body: JSON.stringify({ order }),
  })
export const adminSetAnidb = (username: string, password: string, udpApiKey?: string) =>
  json<{ saved: boolean; verified: boolean; error?: string }>('/admin/v1/providers/anidb', {
    method: 'POST',
    body: JSON.stringify({ username, password, udp_api_key: udpApiKey || null }),
  })
export const adminSetTvdbKey = (apiKey: string, pin?: string) =>
  json<{ saved: boolean }>('/admin/v1/providers/tvdb', {
    method: 'POST',
    body: JSON.stringify({ api_key: apiKey, pin: pin || null }),
  })
export const adminSetTmdbKey = (apiKey: string) =>
  json<{ saved: boolean }>('/admin/v1/providers/tmdb', {
    method: 'POST',
    body: JSON.stringify({ api_key: apiKey }),
  })
export const adminEnrichStatus = () =>
  json<{ running: boolean; matched: number; weak: number; missed: number }>('/admin/v1/enrich')
export type MatchCandidate = {
  format?: string | null
  id: number
  title: string
  overview?: string | null
  poster_path?: string | null
  poster_url?: string
  release_date?: string | null
  vote_average?: number | null
  provider: 'tmdb' | 'tvdb'
}

export const adminReviewSearch = (
  kind: string,
  query: string,
  year?: number | null,
  item?: string,
) =>
  json<{ candidates: MatchCandidate[] }>('/admin/v1/enrich/search', {
    method: 'POST',
    body: JSON.stringify({ kind, query, year: year ?? null, item: item ?? null }),
  })
export const adminApplyMatch = (
  itemId: string,
  action: 'pick' | 'confirm' | 'reject',
  candidate?: MatchCandidate,
) =>
  json<{ ok: boolean }>(`/admin/v1/items/${itemId}/match`, {
    method: 'POST',
    body: JSON.stringify({
      action,
      provider: candidate?.provider ?? null,
      candidate: candidate ?? null,
    }),
  })

export const adminRefreshLibrary = (id: string) =>
  json<{ asked: number; offline: number }>(`/admin/v1/libraries/${id}/refresh`, {
    method: 'POST',
    body: '{}',
  })

export const adminEnrichRun = () =>
  json<{ started: boolean }>('/admin/v1/enrich', { method: 'POST' })

export const adminCreateLibrary = (name: string, mediaType: string) =>
  json<{ id: string }>('/admin/v1/libraries', {
    method: 'POST',
    body: JSON.stringify({ name, media_type: mediaType }),
  })

export const adminDeleteLibrary = (id: string) =>
  ok(`/admin/v1/libraries/${id}`, { method: 'DELETE' })

export const adminAttachCollection = (id: string, moduleId: string, collectionId: string) =>
  ok(`/admin/v1/libraries/${id}/collections`, {
    method: 'POST',
    body: JSON.stringify({ module_id: moduleId, collection_id: collectionId }),
  })

export const adminDetachCollection = (id: string, moduleId: string, collectionId: string) =>
  ok(`/admin/v1/libraries/${id}/collections/${moduleId}/${collectionId}`, {
    method: 'DELETE',
  })

export const adminSetSatelliteDisabled = (id: string, disabled: boolean) =>
  ok(`/admin/v1/satellites/${id}/disabled`, {
    method: 'POST',
    body: JSON.stringify({ disabled }),
  })
export const adminSessions = () => json<{ sessions: AdminSession[] }>('/admin/v1/sessions')
export const adminEndSession = (id: string) => ok(`/admin/v1/sessions/${id}`, { method: 'DELETE' })

/// HUB-10. `all_libraries` wins over `libraries`: with it set the list is
/// stored but not consulted, and libraries created later are included.
export type AdminUser = {
  id: string
  username: string
  is_admin: boolean
  all_libraries: boolean
  libraries: string[]
  created_at: number
}

export const adminUsers = () => json<{ users: AdminUser[] }>('/admin/v1/users')

/// HUB-10: promote or demote. The hub refuses to strip your own rights and
/// refuses to demote the last admin, so this cannot lock an operator out — the
/// client does not have to mirror either rule, it just reports what came back.
/// Both refusals are 409, deliberately not the 403 that `require_admin` uses
/// for "this token is not an admin": otherwise a client could not tell
/// re-authenticate from pick-a-different-account.
export const adminSetUserAdmin = (id: string, admin: boolean) =>
  json<{ id: string; is_admin: boolean }>(`/admin/v1/users/${id}/admin`, {
    method: 'PUT',
    body: JSON.stringify({ admin }),
  })

export const adminCreateUser = (username: string, password: string, admin: boolean) =>
  json<{ id: string }>('/admin/v1/users', {
    method: 'POST',
    body: JSON.stringify({ username, password, admin }),
  })

export const adminDeleteUser = (id: string) => ok(`/admin/v1/users/${id}`, { method: 'DELETE' })

/// Whole state, not a toggle: the panel holds every box, and sending all
/// of them is what keeps two admins from interleaving into a set neither
/// picked.
export const adminSetUserLibraries = (id: string, allLibraries: boolean, libraries: string[]) =>
  json<{ all_libraries: boolean; libraries: string[] }>(`/admin/v1/users/${id}/libraries`, {
    method: 'PUT',
    body: JSON.stringify({ all_libraries: allLibraries, libraries }),
  })
