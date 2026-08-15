/// Recovering from a session the hub no longer has.
///
/// Sessions die for reasons a client cannot predict: idle reaping
/// (HUB-18), a hub restart, `end_for_user`, a module going away, an
/// admin ending them. So recovery is driven ENTIRELY by what the server
/// says, never by a client-side clock. The web player must not know how
/// long the idle timeout is, because a third-party client (HUB-28)
/// cannot know it either, and any behaviour that depends on guessing it
/// breaks silently the day the constant changes.
///
/// The whole signal is one status code: **404 Not Found** on a session
/// request means the session is unavailable to this account, and the correct
/// response for the account that held it is to start a new one at the current
/// position. AUTH-11 deliberately makes an absent id and another user's live
/// id indistinguishable; session ids are not bearer capabilities.
export const SESSION_GONE = 404

/// **503 Service Unavailable** on a session START means the item's bytes are
/// on a mediahost that is not connected. Nothing is wrong with the item and
/// nothing is wrong with the request: the same call may succeed once the
/// host is back, so the client waits and asks again rather than giving up.
///
/// Distinct from 409, which the endpoint now uses only for refusals that are
/// about the ITEM and will refuse forever — no sources, unplayable. The
/// account's stream cap moved out to 429; see `SESSION_CAP`.
export const SOURCE_OFFLINE = 503

/// **429 Too Many Requests** on a session start means this account already
/// holds as many sessions as it may.
///
/// Like 503 it clears by itself, but not for the same reason and not on the
/// same terms: 503 is weather nobody can influence, and this one clears when
/// somebody stops watching something. That difference decides what a caller
/// should do, so it is `busy` rather than `wait` — see `StartRetry`.
export const SESSION_CAP = 429

/// True when a thrown request failure says the source's host is away. Takes
/// `unknown` because it sits in a `catch`, and anything that is not an
/// `ApiError` with this status is some other problem.
export function isSourceOffline(e: unknown): boolean {
  return typeof e === 'object' && e !== null && (e as { status?: number }).status === SOURCE_OFFLINE
}

/// True when a response says the session is gone. Takes `undefined`
/// because the API helpers resolve to it on network failure, which is
/// NOT a dead session and must not trigger a restart.
export function isSessionGone(r: { status: number } | undefined): boolean {
  return r?.status === SESSION_GONE
}

/// True when a THROWN request failure says the session is gone. The sibling
/// of `isSessionGone`, for the calls that reject rather than resolve — a seek
/// or a track switch. Those catches used to report session absence to the
/// viewer as prose instead of acting on the status contract above.
export function isSessionDead(e: unknown): boolean {
  return typeof e === 'object' && e !== null && (e as { status?: number }).status === SESSION_GONE
}

/// What to do about a session START that failed.
///
/// `wait` — nothing is wrong with the item and the condition clears itself:
/// 503 (the mediahost is away), no answer at all (connection refused, DNS, an
/// aborted timeout — everything `api()` wraps as `Offline`), or a 5xx from the
/// hub or from a proxy in front of it. That last one is the case a narrower
/// rule got wrong: most deployments put a reverse proxy in front, so an
/// ordinary hub restart is a 502 with an HTML body rather than a dropped
/// connection, and treating it as final stopped an album for good.
///
/// `busy` — 429: this ACCOUNT is at its stream cap. It clears by itself, like
/// `wait`, and it is deliberately not the same answer, because the two want
/// opposite things on screen. Nobody can hurry a mediahost back, so a player
/// stands by and says so; the cap clears when a person stops watching
/// something, which is a thing they can go and do — and the hub's own message
/// ("close one first") already says it. A background queue treats `busy`
/// exactly like `wait`; a player in front of somebody does not.
///
/// It used to be a 409 indistinguishable from "this item has no sources",
/// which is the whole reason the album queue's `REFUSAL_TRIES` had to guess at
/// a number.
///
/// `maybe` — a 409: about the ITEM, and it will refuse again forever. No
/// sources, or nothing this client can be served.
export type StartRetry = 'wait' | 'busy' | 'maybe'

export function startRetry(e: unknown): StartRetry {
  // No answer at all: `api()` wraps every fetch-level failure as `Offline`, an
  // aborted timeout included. It says nothing about the item.
  if (e instanceof Error && e.name === 'Offline') return 'wait'
  const status = typeof e === 'object' && e !== null ? (e as { status?: number }).status : undefined
  // A gateway status is the hub not being there to answer, which is the
  // restart case. 500 is deliberately NOT one: it is the hub answering, and
  // answering that it failed, which for a given item is usually persistent.
  // Waiting on it puts the player in a stand-by dialog that names a cause
  // which is not the cause and carries no Try again. Anything thrown on this
  // side without a status — a malformed body, a bug here — is not weather
  // either.
  const code = typeof e === 'object' && e !== null ? (e as { code?: string }).code : undefined
  // Not every 503 is weather. Two of them — the hub having no administrator
  // yet, and a provider with no credentials — clear only when an OPERATOR
  // acts, and the player's stand-by dialog says the machine holding the file
  // has stopped answering and offers one button. A hub restarted onto an empty
  // database would have left a tab standing by for ever with the real answer
  // sitting unread in `code`.
  if (status === SOURCE_OFFLINE) return code === 'source_offline' ? 'wait' : 'maybe'
  if (status === 502 || status === 504) return 'wait'
  // The account's own cap, which is neither weather nor a verdict about the
  // item. Its own answer for the reason `StartRetry` gives.
  //
  // The CODE as well as the status, and this is the one place a code is read.
  // A 429 is not necessarily the hub's: a reverse proxy or WAF in front of it
  // rate-limits with its own, and an HTML body, so `code` is undefined. Taking
  // that for the stream cap would spend `BUSY_TRIES` on it — five minutes of
  // asking every five seconds, at a rate limiter, which is what rate limiters
  // extend their window for. Unrecognised, it falls through to `maybe`.
  if (status === SESSION_CAP && code === 'session_cap') return 'busy'
  return 'maybe'
}

/// Two recoveries at the same position mean the first one never played:
/// something is systematically wrong and restarting again would spawn
/// sessions forever, against a per-user cap of 4. So a recovery is
/// allowed only when the last one for this key made progress.
const SAME_POSITION_MS = 1000
/// After this long, a repeat at the same position is a fresh problem
/// (paused on one frame for an hour, say) rather than a spin.
const LOOP_WINDOW_MS = 60_000

/// Module-scoped on purpose: a successful recovery REMOUNTS the player
/// (it is keyed on session id), so anything held in component state or
/// a ref is wiped exactly when the guard needs to remember.
///
/// One entry per key, not one entry total. A single slot was cleared by any
/// recovery for a different key, and the keys alternate by construction: the
/// music queue warms an idle slot, so its two sessions overwrote each other's
/// entry, and a film playing alongside a queue had its entry cleared by the
/// queue's. In both cases the loop the guard exists to stop ran unbounded.
const last = new Map<string, { at: number; when: number }>()
/// Bounded so a long session cannot grow it without limit. Far above the
/// per-user session cap, so it never evicts an entry that is still live.
const MAX_KEYS = 32

/// May we restart `key` (an item or queue id) at `atMs`? Records the
/// attempt when it returns true.
export function mayRecover(key: string, atMs: number, now: number): boolean {
  const prev = last.get(key)
  if (prev && Math.abs(prev.at - atMs) < SAME_POSITION_MS && now - prev.when < LOOP_WINDOW_MS) {
    return false
  }
  // Re-inserting moves it to the end, so the oldest key is the first out.
  last.delete(key)
  last.set(key, { at: atMs, when: now })
  if (last.size > MAX_KEYS) last.delete(last.keys().next().value!)
  return true
}

/// Forget what was tried. Two callers, and both are legitimate: the tests,
/// and a viewer pressing Try again. The guard exists to stop the player
/// restarting ITSELF forever; somebody who pressed a button is not a loop,
/// and refusing them because an automatic attempt already used up the
/// position would be the guard working against the person it protects.
export function forgetRecoveries() {
  last.clear()
}
