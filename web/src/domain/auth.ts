/// What the app opens on, and what the sign-in form will accept.
///
/// Separate from the composable that performs the requests, so the decisions
/// can be checked without a fetch: which screen, what a password has to be,
/// and why the sign-in screen is showing when nobody asked for it.

/// The four states before there is an app.
///
/// `boot` renders nothing — not a spinner. The bootstrap read is a database
/// hit and a token check, and a spinner that flashes for 40 ms on every load
/// is worse than a blank moment. A boot that is genuinely slow ends in
/// `bootError` instead.
export type Phase = 'boot' | 'setup' | 'login' | 'app'

/// One public endpoint states which screen to open on.
///
/// This used to be read off the STATUS of `/api/v1/items` — 503 meant setup,
/// 401 meant login — which inferred the client's own state from an error path
/// and pulled the whole catalogue (1.4 MB) for a body it discarded.
export function phaseFor(
  bootstrap: { setup_required: boolean },
  restored: 'authenticated' | 'anonymous',
): Exclude<Phase, 'boot'> {
  if (bootstrap.setup_required) return 'setup'
  return restored === 'authenticated' ? 'app' : 'login'
}

/// The hub's rule, mirrored here only to keep the button off until it can
/// succeed — the hub enforces it and its refusal is what a wrong password
/// length actually gets told by. Anything the hub would accept and this
/// refuses would be a bug in this line.
export const MIN_PASSWORD = 12

/// By code point, not by UTF-16 unit. `'🔑'.length` is 2, so a passphrase of
/// six emoji counted as twelve and was let through as long enough.
export function passwordLongEnough(password: string): boolean {
  return [...password].length >= MIN_PASSWORD
}

/// Why the sign-in screen is showing, when it was not asked for.
///
/// A session that expired mid-use lands there with no explanation otherwise,
/// which reads as the app having forgotten you for no reason. Signing out
/// deliberately needs no sentence — you know why you are here.
export function endedNote(deliberate: boolean): string {
  return deliberate ? '' : 'Your session ended. Sign in to carry on.'
}
