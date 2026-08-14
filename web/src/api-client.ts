/// Request options understood by Kahawai's generated-client mutator.
/// Auth endpoints opt out of the normal 401 refresh path to avoid recursively
/// refreshing the request that is itself refreshing the session.
export type ApiRequestInit = RequestInit & {
  skipAuthRefresh?: boolean
  skipAuthorization?: boolean
  rawResponse?: boolean
}

let token = (): string | null => null
let refresh = (): Promise<boolean> => Promise.resolve(false)

export function configureApiClient(
  accessToken: () => string | null,
  refreshTokens: () => Promise<boolean>,
) {
  token = accessToken
  refresh = refreshTokens
}

/// The hub could not be reached at all: no response, rather than a bad one.
export class Offline extends Error {
  constructor() {
    super('Could not reach the hub.')
    this.name = 'Offline'
  }
  override toString() {
    return this.message
  }
}

/// A failed request, with the status still attached.
export class ApiError extends Error {
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
    if (access && !skipAuthorization && !headers.has('authorization'))
      headers.set('authorization', `Bearer ${access}`)
    if (init.body && !headers.has('content-type')) headers.set('content-type', 'application/json')
    try {
      return await fetch(path, { ...request, headers })
    } catch {
      throw new Offline()
    }
  }

  let response = await go()
  if (response.status === 401 && !init.skipAuthRefresh && (await refresh())) response = await go()
  return response
}

/// Orval's one transport boundary. It preserves empty, JSON, text and binary
/// response bodies rather than assuming every successful operation is JSON.
export async function apiClient<T>(url: string, options: ApiRequestInit = {}): Promise<T> {
  const response = await api(url, options)
  if (options.rawResponse) return response as T
  if (!response.ok)
    throw new ApiError(response.status, (await response.text()) || `${response.status}`)
  if (
    response.status === 204 ||
    response.status === 205 ||
    response.headers.get('content-length') === '0'
  )
    return undefined as T

  const contentType = response.headers.get('content-type') ?? ''
  if (contentType.includes('json')) return response.json() as Promise<T>
  if (
    contentType.startsWith('text/') ||
    contentType.includes('event-stream') ||
    contentType.includes('subtitle')
  )
    return response.text() as Promise<T>
  return response.blob() as Promise<T>
}
