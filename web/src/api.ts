// Same-origin client for the kahawai API. Access tokens ride the
// Authorization header for fetches and a cookie for <video>/HLS requests
// (media elements cannot set headers). 401s trigger one refresh + retry.

import { buildProfile } from './capabilities'

import { REFRESH_RETRY_MS, refreshDelayMs } from './token'

const LS_ACCESS = 'kahawai.access'
const LS_REFRESH = 'kahawai.refresh'

export type Tokens = { access_token: string; refresh_token: string }

function syncCookie(token: string | null) {
  document.cookie = token
    ? `kahawai_token=${token}; path=/; SameSite=Lax`
    : 'kahawai_token=; path=/; Max-Age=0'
}

export function storeTokens(t: Tokens | null) {
  if (t) {
    localStorage.setItem(LS_ACCESS, t.access_token)
    localStorage.setItem(LS_REFRESH, t.refresh_token)
    syncCookie(t.access_token)
    // Every path that lands a token — login, refresh — re-arms the
    // timer from the new expiry, so the schedule is never stale.
    keepTokenFresh()
  } else {
    localStorage.removeItem(LS_ACCESS)
    localStorage.removeItem(LS_REFRESH)
    syncCookie(null)
    clearTimeout(refreshTimer)
  }
}

export function accessToken(): string | null {
  return localStorage.getItem(LS_ACCESS)
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
/// where it would have answered 410, session recovery could not see
/// its own trigger either.
export function keepTokenFresh() {
  clearTimeout(refreshTimer)
  const exp = claims().exp
  if (!exp) return
  refreshTimer = setTimeout(() => {
    void refreshTokens().then((ok) => {
      // On success storeTokens() reschedules from the NEW expiry.
      if (!ok && accessToken()) refreshTimer = setTimeout(keepTokenFresh, REFRESH_RETRY_MS)
    })
  }, refreshDelayMs(exp * 1000, Date.now()))
}

export function username(): string {
  return claims().username ?? ''
}

export function isAdmin(): boolean {
  return claims().admin === true
}

// Single-flight: refresh tokens rotate server-side, so concurrent 401s
// must share ONE refresh — the losers of the race would otherwise send
// the already-rotated token, fail, and wipe the fresh session.
let refreshInFlight: Promise<boolean> | null = null

export function refreshTokens(): Promise<boolean> {
  refreshInFlight ??= (async () => {
    try {
      const rt = localStorage.getItem(LS_REFRESH)
      if (!rt) return false
      const r = await fetch('/api/v1/auth/refresh', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ refresh_token: rt }),
      })
      if (!r.ok) {
        // Only a definitive rejection means the session is dead;
        // transient failures keep the tokens for a later retry.
        if (r.status === 401 || r.status === 403) storeTokens(null)
        return false
      }
      storeTokens(await r.json())
      return true
    } catch {
      return false
    } finally {
      refreshInFlight = null
    }
  })()
  return refreshInFlight
}

export async function api(path: string, init?: RequestInit): Promise<Response> {
  const go = () => {
    const headers: Record<string, string> = { ...(init?.headers as Record<string, string>) }
    const t = accessToken()
    if (t) headers['Authorization'] = `Bearer ${t}`
    if (init?.body) headers['content-type'] = 'application/json'
    return fetch(path, { ...init, headers })
  }
  let r = await go()
  if (r.status === 401 && (await refreshTokens())) r = await go()
  return r
}

export async function json<T>(path: string, init?: RequestInit): Promise<T> {
  const r = await api(path, init)
  if (!r.ok) throw new Error((await r.text()) || `${r.status}`)
  return r.json()
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
  video?: { codec: string; width: number; height: number; profile?: string | null; level?: string | null }[]
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
export const artworkUrl = (id: string, version?: number | null, size?: 'thumb' | 'card') => {
  const p = new URLSearchParams()
  if (size) p.set('size', size)
  if (version) p.set('v', String(version))
  const q = p.toString()
  return `/api/v1/items/${id}/artwork${q ? `?${q}` : ''}`
}

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

export const fetchLibraries = () =>
  json<{ libraries: LibrarySummary[] }>('/api/v1/libraries')

/// How a library is browsed. `sort` is one of the names the hub knows
/// (`title`, `-title`, `year`, `-year`, `added`, `-added`); anything else
/// falls back to title there rather than erroring.
export type ItemsPage = {
  library?: string
  q?: string
  sort?: string
  limit?: number
  offset?: number
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
  return json<{ items: Item[]; total: number; limit: number; offset: number }>(
    `/api/v1/items?${p.toString()}`,
  )
}

/// Which screen to open on. Public: needs no token, and answers before
/// setup has happened.
export type Bootstrap = { setup_required: boolean; authenticated: boolean }

export const fetchBootstrap = () => json<Bootstrap>('/api/v1/bootstrap')

/// What this client would actually be served, for the profile it asked
/// with — the converged half of the item resource.
export type Negotiated = {
  source: { module_id: string; collection_id: string; path_rel: string } | null
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
  const raw = await json<
    Item & { sources: Source[] | number; negotiated?: Negotiated }
  >(`/api/v1/items/${id}`, {
    method: 'QUERY',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ profile: buildProfile() }),
  })
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
export type StreamVerdict = { video: string; audio: string; subtitles?: SubtitleVerdict[] }

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
/// source-aware refinements → debug mask → session. Shared by the
/// detail page's play button and the player's capability restart, so
/// both negotiate from an identical profile.
export async function startPlaybackSession(
  item: ItemDetail,
  startMs = 0,
  audioTrack = 0,
  videoTrack = 0,
): Promise<Session> {
  let cap: number | undefined
  try {
    const p = await fetchPrefs()
    const v = p.prefs.find((x) => x.scope === '' && x.key === 'bandwidth_kbps')?.value
    cap = v ? Number(v) : undefined
  } catch {
    // prefs unavailable → no cap
  }
  // Source-aware precision: probe the exact strings the announced
  // streams call for (profile/level from the hub's own probing).
  const announced = item.sources_detail.flatMap((s) => s.streams?.video ?? [])
  return startSession(item.id, buildProfile(cap, announced), startMs, audioTrack, videoTrack)
}

/// Music plays direct by operator contract (browsers decode flac/mp3
/// natively; HUB-19 owns future music delivery shapes).
export function startSessionDirect(itemId: string): Promise<Session> {
  return json('/api/v1/playback/sessions', {
    method: 'POST',
    body: JSON.stringify({ item_id: itemId, mode: 'direct' }),
  })
}

export type Pref = { scope: string; key: string; value: string }

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
  const subs =
    subsMem === 'off'
      ? []
      : subsMem
        ? [subsMem]
        : list(get('', `subs.${mediaType}`))
  // Top precedence (subtitle unification): THIS item's exact track id
  // — the only spelling that can name a specific downloaded/OCR row.
  // Callers honor it iff the id is still in the fetched list.
  const exactSub = get(itemId, 'subs.track')
  const subTrack = exactSub && /^\d+$/.test(exactSub) ? Number(exactSub) : null
  return { audioTrack: audioTrack ?? 0, subs, subTrack }
}

/// First subtitle matching the wishlist, in wishlist order ('any'
/// matches the first text sub). null = leave subtitles off.
export function pickSubtitle(
  wishlist: string[],
  subs: Subtitle[],
): Subtitle | null {
  // Language wishes auto-pick only client-rendered tracks: silently
  // forcing a burn (a video encode restart) is never what a language
  // preference means — burns are explicit picks.
  const auto = (s: Subtitle) => s.delivery === 'text' || s.delivery === 'ass' || s.delivery === 'overlay'
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

export const putPref = (scope: string, key: string, value: string) =>
  json<{ ok: boolean }>('/api/v1/prefs', {
    method: 'PUT',
    body: JSON.stringify({ scope, key, value }),
  })

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
export const adminSatellites = () =>
  json<{ satellites: Satellite[] }>('/admin/v1/satellites')
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

export const adminLibraries = () =>
  json<{ libraries: Library[] }>('/admin/v1/libraries')

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
  api(`/admin/v1/libraries/${id}`, { method: 'DELETE' })

export const adminAttachCollection = (
  id: string,
  moduleId: string,
  collectionId: string,
) =>
  api(`/admin/v1/libraries/${id}/collections`, {
    method: 'POST',
    body: JSON.stringify({ module_id: moduleId, collection_id: collectionId }),
  })

export const adminDetachCollection = (
  id: string,
  moduleId: string,
  collectionId: string,
) =>
  api(`/admin/v1/libraries/${id}/collections/${moduleId}/${collectionId}`, {
    method: 'DELETE',
  })

export const adminSetSatelliteDisabled = (id: string, disabled: boolean) =>
  api(`/admin/v1/satellites/${id}/disabled`, {
    method: 'POST',
    body: JSON.stringify({ disabled }),
  })
export const adminSessions = () =>
  json<{ sessions: AdminSession[] }>('/admin/v1/sessions')
export const adminEndSession = (id: string) =>
  api(`/admin/v1/sessions/${id}`, { method: 'DELETE' })
