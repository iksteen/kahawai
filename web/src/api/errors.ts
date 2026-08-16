/// What the hub said when it refused, in the shape the rest of the app reasons
/// about.
///
/// The hub answers every 4xx and 5xx with `{code, message}` — see
/// `crates/kahawai-hub/src/error.rs`. `code` is enumerated and stable;
/// `message` is written for a person and its wording is not contractual, so
/// nothing here may branch on it.

/// No answer, rather than a bad one: the hub could not be reached, or was
/// reached and never replied. One class, because the two are the same thing to
/// everything downstream — nothing was learned, and asking again may work —
/// and because the difference is worth saying only in the sentence on screen.
export class Offline extends Error {
  constructor(message = 'Could not reach the hub.') {
    super(message)
    this.name = 'Offline'
  }
  override toString() {
    return this.message
  }
}

/// A refusal, with the status and the code still attached.
///
/// `code` is undefined when the body was not the hub's — a reverse proxy's
/// error page, a truncated response — which is itself worth being able to tell
/// apart, and is why `retry` below reads the status rather than the code.
export class ApiError extends Error {
  status: number
  /// `undefined` when the body was not the hub's. Spelled `| undefined`
  /// rather than left optional: absent and present-but-unknown are the same
  /// thing here, and `exactOptionalPropertyTypes` would otherwise make
  /// assigning one to the other an error at every call site.
  code: string | undefined
  /// From `Retry-After`, when the hub sent one. See `retryAfter`.
  retryAfterSecs: number | undefined
  constructor(status: number, message: string, code?: string) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.code = code
    this.retryAfterSecs = undefined
  }
  override toString() {
    return this.message
  }
}

/// Whether asking again could work, decided by the STATUS.
///
/// That is the hub's contract and it is deliberately not a table of codes: 429
/// and 503 mean the same request may work later, 5xx is worth a backoff, and
/// every other 4xx is final. A third-party client gets this right knowing
/// nothing about kahawai, which is the property the split exists for.
///
/// Two 503s are the exception and they are NAMED rather than inferred: the hub
/// having no administrator, and a provider with no credentials, clear only
/// when an operator acts. A 503 with no code at all is an intermediary's —
/// HAProxy and ingress-nginx answer it for a backend that is down — and that
/// is the ordinary hub restart, which does clear.
export const OPERATOR_CLEARS = ['setup_required', 'provider_unconfigured']

export function retry(e: unknown): boolean {
  if (e instanceof Offline) return true
  if (!(e instanceof ApiError)) return false
  if (e.status === 503) return !OPERATOR_CLEARS.includes(e.code ?? '')
  // A 429 is only worth retrying when it is the HUB's. A reverse proxy or WAF
  // rate-limiting the tab answers 429 with an HTML body and no code, and
  // asking again is what a rate limiter extends its window for. The old
  // client learned this; carrying over the 503 half of the rule and not this
  // one would have unlearned it.
  if (e.status === 429) return e.code === 'session_cap' || e.code === 'login_throttled'
  return e.status >= 500
}

/// How long the hub says to wait, when it knows. Seconds.
///
/// Only the login lockout carries `Retry-After` — it runs from 30 s to fifteen
/// minutes and nothing else could be guessed from the status. The stream cap
/// deliberately does not: it clears when a person stops watching something.
export function retryAfter(e: unknown): number | undefined {
  return e instanceof ApiError ? e.retryAfterSecs : undefined
}
