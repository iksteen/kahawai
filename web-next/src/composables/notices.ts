/// The one notice host (UX-1).
///
/// Module state rather than a provider: a notice outlives the view that raised
/// it, and every attempt to scope it to a component has ended with a message
/// disappearing because the thing that failed navigated away.
///
/// Deliberately actionless. UI-21 audited every notice site and found that
/// where a report is worth making, the control that caused it is still on
/// screen — so a button here would duplicate one that already exists, five
/// seconds before it vanishes. The two places where the affordance was
/// genuinely missing got inline retries instead, anchored to the absent
/// content.

import { readonly, ref } from 'vue'

/// Long enough to read a sentence, short enough not to sit over the page.
export const NOTICE_MS = 5_000

const current = ref('')
let timer: ReturnType<typeof setTimeout> | undefined

export const notice = readonly(current)

export function notify(message: string) {
  clearTimeout(timer)
  current.value = message
  timer = setTimeout(() => {
    current.value = ''
  }, NOTICE_MS)
}

/// For a test, and for the shell unmounting.
export function clearNotices() {
  clearTimeout(timer)
  current.value = ''
}
