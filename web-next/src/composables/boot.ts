/// Starting up: which screen, and what to do when the hub does not answer.
///
/// One place, mounted once by the app root. The phase it settles on decides
/// whether there is an app at all, so nothing that renders inside the app can
/// own it.

import { onScopeDispose, readonly, ref } from 'vue'

import { onTokensCleared, restoreSession } from '../api/session.ts'
import { bootstrap as fetchBootstrap } from '../api/generated/kahawai.ts'
import { endedNote, type Phase, phaseFor } from '../domain/auth.ts'
import { sentence } from '../domain/refusal.ts'

/// Shorter than a session start's ceiling: this is a database read and a token
/// check, and the page is blank until it lands.
export const BOOTSTRAP_TIMEOUT_MS = 10_000

export function useBoot() {
  const phase = ref<Phase>('boot')
  /// The bootstrap request itself failed. Distinct from every other error in
  /// the app because it happens before there IS an app: no header, no route,
  /// nothing to put a notice on.
  const bootError = ref('')
  const setupAvailable = ref(false)
  const setupUrl = ref<string | undefined>(undefined)
  const note = ref('')

  /// Registered once and read at the time it fires, so it sees the phase the
  /// app is actually in rather than the one it started in.
  ///
  /// The guard comes FIRST. A tab already at the sign-in screen is not being
  /// thrown out of anything, and a note appearing on it because a peer tab
  /// signed out would be an explanation for something that did not happen
  /// here.
  const current = (deliberate: boolean) => {
    if (phase.value !== 'app') return
    note.value = endedNote(deliberate)
    phase.value = 'login'
  }
  // The disposer only clears the slot if this callback is still in it — see
  // `onTokensCleared`.
  onScopeDispose(onTokensCleared(current))

  /// Which boot this is. Pressing Try again against a wedged hub starts
  /// another one without stopping the first, and the first one's failure
  /// landing later put "Could not start." over an app somebody had already
  /// signed into — `bootError` outranks every phase.
  let attempt = 0

  async function start() {
    const mine = ++attempt
    try {
      const state = await fetchBootstrap({ signal: AbortSignal.timeout(BOOTSTRAP_TIMEOUT_MS) })
      if (mine !== attempt) return
      bootError.value = ''
      setupAvailable.value = state.setup_available
      setupUrl.value = state.setup_url ?? undefined
      const restored = state.setup_required ? 'anonymous' : await restoreSession()
      // Asked again after the await: a restore is a round trip of its own.
      if (mine !== attempt) return
      phase.value = phaseFor(state, restored)
    } catch (cause) {
      if (mine !== attempt) return
      // NOT the sign-in screen. Conflating "the hub did not answer" with "you
      // are not signed in" sends a signed-in viewer to a password box over one
      // blip, with their tokens still perfectly good — and there is nothing to
      // sign in TO while the hub is unreachable, so the one thing that screen
      // offers cannot work either.
      bootError.value = sentence(cause)
      // Deliberately NOT back to 'boot': that renders nothing, so pressing
      // Try again against a wedged hub gave a blank page for the full ten
      // seconds before the message came back.
    }
  }

  return {
    phase,
    bootError: readonly(bootError),
    setupAvailable: readonly(setupAvailable),
    setupUrl: readonly(setupUrl),
    note,
    start,
  }
}
