/// Loading a code-split chunk, with the one failure a reload fixes.
///
/// The hub embeds `web/dist` in its own binary, so upgrading it replaces every
/// content hash while tabs are still open holding the old ones. Two chunks are
/// fetched lazily — the player and libass — and a tab that outlives an upgrade
/// asks for names the new binary does not have.
///
/// `React.lazy` caches a REJECTED promise for the life of the page, so this is
/// not a transient error: the boundary's Try again re-renders the same
/// rejection and the player can never open again in that tab. The dynamic
/// `import()` for libass behaves the same way, leaving subtitles broken with a
/// message that blames the file.
///
/// A missing chunk means the build this page came from is gone, so the page has
/// to go too. Once per chunk, and the mark is cleared the moment that chunk
/// loads. Both halves matter:
///
///   - PER CHUNK, because one mark shared between them is a reload loop. Take a
///     build where the player is present and libass is not: the player resolves
///     and clears the mark, libass 404s and sets it, the reload lands back on
///     `…/play` and autoplays, the player resolves and clears it again. Round
///     for ever.
///   - CLEARED ON SUCCESS, because `sessionStorage` outlives a reload — it is
///     scoped to the tab, not the page. A mark left set by the reload that
///     FIXED things means the next upgrade in that same tab gets no reload at
///     all, which is the original bug verbatim. Tabs here live for days.
///
/// Together, a mark that is still set can only mean "the reload did not help",
/// which is when the caller's own error handling should speak instead.
/// The chunks this page load inherited a mark for — set by the load that
/// reloaded, and consumed by this one.
///
/// Read once and cleared, so a mark cannot outlive the load it was written
/// for. It used to sit in `sessionStorage` until the chunk was asked for
/// again, and libass is asked for only when an ASS track is actually
/// selected: close the player before that happens and the mark stayed set for
/// the life of the tab, which is days. The next upgrade then found it, took
/// "the reload did not help" from it, and reported broken subtitles without
/// ever reloading — the original bug, restored by its own fix.
const KEY = 'kahawai.chunkReloaded'

const carried: Set<string> = (() => {
  try {
    const raw = sessionStorage.getItem(KEY)
    sessionStorage.removeItem(KEY)
    return new Set<string>(raw ? (JSON.parse(raw) as string[]) : [])
  } catch {
    return new Set<string>()
  }
})()

/// Marks written by THIS load, and the only thing handed to the next one.
const marked = new Set<string>()

function persist() {
  try {
    if (marked.size === 0) sessionStorage.removeItem(KEY)
    else sessionStorage.setItem(KEY, JSON.stringify([...marked]))
  } catch {
    // A tab with storage denied still loads chunks; it just cannot reload once.
  }
}

export function loadChunk<T>(key: string, load: () => Promise<T>): Promise<T> {
  return load().then(
    (mod) => {
      marked.delete(key)
      carried.delete(key)
      persist()
      return mod
    },
    (e: unknown) => {
      if (carried.has(key) || marked.has(key)) throw e
      marked.add(key)
      persist()
      location.reload()
      // Never settles: the reload takes the page before anything can render.
      return new Promise<T>(() => {})
    },
  )
}
