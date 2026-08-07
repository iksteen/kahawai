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
/// The whole signal is one status code: **410 Gone** on any
/// `/api/v1/playback/sessions/{id}/…` endpoint means the session is
/// unrecoverable, and the correct response is to start a new one at the
/// current position. 404 on those paths keeps its ordinary meaning —
/// `session_file` answers it for "no such embedded track" on a live
/// session — so the two must never be conflated.
export const SESSION_GONE = 410

/// True when a response says the session is gone. Takes `undefined`
/// because the API helpers resolve to it on network failure, which is
/// NOT a dead session and must not trigger a restart.
export function isSessionGone(r: { status: number } | undefined): boolean {
  return r?.status === SESSION_GONE
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
let last: { key: string; at: number; when: number } | null = null

/// May we restart `key` (an item or queue id) at `atMs`? Records the
/// attempt when it returns true.
export function mayRecover(key: string, atMs: number, now: number): boolean {
  if (
    last &&
    last.key === key &&
    Math.abs(last.at - atMs) < SAME_POSITION_MS &&
    now - last.when < LOOP_WINDOW_MS
  ) {
    return false
  }
  last = { key, at: atMs, when: now }
  return true
}

/// Test seam only — the module remembers across a remount by design.
export function forgetRecoveries() {
  last = null
}
