/// The player's health: is it playing, waiting for a host, restarting, or done.
///
/// Seven slots that used to live as four `useState` and three refs in the
/// component. The refs were not caches — listeners read them because a closure
/// captured at render time sees a stale copy, and this machinery is driven
/// almost entirely from listeners. As one reducer the transitions are pure, and
/// the component can hold the current value in one place instead of seven.

export type SessionHealth = {
  /// Resume position held while a mediahost is away; `null` when not waiting.
  ///
  /// AR-6 as the viewer meets it: the bytes are on a host that stopped
  /// answering. Nothing is broken and nothing is lost, so this is a wait rather
  /// than an error, and holding the position is what makes the retry a resume.
  standby: number | null
  /// Why playback stopped for good; empty when it has not.
  gone: string
  /// A capability restart is outstanding.
  restarting: boolean
  /// Why the last one failed, shown beside the mask editor. Cleared when a new
  /// attempt starts, because the old reason is no longer the answer.
  capsError: string
  /// Which restart owns the timeline. 0 is none, and answers for older
  /// generations are superseded rather than late.
  awaitingGen: number
  /// The session died while paused, so pressing play must rebuild it rather
  /// than un-pause a dead element.
  dead: boolean
  /// A recovery is already running. Two detectors notice the same death — the
  /// progress ping and hls.js — and the second one reaching the restart would
  /// be refused as a loop and report a failure over a recovery that worked.
  recovering: boolean
}

export type SessionEvent =
  /// A capability restart: a new session with a different profile. Separate
  /// from the timeline generation below — the component sets these at
  /// different moments and one does not imply the other.
  | { type: 'caps-restart-started' }
  | { type: 'caps-restart-failed'; why: string }
  /// This seek or track switch owns the timeline from now on.
  | { type: 'timeline-taken'; gen: number }
  /// The answer landed. Older generations are ignored.
  | { type: 'restart-settled'; gen: number }
  /// Giving up on `gen`: pauses the picture at the call site, dead here.
  | { type: 'gave-up'; gen: number }
  | { type: 'died-while-paused' }
  | { type: 'play-pressed' }
  | { type: 'recovery-started' }
  | { type: 'recovery-ended' }
  /// The host is not answering — a wait, holding the position it stopped at.
  | { type: 'host-away'; atMs: number }
  /// A real failure, from a start or from the stand-by retry loop.
  | { type: 'stopped'; why: string }
  | { type: 'retry-by-hand' }

export function initialHealth(): SessionHealth {
  return {
    standby: null,
    gone: '',
    restarting: false,
    capsError: '',
    awaitingGen: 0,
    dead: false,
    recovering: false,
  }
}

/// Whether the pipeline is the viewer's to steer. False while a host is away,
/// after a stop, and during a restart — the three phases that outrank the
/// viewer's own pause, so it needs nothing from the element.
///
/// A question rather than a stored value: listeners used to read a ref that
/// shadowed it, which is one more thing to keep in step.
export function isFrozen(s: SessionHealth): boolean {
  return s.standby !== null || s.gone !== '' || s.awaitingGen !== 0
}

export function sessionHealth(s: SessionHealth, e: SessionEvent): SessionHealth {
  switch (e.type) {
    case 'caps-restart-started':
      return { ...s, restarting: true, capsError: '' }
    case 'caps-restart-failed':
      return { ...s, restarting: false, capsError: e.why }
    case 'timeline-taken':
      return { ...s, awaitingGen: e.gen }
    case 'restart-settled':
      return s.awaitingGen === e.gen ? { ...s, awaitingGen: 0 } : s
    case 'gave-up':
      // Unchecked, an older POST answering "no" marks the player dead while a
      // newer restart is still genuinely coming.
      return s.awaitingGen === e.gen ? { ...s, dead: true, awaitingGen: 0 } : s
    case 'died-while-paused':
      return { ...s, dead: true }
    case 'play-pressed':
      return s.dead ? { ...s, dead: false } : s
    case 'recovery-started':
      return s.recovering ? s : { ...s, recovering: true }
    case 'recovery-ended':
      return { ...s, recovering: false }
    case 'host-away':
      return { ...s, standby: e.atMs }
    case 'stopped':
      // Clears any wait: this is the answer the wait was waiting for.
      return { ...s, standby: null, gone: e.why }
    case 'retry-by-hand':
      // Only the stop. Reachable from the stopped dialog, where there is no
      // wait to lift — and a wait is cleared by the retry loop that owns it,
      // not from here.
      return { ...s, gone: '' }
  }
}
