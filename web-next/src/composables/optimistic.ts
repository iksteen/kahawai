/// An optimistic write that can be put back.
///
/// The brief: when a change fails, showing an error is fine as long as the
/// on-screen value reverts to what the server holds. The whole difficulty is
/// in "what the server holds", because two changes in quick succession — a
/// drag, then another before the first answers — make three different answers
/// available and only one of them is right.
///
/// Reverting to the value the failing write started from puts the list back
/// past a change that was saved. So the revert target is the last value the
/// server actually CONFIRMED, and only the newest write is allowed to use it.

import { ref } from 'vue'

import { notify } from './notices.ts'

export function useOptimistic<T>(
  /// Where the value lives. Written immediately, and written back on a failure
  /// that is still current.
  show: {
    value: T
  },
) {
  let seq = 0
  let inflight = 0
  let saved = show.value
  /// Which write `saved` came from. The queue guarantees the writes commit in
  /// order; this decides whether a failure is current enough to put anything
  /// back on screen.
  let savedSeq = 0
  const busy = ref(false)

  return {
    busy,
    /// Show `next`, then try to save it.
    async put(next: T, write: () => Promise<unknown>): Promise<boolean> {
      // Nothing outstanding means what is on screen is what the server has.
      if (inflight === 0) saved = show.value
      const mine = ++seq
      inflight++
      busy.value = true
      show.value = next
      try {
        await write()
        // ANY success moves the revert target, not only the newest. Advancing
        // it only for the newest meant an older write succeeding while a newer
        // one was still out left the target at the value from before both — so
        // when the newer one failed, the revert went back past a change the
        // server had accepted and kept.
        if (mine > savedSeq) {
          savedSeq = mine
          saved = next
        }
        return true
      } catch {
        // Only the newest write may put anything back: an older failure
        // arriving after a newer success would drag the screen backwards.
        if (mine === seq) {
          show.value = saved
          notify('Could not save that — put back the way it was.')
        }
        return false
      } finally {
        inflight--
        busy.value = inflight > 0
      }
    },
  }
}
