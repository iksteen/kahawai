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
///
/// `code` is the hub's machine-readable reason (`ErrorCode` in the generated
/// model). Undefined when the body was not the hub's — a proxy's 502 page, a
/// truncated response — which is itself worth being able to tell apart.
///
/// Branch on the STATUS for whether to retry and on the CODE for what to say.
/// That split is the hub's contract — 429 and 503 clear on their own, every
/// other 4xx is final — and it needs no table of codes here.
///
/// It stops short of a policy, deliberately: THIS app's is `startRetry` in
/// `recovery.ts`, which is narrower about 5xx than the contract allows. A 500
/// is the hub answering that it failed, which for one item does not clear on
/// its own, and standing by on it names a cause that is not the cause. 502 and
/// 504 are a different animal — a hub restarting behind a reverse proxy — and
/// those do wait.
export class ApiError extends Error {
  status: number
  code?: string
  constructor(status: number, message: string, code?: string) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.code = code
  }
  override toString() {
    return this.message
  }
}

/// The hub answers every 4xx and 5xx with `{code, message}`. A body that is
/// not that shape is still shown: a reverse proxy in front of the hub answers
/// its own failures, and "502 Bad Gateway" as HTML is more use on screen than
/// a bare status number.
export async function apiFailure(response: Response): Promise<ApiError> {
  const text = await response.text().catch(() => '')
  try {
    const body: unknown = JSON.parse(text)
    if (
      typeof body === 'object' &&
      body !== null &&
      typeof (body as { message?: unknown }).message === 'string'
    ) {
      const { code, message } = body as { code?: unknown; message: string }
      return new ApiError(response.status, message, typeof code === 'string' ? code : undefined)
    }
  } catch {
    // not JSON: fall through to the raw text
  }
  return new ApiError(response.status, text || `${response.status}`)
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
  if (!response.ok) throw await apiFailure(response)
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
