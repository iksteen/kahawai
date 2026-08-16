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

import { computed, onScopeDispose, readonly, type Ref, ref, watch } from 'vue'

import { awaitsName, documentTitle, type Screen } from '../domain/titles.ts'

/// What the live region is saying. Empty between screens: a region holding the
/// same text as last time announces nothing when it is set again.
const said = ref('')
export const screenName = readonly(said)

/// What the current screen is showing, published by the view that knows. The
/// route says `detail`; only the detail view ever learns the item's title, and
/// it learns it a round trip late.
///
/// Tagged with WHICH screen it describes, and that tag is the whole point. The
/// route changes before the outgoing view is torn down — a `pre`-flush watcher
/// runs ahead of the component update that unmounts it — so for one tick the
/// new screen is paired with the old screen's name. Untagged, that pairing is
/// indistinguishable from the real one: arriving on an item announced the
/// LIBRARY's name, and the item's own title, landing a beat later, was then
/// swallowed as a repeat of a screen already announced.
export type Showing = { screen: Screen; name: string }
const showing = ref<Showing | null>(null)
/// Exported WITH its tag. A name-only view of this would hide the one thing
/// worth checking: the tag is a hand-typed literal repeated in four views, and
/// a view that publishes under the wrong screen never titles itself and is
/// never announced — while every assertion about "the name it published"
/// carries on passing.
export const screenShowing = computed<Showing | null>(() => showing.value)

/// A view naming its own screen. The screen is passed rather than read from the
/// route so that the tag is the view's own answer, not whatever the router has
/// moved on to.
///
/// The clear on teardown only takes back what THIS screen put there.
export function useScreenName(screen: Screen, source: Ref<string | null | undefined>) {
  watch(
    source,
    (name) => {
      const text = name?.trim() || null
      showing.value = text ? { screen, name: text } : null
    },
    { immediate: true },
  )
  onScopeDispose(() => {
    if (showing.value?.screen === screen) showing.value = null
  })
}

/// The document's title, from the screen and whatever that screen published.
///
/// It reads the publication itself rather than taking a name, so there is one
/// place where a screen and a name are paired and one place that can get it
/// wrong.
export function useDocumentTitle(screen: Ref<Screen>) {
  let clearing: ReturnType<typeof setTimeout> | undefined
  /// The screen whose arrival has not been announced yet, or null once it has.
  /// This fires twice per navigation — once for the route, once for the name —
  /// and announcing both says the same screen twice, a beat apart, over
  /// whatever the reader had moved on to.
  let unannounced: Screen | null = null

  watch(
    [screen, showing],
    ([where, published], previous) => {
      // A name belongs to the screen that published it, and to no other.
      const thing = published?.screen === where ? published.name : null
      const title = documentTitle(where, thing)
      document.title = title
      if (!previous || previous[0] !== where) {
        unannounced = where
        // Emptied on ARRIVAL, not only by the timer below. The next screen can
        // be announced with the same words as this one — pressing Play on an
        // item page is exactly that — and a region already holding them says
        // nothing at all when they are set again. Pressing Play within a
        // second of the item page loading announced neither screen.
        clearTimeout(clearing)
        said.value = ''
      }
      if (unannounced !== where) return
      // Nothing worth saying yet. On a screen titled by what it is showing,
      // announcing before the name lands announces the word "kahawai" — and
      // then the real title arrives under the guard above and is never said.
      if (awaitsName(where) && !thing) return
      unannounced = null
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
