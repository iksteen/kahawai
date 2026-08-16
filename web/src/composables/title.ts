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
import { useRoute } from 'vue-router'

import { addressOf } from '../domain/routes.ts'
import { awaitsName, documentTitle, type Screen } from '../domain/titles.ts'

/// What the live region is saying. Empty between screens: a region holding the
/// same text as last time announces nothing when it is set again.
const said = ref('')
export const screenName = readonly(said)

/// What the current screen is showing, published by the view that knows. The
/// route says `detail`; only the detail view ever learns the item's title, and
/// it learns it a round trip late.
///
/// Tagged with the ADDRESS it describes, and that tag is the whole point. The
/// route changes before the outgoing view is torn down — a `pre`-flush watcher
/// runs ahead of the component update that unmounts it — so for one tick the
/// new address is paired with the old one's name. Untagged, that pairing is
/// indistinguishable from the real one: arriving on an item announced the
/// LIBRARY's name, and the item's own title, landing a beat later, was then
/// swallowed as a repeat of a screen already announced. Tagged by SCREEN it
/// was still wrong for an item and the next item, which are one screen and two
/// addresses.
export type Showing = { at: string; name: string }
const showing = ref<Showing | null>(null)
/// Exported WITH its tag, because the tag is the thing worth checking: a name
/// published against the wrong address never reaches the title and is never
/// announced, while every assertion about "the name it published" goes on
/// passing.
export const screenShowing = computed<Showing | null>(() => showing.value)

/// A view naming the screen it is on.
///
/// The address is read at PUBLISH time, not at setup: these components are
/// reused across a change of item, and the name being published is always the
/// name of whatever the route says now — a view showing stale data has not
/// published anything new, so its old publication keeps its old address.
///
/// The clear on teardown only takes back what this view actually put there.
export function useScreenName(source: Ref<string | null | undefined>) {
  const route = useRoute()
  let mine: string | null = null
  watch(
    source,
    (name) => {
      const text = name?.trim() || null
      mine = text ? addressOf(route) : null
      showing.value = text && mine ? { at: mine, name: text } : null
    },
    { immediate: true },
  )
  onScopeDispose(() => {
    if (mine !== null && showing.value?.at === mine) showing.value = null
  })
}

/// The document's title, from the screen and whatever that screen published.
///
/// It reads the publication itself rather than taking a name, so there is one
/// place where a screen and a name are paired and one place that can get it
/// wrong.
/// `arrival` is what counts as having GONE somewhere, which the screen alone
/// cannot say: an item and the next item are both `detail`, and pressing an
/// episode announced nothing. The caller passes the error boundary's key,
/// because it draws the same line for the same reason — the player's autoplay
/// handover changes the URL and is not somewhere the viewer went.
export function useDocumentTitle(screen: Ref<Screen>, arrival: Ref<string>) {
  let clearing: ReturnType<typeof setTimeout> | undefined
  /// Where the viewer has arrived without being told so, or null once they
  /// have been. This fires twice per navigation — once for the route, once for
  /// the name — and announcing both says the same screen twice, a beat apart,
  /// over whatever the reader had moved on to.
  let unannounced: string | null = null

  watch(
    [screen, arrival, showing],
    ([where, at, published], previous) => {
      // A name belongs to the address it was published for, and to no other.
      const thing = published?.at === at ? published.name : null
      const title = documentTitle(where, thing)
      document.title = title
      const here = `${where} ${at}`
      if (!previous || `${previous[0]} ${previous[1]}` !== here) {
        unannounced = here
        // Emptied on ARRIVAL, not only by the timer below. The next screen can
        // be announced with the same words as this one — pressing Play on an
        // item page is exactly that — and a region already holding them says
        // nothing at all when they are set again. Pressing Play within a
        // second of the item page loading announced neither screen.
        clearTimeout(clearing)
        said.value = ''
      }
      if (unannounced !== here) return
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
