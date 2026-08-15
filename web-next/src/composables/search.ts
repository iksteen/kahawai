/// One search box, two meanings, decided by where you are.
///
/// On the home screen it searches every library; on a library page it filters
/// that one. The TEXT is shared, which is what lets a result lead into its
/// library with the query still standing.
///
/// Debounced here rather than in each view. Two views debouncing the same text
/// would each have their own idea of when it settled, and the query would
/// change twice on every keystroke — typing hits the database, once per
/// library on the home screen.

import { inject, onScopeDispose, provide, readonly, ref, watch, type InjectionKey } from 'vue'

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

  /// Going home is a fresh start: a standing filter that silently follows you
  /// there reads as missing items.
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

  /// Opening one result: you asked for this one thing and got it, so the text
  /// goes with the navigation. Cleared in the same handler so it cannot be
  /// forgotten.
  function taken() {
    clear()
  }

  const search = {
    text,
    query: readonly(query),
    open: readonly(open),
    typed,
    reopen,
    dismiss,
    clear,
    taken,
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
