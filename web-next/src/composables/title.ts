/// The document's title, and the announcement that goes with it.
///
/// UI-17. Two things a real navigation does for free and a single-page app does
/// not: the browser announces the document title, and it moves the focus to the
/// top of the new document. Neither happens here, so a screen reader user
/// pressing a link is told nothing at all and is left with their focus on a
/// button that no longer exists.
///
/// The title is set for both reasons — the tab strip and the bookmark want it
/// too — and a polite live region carries the same words, because a title
/// change alone is announced by some screen readers and not others.

import { onScopeDispose, readonly, type Ref, ref, watch } from 'vue'

import { documentTitle } from '../domain/titles.ts'
import type { RouteName } from '../domain/routes.ts'

/// What the live region is saying. Empty between screens: a region holding the
/// same text as last time announces nothing when it is set again.
const said = ref('')
export const screenName = readonly(said)

export function useDocumentTitle(route: Ref<RouteName>, named: Ref<string | null | undefined>) {
  let clearing: ReturnType<typeof setTimeout> | undefined

  watch(
    [route, named],
    ([name, thing], previous) => {
      const title = documentTitle(name, thing)
      document.title = title
      // Only when the SCREEN changed. The name arrives a round trip after the
      // route does, so this fires twice per navigation — and announcing the
      // second one is announcing the same screen again, a beat later, over
      // whatever the reader had moved on to.
      if (previous && previous[0] === name) return
      said.value = title
      clearTimeout(clearing)
      // Cleared, so the next visit to the same screen is announced too: a live
      // region only speaks when its content CHANGES.
      clearing = setTimeout(() => (said.value = ''), 1000)
    },
    { immediate: true },
  )

  onScopeDispose(() => clearTimeout(clearing))
}
