/// What the player is doing, as one value.
///
/// These five states are mutually exclusive on screen, but each of the four
/// overlays used to decide for itself by ANDing the others' conditions —
/// `paused && standby === null && !seeking` for the play veil, and so on. Every
/// new overlay meant revisiting every existing condition, and the one time that
/// was missed the play button rendered underneath the "Playback stopped"
/// dialog, unclickable but visible through the scrim.
///
/// Ranked once, here, so that is unrepresentable: a sixth overlay is a line in
/// a list rather than four conditions to find.
export type Phase = 'standby' | 'gone' | 'restarting' | 'paused' | 'playing'

/// Highest priority first.
///
/// A wait outranks a stop, because by the time we are standing by for a host
/// the earlier failure is stale. A stop outranks a restart. And a restart
/// outranks the viewer's own pause, because the restart PAUSED the element
/// itself — offering that back as a pause of theirs to undo invites a click
/// that fights the pipeline, which is exactly what a play circle over a
/// restarting picture did.
export function playerPhase(s: {
  /// Resume position held while a mediahost is away; `null` when not waiting.
  standby: number | null
  /// Why playback stopped for good; empty when it has not.
  gone: string
  /// A pipeline restart is outstanding — its picture has not arrived.
  restarting: boolean
  /// The element's own state.
  paused: boolean
}): Phase {
  if (s.standby !== null) return 'standby'
  if (s.gone) return 'gone'
  if (s.restarting) return 'restarting'
  return s.paused ? 'paused' : 'playing'
}
