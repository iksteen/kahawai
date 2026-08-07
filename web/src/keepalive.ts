import { postProgress } from './api'

/// Keeping a play session alive while the client is still there.
///
/// HUB-18 reaps a session after 90 s with no fetch and no progress
/// ping. That is right for an abandoned session and wrong for a
/// present one, and a player stops fetching more often than it stops
/// existing: paused with a full buffer, or a track preloaded for
/// gapless handover and waiting its turn. Neither reads a byte.
///
/// Losing the session to the reaper is not recoverable from here.
/// `Sessions::end` deletes a remux session's segment directory and
/// hands a transcode session's slot back to the pool, and neither
/// player has a path that restarts a session it lost — the only 404
/// handling in the video player is for seek-restarts, where it
/// deliberately stops loading. So the symptom is a viewer who pauses,
/// comes back, and finds the film dead.
///
/// The ping is the liveness signal HUB-18 actually asks for, so send
/// it whether or not the playhead is moving — but bounded, because the
/// reaper is right about the case it was built for. Someone who paused
/// and walked away must not hold a transcoder slot all night.
const PING_MS = 10_000

/// How long a session is held once nothing is advancing. Long enough
/// to cover a pause a viewer means to come back from, short enough
/// that a forgotten tab frees its slot the same hour.
const IDLE_LIMIT_MS = 30 * 60_000

/// Ping `position()` every 10 s for as long as it keeps changing, and
/// for IDLE_LIMIT_MS after it stops. Returns a cancel function.
///
/// A position that never moves is the preload case — it pings zero,
/// which is the truth for a track that has not started — and it falls
/// out of the same rule rather than needing its own.
export function keepSessionAlive(sessionId: string, position: () => number) {
  let last = NaN
  let stalledMs = 0
  const tick = setInterval(() => {
    const pos = position()
    if (pos === last) {
      stalledMs += PING_MS
      if (stalledMs >= IDLE_LIMIT_MS) return
    } else {
      last = pos
      stalledMs = 0
    }
    void postProgress(sessionId, pos)
  }, PING_MS)
  return () => clearInterval(tick)
}
