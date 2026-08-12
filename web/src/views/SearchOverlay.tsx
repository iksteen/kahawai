import { useEffect, useMemo, useState, type RefObject } from 'react'
import { artworkUrl, fetchItems, type Item, type LibrarySummary } from '../api'
import Icon from '../icons'
import { metaLine, targetOf } from '../label'
import {
  countLabel,
  moveHighlight,
  SEARCH_LIST_ID,
  searchOptionId,
  searchRows,
  type LibraryHits,
} from '../search-nav'
import { notify } from '../toast'

/// How many hits each library contributes to the panel.
///
/// One request per library rather than one for everything, because the items
/// endpoint does not say which library a row came from, and "at most five each"
/// is not something a single LIMIT can express. They run concurrently and each
/// is a bounded query, so it costs one round trip.
const PER_LIBRARY = 5

/// The search results, over whatever you were already looking at.
///
/// This used to be a page: typing on the home screen replaced it outright,
/// shelves and continue-watching included, so a search you did not mean cost
/// you your place and the home screen reloaded when you cleared it. The panel
/// leaves the screen alone — which is what the design had, and what makes
/// dismissing it free.
///
/// Home only, deliberately. On a library page the same box filters that library
/// in place, and a dropdown of cross-library hits over a page that is already
/// filtering would be two answers to one question.
export default function SearchOverlay({
  open,
  query,
  libs,
  inputRef,
  onNav,
  onOpenLibrary,
  onOpenItem,
  onClose,
}: {
  /// Whether the panel is showing. Mounted either way, so dismissing it keeps
  /// the results it already has: unmounting threw them away, and focusing the
  /// box again re-ran every library's search and showed nothing for a round
  /// trip.
  open: boolean
  query: string
  /// The shell's own list, so the panel does not fetch a second copy. EMPTY
  /// while it is still coming, and empty for good if that request failed — the
  /// shell swallows its failure on purpose, since the only other thing it costs
  /// is the jump menu's entries.
  ///
  /// Which is why the guard below is on the length and not on the value. An
  /// empty list searched nothing, `Promise.all([])` answers immediately, and
  /// the panel would state "No matches" as a fact about the catalogue — from a
  /// search that never ran, for the rest of the session, with no Try again
  /// because nothing failed from its point of view. That is the exact thing the
  /// per-library failure reporting exists to prevent.
  libs: LibrarySummary[]
  /// The search box. The panel listens for keys on it rather than on the
  /// window: the keyboard only reaches the panel through the field it belongs
  /// to, so scoping it there means no priority question against the menus, the
  /// dialogs or the player's own Escape.
  inputRef: RefObject<HTMLInputElement | null>
  /// Reports up exactly what the input has to announce: whether the panel is on
  /// screen at all, and which row is lit. The panel keeps the authoritative copy
  /// — it owns the rows — and this is a mirror, so it must be a stable function
  /// or the effect behind it never settles.
  ///
  /// `shown` is not `open && count > 0`: a query that matches nothing, or a hub
  /// that failed every library, puts a visible panel on screen with no rows in
  /// it. Deriving it from the row count told a screen reader the combobox was
  /// collapsed while "No matches" or the retry button was showing — the exact
  /// two states someone would most need read out.
  onNav: (state: { shown: boolean; highlight: number }) => void
  /// Both close the panel. Only the item clears the query, and that is the
  /// caller's job because only it owns the box: a library keeps the text,
  /// where it becomes that library's filter.
  onOpenLibrary: (id: string) => void
  onOpenItem: (target: string, libraryId: string) => void
  onClose: () => void
}) {
  const [hits, setHits] = useState<LibraryHits[] | null>(null)
  /// The query that produced the rows still on screen. Rows remain visible and
  /// actionable while their replacements load; this keeps their label honest.
  const [shownQuery, setShownQuery] = useState('')
  const [searching, setSearching] = useState(false)
  /// The libraries that could not be asked, by name. Only reporting the
  /// all-failed case still stated a fact it did not have: two libraries
  /// erroring beside one with no matches printed "nothing matches" over a count
  /// of one, and the only mention of the two was a toast gone in five seconds.
  const [failed, setFailed] = useState<string[]>([])
  const [attempt, setAttempt] = useState(0)
  /// The lit row, `-1` for none, which is where every new set of results
  /// starts. Not carried across a query: a position kept in a list that has
  /// been replaced points at a different film, and Enter would open it.
  const [highlight, setHighlight] = useState(-1)

  useEffect(() => {
    if (libs.length === 0 || !query) {
      setHits(null)
      setShownQuery('')
      setSearching(false)
      setHighlight(-1)
      setFailed([])
      return
    }
    // Keep the old rows in place while asking. They are visible, so pressing
    // Enter on their highlight remains predictable; blanking the panel here
    // makes every debounced keystroke flash the whole surface away instead.
    setSearching(true)
    // Cleared before asking, so a failure does not paint over the next
    // keystroke's results for a round trip.
    setFailed([])
    let stale = false
    Promise.all(
      libs.map((library) =>
        fetchItems({ library: library.id, q: query, limit: PER_LIBRARY })
          .then((r) => ({ library, items: r.items, total: r.total, failure: '' }))
          // "No matches here" and "we could not ask" are different answers, and
          // an empty list gives the first when it means the second. Marked
          // rather than announced: notices are latest-wins, so one per library
          // would name whichever failed last and imply the rest were fine.
          .catch((e: unknown) => ({
            library,
            items: [] as Item[],
            total: 0,
            failure: String(e),
          })),
      ),
    ).then((all) => {
      // Answers can arrive after the query has moved on; only the newest set
      // may paint.
      if (stale) return
      setHits(all.filter((h) => h.items.length > 0))
      setShownQuery(query)
      setSearching(false)
      // Beside the rows it belongs to rather than at the top of this effect:
      // the old results stay on screen until these land, and a Down pressed
      // while the new query was in flight is a Down against what was showing.
      setHighlight(-1)
      setFailed(all.filter((h) => h.failure !== '').map((h) => h.library.name))
      // One notice for the search, once every library has answered, so it can
      // say whether this was a bad connection or one bad library.
      const broke = all.filter((h) => h.failure !== '')
      if (broke.length === all.length && broke.length > 0) {
        notify(`Could not search — ${broke[0].failure}`)
      } else if (broke.length > 0) {
        notify(`Could not search ${broke.map((h) => h.library.name).join(', ')}.`)
      }
    })
    return () => {
      stale = true
    }
  }, [libs, query, attempt])

  // Tied to `hits` so it changes when the results do and not on every render of
  // the header above — which is every keystroke, since the query lives up there.
  // Nothing depends on this identity for correctness: the effect that reports
  // upward takes primitives, and the keydown listener resubscribes on most
  // renders anyway because the callbacks it closes over are inline lambdas from
  // the header. That is a detach and reattach inside one flush, with no window
  // for an event to fall through.
  const rows = useMemo(() => (hits ? searchRows(hits) : []), [hits])

  /// The keyboard, and leaving.
  ///
  /// Scoped to the search area — the box, its ✕, and the panel itself, which is
  /// rendered inside the same wrapper — rather than to the window: the keyboard
  /// only reaches this panel through that area, so there is no priority question
  /// against the menus, the dialogs or the player's own Escape. And only while
  /// the panel is open, so a closed box keeps its own keys exactly as they were.
  ///
  /// The area rather than the input alone because focus can legitimately be on
  /// the ✕ or on Try again with the panel still up, and Escape has to work from
  /// there too. If the wrapper ever goes missing this falls back to the input,
  /// which is the smaller version of the same behaviour rather than none of it.
  useEffect(() => {
    const box = inputRef.current
    if (!box || !open) return
    const area = box.closest<HTMLElement>('.search') ?? box
    const onKey = (e: KeyboardEvent) => {
      // A composition owns these keys first. Typing Japanese, the arrows walk
      // the IME's candidate list and Enter commits the word — take them and
      // choosing a character navigates into a library instead.
      if (e.isComposing) return
      if (e.key === 'Escape') {
        // Out of the field as well as out of the panel: a dropdown dismissed
        // while the caret is still blinking in the box that opened it reads as
        // a box that stopped working. Whatever holds focus lets go, because from
        // the retry button the alternative is focus on an element that is about
        // to be unmounted. `preventDefault` because Escape in a search field
        // reverts its value in some browsers, and losing the query was not what
        // was asked for.
        e.preventDefault()
        onClose()
        ;(document.activeElement as HTMLElement | null)?.blur()
        return
      }
      // Walking and opening belong to the field. Anywhere else in here — the ✕,
      // Try again — the keys are that control's own, and Enter must press the
      // button rather than open a library.
      if (e.target !== box || rows.length === 0) return
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        // Or the caret jumps to one end of the query while the highlight moves.
        e.preventDefault()
        setHighlight((h) => moveHighlight(rows.length, h, e.key === 'ArrowDown' ? 1 : -1))
        return
      }
      if (e.key === 'Enter') {
        e.preventDefault()
        // Nothing highlighted falls to `rows[0]`, which `searchRows` guarantees
        // is a library heading — so Enter straight after typing shows
        // everything the first library matched, rather than guessing at one
        // film out of it.
        const row = rows[highlight] ?? rows[0]
        if (row.kind === 'library') onOpenLibrary(row.library.id)
        else onOpenItem(targetOf(row.item), row.library.id)
      }
    }
    /// Tab out of the area and the panel goes with you. This replaces closing on
    /// the Tab key itself, which could not tell "leaving" from "reaching for the
    /// retry button inside the panel" and so made that button mouse-only.
    ///
    /// Only when focus actually landed on something outside: a `relatedTarget`
    /// of null means focus went nowhere, which is also what a mousedown on a row
    /// looks like in browsers that do not focus buttons on click — closing there
    /// would unmount the row before its click could fire.
    const onFocusOut = (e: FocusEvent) => {
      const to = e.relatedTarget as Node | null
      if (to && !area.contains(to)) onClose()
    }
    area.addEventListener('keydown', onKey)
    area.addEventListener('focusout', onFocusOut)
    return () => {
      area.removeEventListener('keydown', onKey)
      area.removeEventListener('focusout', onFocusOut)
    }
  }, [open, rows, highlight, inputRef, onClose, onOpenLibrary, onOpenItem])

  // The panel scrolls at 70vh, so the highlight can walk out of sight.
  // `nearest` rather than `center`: it moves things only as far as it must,
  // which on a long panel whose bottom is past the fold means the page comes
  // along — the alternative is a lit row nobody can see.
  useEffect(() => {
    if (highlight < 0) return
    document.getElementById(searchOptionId(highlight))?.scrollIntoView({ block: 'nearest' })
  }, [highlight])

  /// Dismissing abandons the walk. Here rather than in the Escape branch
  /// because there are four ways out — Escape, focus leaving the search area, a
  /// click on the sheet, and opening something — and only this one covers them
  /// all. Without it the
  /// panel came back with the old row still lit: dismissed at the eighth hit,
  /// refocused, and Enter opened that hit instead of the first library, which
  /// is what "nothing highlighted" is supposed to mean.
  useEffect(() => {
    if (!open) setHighlight(-1)
  }, [open])

  // The same condition as the early return below, which is the point: the
  // input's `aria-expanded` has to mean "there is a panel on screen", and the
  // only thing that knows is whatever decides to render one.
  const shown = open && !!query && hits !== null

  // A closed panel offers the keyboard nothing, whatever it still holds.
  useEffect(() => {
    onNav({ shown, highlight: shown ? highlight : -1 })
  }, [shown, highlight, onNav])

  // And nothing at all once it is gone. Without this, leaving home mid-walk
  // left the input pointing `aria-activedescendant` at a row id that no longer
  // existed until a fresh panel got round to correcting it. Unmount only —
  // putting the reset in the effect above would have it report "closed" between
  // every two arrow presses.
  useEffect(() => () => onNav({ shown: false, highlight: -1 }), [onNav])

  // Nothing to say yet: no text, or the first answer has not arrived. An empty
  // panel that appears on the first keystroke and then fills is worse than one
  // that appears once it has something — the layout jumps and the mouse is
  // already moving.
  if (!shown) return null

  return (
    <>
      {/* Same shape as the menus: a click anywhere else lands here rather than
          on whatever it was over. */}
      <div className="menu-sheet" onClick={onClose} />
      <div className="search-panel">
        {searching && hits !== null && (
          <span className="search-update mono" role="status">
            updating
          </span>
        )}
        {/* Outside the listbox below, because a listbox may contain nothing but
            options: a paragraph and a button inside one can be dropped from the
            accessibility tree altogether, which would have silently hidden the
            only two things in here worth reading out. `status` in the hope of an
            announcement — a live region that arrives together with its text is
            not reliably read, so the failure case does not lean on it: the
            search also raises a notice, and that region is always mounted. */}
        {rows.length === 0 && failed.length === 0 && (
          <p className="search-empty dim" role="status">
            No matches for “{query}”.
          </p>
        )}
        {failed.length > 0 && (
          <p className="search-failed error" role="status">
            {failed.length === libs.length
              ? 'Could not search — the hub did not answer.'
              : `Could not search ${failed.join(', ')}, so these results are incomplete.`}{' '}
            <button
              className="linklike"
              // Focus first, because pressing this destroys it: clearing `failed`
              // unmounts the button, and focus would land on the document body
              // with the panel still open and its keys dead. The box is where it
              // came from and where the answer will arrive.
              onClick={() => {
                inputRef.current?.focus()
                setAttempt((n) => n + 1)
              }}
            >
              Try again
            </button>
          </p>
        )}
        {/* The list the input's `aria-controls` names, holding options and
            nothing else. Focus stays in the box and the lit row is named by
            `aria-activedescendant`, which is the whole reason the rows carry
            ids — and why they are not tab stops. */}
        <div id={SEARCH_LIST_ID} role="listbox" aria-label={`Results for ${shownQuery || query}`}>
          {rows.map((row, i) =>
            row.kind === 'library' ? (
              <button
                key={`lib:${row.library.id}`}
                id={searchOptionId(i)}
                role="option"
                aria-selected={i === highlight}
                // Buttons, so the mouse gets a real control — but out of the
                // tab order, because the arrows are how this list is walked and
                // Tab is how you leave it. Nine rows of tab stops between the
                // search box and the rest of the header is not navigation.
                tabIndex={-1}
                className={`search-lib${i === highlight ? ' search-row-on' : ''}`}
                title={`Show everything in ${row.library.name}`}
                onClick={() => onOpenLibrary(row.library.id)}
              >
                <span className="search-lib-name">{row.library.name}</span>
                <span className="count mono">{countLabel(row.shown, row.total)}</span>
                <span className="search-lib-go">
                  <Icon name="next" size={13} />
                </span>
              </button>
            ) : (
              <button
                // The library id belongs in the key: membership is many-to-many,
                // so one item can appear under two libraries and its id alone is
                // not unique in this list.
                key={`${row.library.id}:${row.item.id}`}
                id={searchOptionId(i)}
                role="option"
                aria-selected={i === highlight}
                tabIndex={-1}
                className={`search-hit${i === highlight ? ' search-row-on' : ''}`}
                onClick={() => onOpenItem(targetOf(row.item), row.library.id)}
              >
                <img
                  className="result-art"
                  src={artworkUrl(row.item.id, row.item.art_version, 'thumb')}
                  // Not `loading="lazy"`, which it inherited from the page it
                  // replaced: in here those images never loaded at all. Every
                  // row sat with an empty 34px box while the same URL fetched
                  // 200 and rendered the moment the attribute came off. The
                  // shelves keep it because they are a long page of posters
                  // below the fold; a dropdown is at most fifteen thumbnails
                  // that are all on screen the instant it opens, so deferring
                  // them was never buying anything to begin with.
                  alt=""
                />
                <span className="search-hit-text">
                  <span className="result-title">{row.item.title}</span>
                  <span className="result-meta mono">{metaLine(row.item)}</span>
                </span>
              </button>
            ),
          )}
        </div>
      </div>
    </>
  )
}
