/// One search box, two meanings, decided by where you are.
///
/// On the home screen it searches every library; on a library page it filters
/// that one. The TEXT lives in the address as `q` (see `useSearchAddress`), so
/// Back, a reload and a shared link all come back to the search or the
/// filtered page they left.
///
/// Debounced here rather than in each view. Two views debouncing the same text
/// would each have their own idea of when it settled, and the query would
/// change twice on every keystroke — typing hits the database, once per
/// library on the home screen.

import { inject, onScopeDispose, provide, readonly, ref, watch, type InjectionKey } from 'vue'
import {
  isNavigationFailure,
  NavigationFailureType,
  type RouteLocationNormalized,
  useRoute,
  useRouter,
} from 'vue-router'

import { hasSearchBox, hasSearchPanel, searchOf, type RouteName } from '../domain/routes.ts'

/// Long enough that a word is typed before the first request, short enough
/// that it does not feel like waiting.
export const DEBOUNCE_MS = 250

export function useSearch() {
  /// What is in the box, updated on every keystroke.
  const text = ref('')
  /// What the views actually search on, once the typing has settled.
  const query = ref('')
  /// Whether the results panel is showing.
  ///
  /// NOT derived from `text`: dismissing the panel has to be possible without
  /// clearing the box — a click elsewhere puts it away and leaves the text
  /// where it is — and focusing the box brings it back.
  const open = ref(false)

  let timer: ReturnType<typeof setTimeout> | undefined
  // A pending write after the owner is gone lands on a ref nobody reads, which
  // is harmless — but only until one of those writes is a fetch.
  onScopeDispose(() => clearTimeout(timer))
  watch(text, (value) => {
    clearTimeout(timer)
    timer = setTimeout(() => {
      query.value = value
    }, DEBOUNCE_MS)
  })

  /// Typing brings the panel back, so a dismissed box is not a dead one — and
  /// emptying the field puts it away, since there is nothing left to have
  /// results for. `panel` says whether this screen has one at all.
  function typed(value: string, panel: boolean) {
    text.value = value
    open.value = panel && value.trim() !== ''
  }

  /// Focus AND click, which are not the same event. Opening a library from the
  /// panel leaves focus in the box and navigates; coming back to the home
  /// screen, the box still holds the query but has never lost focus, so no
  /// focus event can fire again. The panel was then unreachable — clicks did
  /// nothing, the arrows do not exist while it is closed — and editing the
  /// text was the only way back to results already fetched.
  function reopen(panel: boolean) {
    open.value = panel && text.value.trim() !== ''
  }

  function dismiss() {
    open.value = false
  }

  /// The ✕, and the library heading: drop the filter on the screen you are on.
  function clear() {
    // No `clearTimeout` here, deliberately. Setting the text fires the watcher
    // above, which clears the pending timer itself — and it does so first,
    // because Vue flushes watchers on the microtask queue and a timer cannot
    // run until that has drained. A second clear looked load-bearing and was
    // not; removing it changed no test, which is how it was found.
    text.value = ''
    query.value = ''
    open.value = false
  }

  /// What an address says the box holds, taken as already settled: the page
  /// arriving with it is filtered from its first request, not a debounce
  /// later. `panel` says whether this screen has one, which an address with a
  /// search then opens: Back from a hit returns to the results it came from.
  function adopt(value: string, panel: boolean) {
    text.value = value
    query.value = value
    open.value = panel && value.trim() !== ''
  }

  /// Drops a keystroke that has not settled yet. Leaving a screen with one
  /// pending let it settle on the way out, and its address write then
  /// replaced the navigation that was leaving.
  function abandon() {
    clearTimeout(timer)
  }

  const search = {
    text,
    query: readonly(query),
    open: readonly(open),
    typed,
    reopen,
    dismiss,
    clear,
    adopt,
    abandon,
  }
  // The shell owns the box; the views read what it settled on. A slot prop
  // cannot cross a `<RouterView>` — the route component is not slot content —
  // and module-scope state would be shared by two mounted apps in one test
  // file. This is the seam a view asks through.
  provide(SEARCH, search)
  return search
}

export type Search = ReturnType<typeof useSearch>

const SEARCH = Symbol('search') as InjectionKey<Search>

/// For a view that filters on what the header holds. Outside the shell there
/// is no box, and a screen with no filter is a real state rather than a
/// mistake — so this answers with an empty query rather than throwing.
export function useSearchQuery() {
  return inject(SEARCH, undefined)?.query ?? readonly(ref(''))
}

/// The box itself, for a view that can clear it — the library heading drops
/// the filter, which is the other half of the ✕. `undefined` outside the
/// shell, where there is no box to clear.
export function useSearchBox(): Search | undefined {
  return inject(SEARCH, undefined)
}

/// The box and the address, kept in step. The ADDRESS leads.
///
/// Arriving anywhere — a link, a menu, Back, a reload — the box takes what the
/// destination's address carries (`searchOf`): `q` on the home, library and
/// artist screens, nothing everywhere else. So nothing that navigates clears
/// the box first. Clearing first would write the page being LEFT, dropping its
/// `q` from its own history entry, and Back would return to it without its
/// search. Opening an artist from a filtered library therefore starts that
/// artist with an empty box (its address has no `q`), and Back finds the
/// library still filtered; opening a hit from the home panel, Back finds the
/// panel open on it again.
///
/// Arriving also decides whether the panel is open, which is why the shell no
/// longer dismisses it on every address change: the home screen's own writes
/// change the address on every settled word, and would close the panel under
/// somebody still typing in it.
///
/// The other way, only what is done ON a screen writes its address: typing,
/// once it settles, and the ✕ or the heading. Always `replace`, so a word typed
/// is one history entry rather than one per keystroke.
export function useSearchAddress(search: Search) {
  const router = useRouter()
  const route = useRoute()
  const nameOf = (to: RouteLocationNormalized) => (to.name ?? 'libraries') as RouteName
  const carried = (to: RouteLocationNormalized) => searchOf(nameOf(to), to.query)
  const arrive = (to: RouteLocationNormalized) =>
    search.adopt(carried(to), hasSearchPanel(nameOf(to)))

  arrive(route)

  const offBefore = router.beforeEach(() => search.abandon())
  // In `afterEach`, which runs before the new screen renders, so a view never
  // sees the previous screen's filter — a library mounted with it would ask
  // the hub twice and scroll to the top over a restored position.
  const offAfter = router.afterEach((to, from, failure) => {
    // Asking for the address you are already at is still asking for it: the
    // brand mark on the home screen, a library's own menu entry.
    const again = isNavigationFailure(failure, NavigationFailureType.duplicated)
    if (failure && !again) return
    // On the same page, only a `q` the box does not already hold. The box's
    // own `replace` lands here too, possibly after more has been typed; taking
    // its value back would undo those keystrokes.
    if (again || to.path !== from.path || carried(to) !== search.query.value) arrive(to)
  })
  onScopeDispose(() => {
    offBefore()
    offAfter()
  })

  watch(search.query, (value) => {
    if (!hasSearchBox(nameOf(route)) || value === carried(route)) return
    void router.replace({ query: { ...route.query, q: value === '' ? undefined : value } })
  })
}
