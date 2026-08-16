/// What a refusal MEANS to this app, in one place.
///
/// The hub publishes a code and a status; this turns them into the handful of
/// answers a screen can act on. Keeping it here rather than at each call site
/// is the point — the same 403 must not be a toast on one page and a sign-out
/// on another because two people reasoned about it separately.
///
/// Nothing here reads `message`. The hub's wording is for a person and is not
/// contractual; every decision below comes from the status or the code.

import { ApiError, OPERATOR_CLEARS, Offline, retry } from '../api/errors.ts'

/// The six kinds the rebuild's brief names, plus the one that is not an
/// answer at all.
///
/// - `offline`   the hub was not reached. Says nothing about the request.
/// - `transient` the same request may work later, on its own.
/// - `blocked`   an operator has to do something first. It will not clear by
///               waiting, and telling somebody to wait is worse than useless.
/// - `signedOut` the credentials are gone or no longer good.
/// - `denied`    authenticated, and not allowed. A different account might be.
/// - `refused`   the request reached the hub and the hub said no, finally.
/// - `broken`    the hub failed at something that should have worked.
export type Kind =
  | 'offline'
  | 'transient'
  | 'blocked'
  | 'signedOut'
  | 'denied'
  | 'refused'
  | 'broken'

export function kindOf(e: unknown): Kind {
  if (e instanceof Offline) return 'offline'
  if (!(e instanceof ApiError)) return 'broken'
  // The one list, shared with `retry`. Two copies drifted the moment a third
  // code was added to only one of them: a stand-by offered on something that
  // never clears, or the reverse.
  if (OPERATOR_CLEARS.includes(e.code ?? '')) return 'blocked'
  if (e.status === 401) return 'signedOut'
  if (e.status === 403) return 'denied'
  if (e.status >= 500 && e.status !== 503) return 'broken'
  return retry(e) ? 'transient' : 'refused'
}

/// Whether this ends the session rather than the request.
///
/// Only a 401. A 403 is the hub saying this account may not, which
/// re-authenticating as the same person does not change — signing them out
/// for it would be a logout loop on a page they simply cannot open.
export function endsSession(e: unknown): boolean {
  return kindOf(e) === 'signedOut'
}

/// Where the report belongs, by UI-21's test: **is the control that caused
/// this still on screen?**
///
/// If it is — a failed watched-mark, a Settings write, a refused next episode
/// — the button is right there and pressing it again IS the retry, so a toast
/// with its own action would duplicate a control five seconds before
/// vanishing. If it is not, because the thing that failed is the content
/// itself, the retry has to be anchored where the content is absent.
///
/// So this does not decide for the caller; it answers the one question the
/// caller cannot answer generically, and the caller says which case it is in.
export type Report = 'notice' | 'inline'

export function reportFor(cause: 'action' | 'content'): Report {
  return cause === 'action' ? 'notice' : 'inline'
}

/// The sentence to show, when there is nowhere better to get one.
///
/// `Offline` and `ApiError` both render as their message, which is either the
/// hub's own words or this client's about not reaching it. Anything else is a
/// bug rather than a condition, and `String(x)` on a thrown non-Error gives
/// "[object Object]" — which tells whoever is looking at it nothing, so it is
/// replaced with a sentence that at least says what happened.
export function sentence(e: unknown): string {
  if (e instanceof Error) return e.message || String(e)
  return 'Something went wrong.'
}
