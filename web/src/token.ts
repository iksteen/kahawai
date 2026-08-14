/// When to refresh the access token.
///
/// Kept apart from api.ts so it can be checked without a browser, and
/// because the decision is worth stating on its own: the token is
/// refreshed BEFORE it expires, not repaired after something fails.
///
/// `api()` retries a 401 with a fresh access token. Native media, images and
/// EventSource instead use the server-managed `kahawai_media` cookie; hls.js
/// reads the current in-memory access token for each XHR. Refreshing ahead of
/// expiry rotates both credentials before either kind of request fails.

/// Refresh this long before expiry. The access token lives 15 minutes
/// (auth.rs ACCESS_TTL_SECS), so this costs a request per ~14 minutes.
export const REFRESH_LEAD_MS = 60_000

/// A refresh that failed transiently — hub restarting, link down — is
/// worth retrying. A definitive rejection clears the in-memory access token,
/// and then there is nothing left to schedule.
export const REFRESH_RETRY_MS = 30_000

/// Delay until the next refresh. Never negative: a token already inside
/// its lead time, or one a sleeping laptop woke up past, refreshes now.
export function refreshDelayMs(expMs: number, nowMs: number): number {
  return Math.max(0, expMs - nowMs - REFRESH_LEAD_MS)
}
