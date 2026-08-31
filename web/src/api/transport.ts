/// Orval's one transport boundary: every generated binding calls `apiClient`.
///
/// It owns three things and nothing else — the bearer header, the single
/// retry after a token refresh, and turning a refusal into an `ApiError`.
/// Anything that decides what a refusal MEANS belongs above this.

import { ApiError, Offline } from './errors.ts'

export type ApiRequestInit = RequestInit & {
  /// Auth endpoints opt out of the 401 refresh path, or refreshing the session
  /// would recursively refresh the request that is doing the refreshing.
  skipAuthRefresh?: boolean
  skipAuthorization?: boolean
  rawResponse?: boolean
}

let token = (): string | null => null
let refresh = (): Promise<boolean> => Promise.resolve(false)

/// Wired up by the auth session (phase 3). Injected rather than imported so
/// the generated bindings cannot pull the session in behind the transport and
/// make a cycle out of it.
export function configureApiClient(
  accessToken: () => string | null,
  refreshTokens: () => Promise<boolean>,
) {
  token = accessToken
  refresh = refreshTokens
}

/// The hub answers every 4xx and 5xx with `{code, message, request_id}`. A body that is
/// not that shape is still shown: a reverse proxy in front of the hub answers
/// its own failures, and "502 Bad Gateway" as HTML is more use on screen than
/// a bare status number.
export async function apiFailure(response: Response): Promise<ApiError> {
  const text = await response.text().catch(() => '')
  let error = new ApiError(response.status, text || `${response.status}`)
  try {
    const body: unknown = JSON.parse(text)
    if (
      typeof body === 'object' &&
      body !== null &&
      typeof (body as { code?: unknown }).code === 'string' &&
      typeof (body as { message?: unknown }).message === 'string' &&
      typeof (body as { request_id?: unknown }).request_id === 'string'
    ) {
      const { code, message, request_id } = body as {
        code: string
        message: string
        request_id: string
      }
      error = new ApiError(response.status, message, code, request_id)
    }
  } catch {
    // Not JSON: the raw text stands.
  }
  // Only when the hub actually said so. Assigning `undefined` to an optional
  // field is not the same as leaving it out — `exactOptionalPropertyTypes`
  // makes that distinction, and the distinction is the point: "no
  // Retry-After" and "Retry-After: nonsense" are both absent, not zero.
  const seconds = Number(response.headers.get('retry-after'))
  if (Number.isFinite(seconds) && seconds > 0) error.retryAfterSecs = seconds
  return error
}

export async function api(path: string, init: ApiRequestInit = {}): Promise<Response> {
  const go = async () => {
    const {
      skipAuthRefresh: _skipAuthRefresh,
      skipAuthorization,
      rawResponse: _rawResponse,
      ...request
    } = init
    const headers = new Headers(init.headers)
    const access = token()
    if (access && !skipAuthorization && !headers.has('authorization')) {
      headers.set('authorization', `Bearer ${access}`)
    }
    if (init.body && !headers.has('content-type')) {
      headers.set('content-type', 'application/json')
    }
    try {
      return await fetch(path, { ...request, headers })
    } catch (error) {
      // Asked of the SIGNAL, not of the error. A caller that aborts with a
      // reason — `controller.abort(new Error('cancelled'))` — rejects with
      // that reason, which is an ordinary Error and matches no DOMException
      // name: reading the name alone reported a cancel the user performed as
      // "Could not reach the hub."
      if (init.signal?.aborted) {
        // The caller's own deadline, expired. Everything else here is a
        // deliberate cancellation, which is not a failure to report — it goes
        // back untouched, for the caller that asked for it.
        const reason: unknown = init.signal.reason
        if (reason instanceof DOMException && reason.name === 'TimeoutError') {
          throw new Offline('The hub did not answer in time.')
        }
        throw error
      }
      throw new Offline()
    }
  }

  const response = await go()
  // One retry, and only after a refresh that worked. A second 401 is an
  // answer, not a race.
  if (response.status === 401 && !init.skipAuthRefresh && (await refresh())) return go()
  return response
}

/// Preserves empty, JSON, text and binary bodies rather than assuming every
/// successful operation is JSON — playlists, subtitle files and artwork all
/// come through here.
export async function apiClient<T>(url: string, options: ApiRequestInit = {}): Promise<T> {
  const response = await api(url, options)
  if (options.rawResponse) return response as T
  if (!response.ok) throw await apiFailure(response)
  if (
    response.status === 204 ||
    response.status === 205 ||
    response.headers.get('content-length') === '0'
  ) {
    return undefined as T
  }

  const contentType = response.headers.get('content-type') ?? ''
  if (contentType.includes('json')) return response.json() as Promise<T>
  if (
    contentType.startsWith('text/') ||
    contentType.includes('event-stream') ||
    contentType.includes('subtitle')
  ) {
    return response.text() as Promise<T>
  }
  return response.blob() as Promise<T>
}
