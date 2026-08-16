/// Recovering from a session the hub no longer has.
///
/// Sessions die for reasons a client cannot predict: idle reaping (HUB-18), a
/// hub restart, `end_for_user`, a module going away, an admin ending them. So
/// recovery is driven ENTIRELY by what the server says, never by a client-side
/// clock — see the no-backend-constants rule. A third-party client (HUB-28)
/// cannot know the idle timeout either, and any behaviour that depends on
/// guessing it breaks silently the day the constant changes.
///
/// The whole signal is one status: **404 Not Found** on a session request means
/// the session is unavailable to this account, and the right answer for the
/// account that held it is to start a new one at the current position. AUTH-11
/// deliberately makes an absent id and another user's live id
/// indistinguishable; session ids are not bearer capabilities.
///
/// WHETHER a session start is worth repeating is `retry` in `api/errors.ts`,
/// for every request in the app. HOW MANY TIMES is `startCeiling` below, and
/// only a repeating caller needs it.

import { ApiError, retry } from '../api/errors.ts'

export const SESSION_GONE = 404

/// How many times a caller that asks on a TIMER may ask again.
///
/// `retry` says whether the answer could change; this says how long to keep
/// believing that. The two are different questions, and only something that
/// repeats on its own needs the second — a page with a Try again button asks
/// the person.
///
/// The queue used to bound every self-clearing refusal at three, because the
/// hub said 409 both for "this item has no sources" and for "too many
/// concurrent streams" and the client could not tell them apart. It now says
/// `unplayable` and `session_cap`, so the ceiling can be per condition:
///
/// - **No ceiling** for weather. A mediahost that is away (503), a hub behind a
///   proxy that is restarting (502/504), a request that got no answer at all:
///   nothing is wrong with the item, and UI-19 is that the client waits it out
///   for as long as it takes.
/// - **A minute** for the stream cap. It does clear when somebody stops
///   watching something — but a queue holds two sessions of its own, so with a
///   low enough `max_sessions_per_user` the album's warm slot is refused by the
///   album's own active one and the condition can NEVER clear. Unbounded, that
///   tab posts a session start every five seconds for as long as it is open.
/// - **Three** for a 500. That is the hub answering, and answering that it
///   failed, which for a given item is usually persistent.
///
/// Returns 0 for a refusal not worth repeating at all.
export const CAP_TRIES = 12
export const HUB_ERROR_TRIES = 3

export function startCeiling(cause: unknown): number | null {
  if (!retry(cause)) return 0
  if (!(cause instanceof ApiError)) return null
  if (cause.code === 'session_cap') return CAP_TRIES
  if (cause.status === 500) return HUB_ERROR_TRIES
  return null
}

/// True when a request failure says the session is gone.
export function isSessionGone(error: unknown): boolean {
  return error instanceof ApiError && error.status === SESSION_GONE
}

/// Two recoveries at the same position mean the first one never played:
/// something is systematically wrong and restarting again would spawn sessions
/// for ever, against a per-user cap. So a recovery is allowed only when the
/// last one for this key made progress.
const SAME_POSITION_MS = 1000
/// After this long, a repeat at the same position is a fresh problem (paused
/// on one frame for an hour, say) rather than a spin.
const LOOP_WINDOW_MS = 60_000

/// Module-scoped on purpose: a successful recovery may remount whatever asked
/// for it, so anything held in component state is wiped exactly when the guard
/// needs to remember.
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

/// May we restart `key` (an item id) at `atMs`? Records the attempt when it
/// returns true.
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

/// Forget what was tried. Two callers, and both are legitimate: the tests, and
/// a viewer pressing Try again. The guard exists to stop playback restarting
/// ITSELF for ever; somebody who pressed a button is not a loop, and refusing
/// them because an automatic attempt already used up the position would be the
/// guard working against the person it protects.
export function forgetRecoveries() {
  last.clear()
}
