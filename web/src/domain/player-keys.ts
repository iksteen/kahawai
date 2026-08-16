/// What a keypress means to the player. The component does it; this decides it.

export type PlayerMode = 'window' | 'theater' | 'full'

export type PlayerIntent =
  | { kind: 'toggle-pause' }
  | { kind: 'nudge'; seconds: number }
  | { kind: 'mode'; to: PlayerMode }

/// `null` for a key this screen does not claim, so the browser keeps it.
///
/// Nothing here knows whether the pipeline will honour the intent. Pausing and
/// seeking are refused during a restart or a stand-by, and that rule lives in
/// the two funnels that own it so it covers the buttons too.
export function playerIntent(
  key: string,
  state: { typing: boolean; mode: PlayerMode },
): { intent: PlayerIntent; preventDefault: boolean } | null {
  if (state.typing) return null
  switch (key) {
    case ' ':
    case 'k':
      // Prevented, or the page scrolls under the pointer as it pauses.
      return { intent: { kind: 'toggle-pause' }, preventDefault: true }
    case 'ArrowLeft':
      // The transport buttons' numbers: back to re-hear, forward to skip.
      return { intent: { kind: 'nudge', seconds: -10 }, preventDefault: false }
    case 'ArrowRight':
      return { intent: { kind: 'nudge', seconds: 30 }, preventDefault: false }
    case 't':
      return {
        intent: { kind: 'mode', to: state.mode === 'theater' ? 'window' : 'theater' },
        preventDefault: false,
      }
    case 'f':
      return {
        intent: { kind: 'mode', to: state.mode === 'full' ? 'window' : 'full' },
        preventDefault: false,
      }
    case 'Escape':
      // Unclaimed when there is nothing to leave, so a dialog above the player
      // still gets it.
      return state.mode === 'window'
        ? null
        : { intent: { kind: 'mode', to: 'window' }, preventDefault: false }
    default:
      return null
  }
}

/// A `<select>` counts: the audio and subtitle pickers are selects, and space
/// opens a focused one.
export function isTypingTarget(tagName: string | undefined | null): boolean {
  return tagName === 'INPUT' || tagName === 'SELECT' || tagName === 'TEXTAREA'
}
