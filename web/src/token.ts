/// When to refresh the access token.
///
/// Kept apart from api.ts so it can be checked without a browser, and
/// because the decision is worth stating on its own: the token is
/// refreshed BEFORE it expires, not repaired after something fails.
///
/// api() retries a 401 with a fresh token, but it is not the only thing
/// carrying this token. The same token rides the kahawai_token cookie
/// for <video>, <img> and EventSource, and hls.js sends it as a Bearer
/// header from its own XHR. None of those pass through api(), so an
/// expired token just fails the media request — and worse, makes the
/// hub answer 401 where it would have answered 410, so session recovery
/// cannot see its trigger either (observed 2026-08-07: a paused film
/// died this way and had to be restarted by hand).

/// Refresh this long before expiry. The access token lives 15 minutes
/// (auth.rs ACCESS_TTL_SECS), so this costs a request per ~14 minutes.
export const REFRESH_LEAD_MS = 60_000

/// A refresh that failed transiently — hub restarting, link down — is
/// worth retrying. A definitive rejection clears the tokens instead,
/// and then there is nothing left to schedule.
export const REFRESH_RETRY_MS = 30_000

/// Delay until the next refresh. Never negative: a token already inside
/// its lead time, or one a sleeping laptop woke up past, refreshes now.
export function refreshDelayMs(expMs: number, nowMs: number): number {
  return Math.max(0, expMs - nowMs - REFRESH_LEAD_MS)
}
