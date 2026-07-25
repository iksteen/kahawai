// Same-origin client for the kahawai API. Access tokens ride the
// Authorization header for fetches and a cookie for <video>/HLS requests
// (media elements cannot set headers). 401s trigger one refresh + retry.

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
  } else {
    localStorage.removeItem(LS_ACCESS)
    localStorage.removeItem(LS_REFRESH)
    syncCookie(null)
  }
}

export function accessToken(): string | null {
  return localStorage.getItem(LS_ACCESS)
}

function claims(): { username?: string; admin?: boolean } {
  const t = accessToken()
  if (!t) return {}
  try {
    return JSON.parse(atob(t.split('.')[1]))
  } catch {
    return {}
  }
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
  /// HUB-31: TVDB-style projection of absolute numbering (anime).
  proj_season?: number | null
  proj_episode?: number | null
  sources: number
  /// Enrichment state (movie/show): null = never enriched,
  /// miss/rejected = unmatched, weak = uncertain, auto/manual = good.
  match_confidence?: string | null
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
  video?: { codec: string; width: number; height: number }[]
  audio?: { codec: string; channels: number; language?: string | null }[]
  subtitles?: { format: string; language?: string | null }[]
}

export type Source = {
  path_rel: string
  size: number
  available: boolean
  streams: StreamInfo | null
}

export type ItemMetadata = {
  overview: string | null
  rating: number | null
  premiered: string | null
  confidence: 'auto' | 'weak'
  provider: string
}

export type ItemDetail = Item & {
  sources_detail: Source[]
  show_title?: string | null
  parent_id?: string | null
  metadata?: ItemMetadata
  related?: { kind: string; title: string | null; item_id: string | null }[]
}

/// Local artwork (cover.jpg etc). <img> requests authenticate with the
/// media cookie; 404 = no artwork (hide the img).
export const artworkUrl = (id: string) => `/api/v1/items/${id}/artwork`

export const fetchChildren = (id: string) =>
  json<{ children: Item[] }>(`/api/v1/items/${id}/children`)

export type Subtitle = {
  key: string
  kind: 'embedded' | 'sidecar'
  format: string
  language: string | null
  flattened: boolean
  image?: boolean
}

export const fetchSubtitles = (itemId: string) =>
  json<{ subtitles: Subtitle[] }>(`/api/v1/items/${itemId}/subtitles`)

export const fetchFonts = (itemId: string) =>
  json<{ fonts: string[] }>(`/api/v1/items/${itemId}/fonts`)

// ASS renders faithfully via JASSUB in this player (HUB-32), so the
// flattened warning is history here; other clients still get .vtt.
export const subtitleLabel = (s: Subtitle) =>
  `${s.language ?? 'unknown'} · ${s.format}${s.kind === 'sidecar' ? ' · file' : ''}`

export type LibrarySummary = {
  id: string
  name: string
  media_type: string
  anime_view?: 'seasons' | 'native'
}

export const fetchLibraries = () =>
  json<{ libraries: LibrarySummary[] }>('/api/v1/libraries')

export const fetchItems = (libraryId?: string) =>
  json<{ items: Item[] }>(
    libraryId ? `/api/v1/items?library=${encodeURIComponent(libraryId)}` : '/api/v1/items'
  )

export async function fetchItem(id: string): Promise<ItemDetail> {
  const raw = await json<Item & { sources: Source[] | number }>(`/api/v1/items/${id}`)
  const sources = Array.isArray(raw.sources) ? raw.sources : []
  return { ...(raw as Item), sources: sources.length, sources_detail: sources }
}

export type StreamVerdict = { video: string; audio: string }

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
  /// HUB-33: the audio track the session opened with (user preference
  /// when the client sent none) and whether subs should default on.
  audio_track?: number
  subs_on?: boolean
  streams: StreamVerdict | null
}

export function startSession(
  itemId: string,
  mode: string,
  startMs = 0,
  audioTrack?: number,
  videoTrack = 0,
): Promise<Session> {
  // audio_track omitted → the hub applies the user's dual-audio
  // preference (HUB-33) and reports its pick in the response.
  return json('/api/v1/playback/sessions', {
    method: 'POST',
    body: JSON.stringify({
      item_id: itemId,
      mode,
      start_ms: Math.round(startMs),
      ...(audioTrack !== undefined ? { audio_track: audioTrack } : {}),
      video_track: videoTrack,
    }),
  })
}

export type Pref = { scope: string; key: string; value: string }

export const fetchPrefs = () => json<{ prefs: Pref[] }>('/api/v1/prefs')

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
): Promise<{ part_base_ms: number }> {
  return json(`/api/v1/playback/sessions/${sessionId}/seek`, {
    method: 'POST',
    body: JSON.stringify({
      position_ms: Math.round(positionMs),
      audio_track: audioTrack ?? null,
      video_track: videoTrack ?? null,
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

export type PendingEnrollment = {
  csr_fingerprint: string
  module_type: string
  module_id: string
  name: string
}

export type Satellite = {
  module_id: string
  module_type: string
  name: string
  cert_fingerprint: string
  connected: boolean
  disabled: boolean
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
  anime_view?: 'seasons' | 'native'
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

export const adminProviders = () =>
  json<{
    tmdb: { configured: boolean }
    tvdb: { configured: boolean }
    anidb: { configured: boolean }
  }>('/admin/v1/providers')
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
export type ReviewEntry = {
  item_id: string
  kind: 'movie' | 'show'
  title: string
  year: number | null
  path: string | null
  confidence: 'miss' | 'weak' | 'rejected'
  matched_title: string | null
  premiered: string | null
  provider: string
}

export type MatchCandidate = {
  id: number
  title: string
  overview?: string | null
  poster_path?: string | null
  poster_url?: string
  release_date?: string | null
  vote_average?: number | null
  provider: 'tmdb' | 'tvdb'
}

export const adminReviewList = () =>
  json<{ entries: ReviewEntry[] }>('/admin/v1/enrich/review')
export const adminReviewSearch = (kind: string, query: string, year?: number | null) =>
  json<{ candidates: MatchCandidate[] }>('/admin/v1/enrich/search', {
    method: 'POST',
    body: JSON.stringify({ kind, query, year: year ?? null }),
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

export const adminSetAnimeView = (id: string, view: 'seasons' | 'native') =>
  json<{ anime_view: string }>(`/admin/v1/libraries/${id}/anime-view`, {
    method: 'POST',
    body: JSON.stringify({ anime_view: view }),
  })

export const adminRefreshLibrary = (id: string) =>
  json<{ asked: number; offline: number }>(`/admin/v1/libraries/${id}/refresh`, {
    method: 'POST',
    body: '{}',
  })

export const adminRefreshCollection = (moduleId: string, collectionId: string) =>
  json<{ asked: number; offline: number }>('/admin/v1/collections/refresh', {
    method: 'POST',
    body: JSON.stringify({ module_id: moduleId, collection_id: collectionId }),
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
