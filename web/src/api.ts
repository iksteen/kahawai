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

export async function refreshTokens(): Promise<boolean> {
  const rt = localStorage.getItem(LS_REFRESH)
  if (!rt) return false
  const r = await fetch('/api/v1/auth/refresh', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ refresh_token: rt }),
  })
  if (!r.ok) {
    storeTokens(null)
    return false
  }
  storeTokens(await r.json())
  return true
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
  title: string
  year: number | null
  sources: number
  resume_position_ms: number | null
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

export type ItemDetail = Item & { sources_detail: Source[] }

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
  streams: StreamVerdict | null
}

export function startSession(
  itemId: string,
  mode: string,
  startMs = 0,
): Promise<Session> {
  return json('/api/v1/playback/sessions', {
    method: 'POST',
    body: JSON.stringify({ item_id: itemId, mode, start_ms: Math.round(startMs) }),
  })
}

/// Seek-restart: the pipeline restarts at the offset; re-attach the player.
export function seekSession(sessionId: string, positionMs: number) {
  return api(`/api/v1/playback/sessions/${sessionId}/seek`, {
    method: 'POST',
    body: JSON.stringify({ position_ms: Math.round(positionMs) }),
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

export const adminSetSatelliteDisabled = (id: string, disabled: boolean) =>
  api(`/admin/v1/satellites/${id}/disabled`, {
    method: 'POST',
    body: JSON.stringify({ disabled }),
  })
export const adminSessions = () =>
  json<{ sessions: AdminSession[] }>('/admin/v1/sessions')
export const adminEndSession = (id: string) =>
  api(`/admin/v1/sessions/${id}`, { method: 'DELETE' })
