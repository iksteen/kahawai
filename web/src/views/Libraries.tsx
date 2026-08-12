import { useEffect, useRef, useState } from 'react'
import { notify } from '../toast'
import Failed from '../Failed'
import {
  artworkSrcSet,
  artworkUrl,
  fetchItems,
  fetchLibraries,
  isAdmin,
  type Item,
  type LibrarySummary,
} from '../api'
import Icon, { type IconName } from '../icons'
import Lane from '../Lane'
import { metaLine, targetOf, watchedPct } from '../label'

/// A shelf's first page, and how much it fetches each time it is scrolled
/// near its end. Small enough that opening the home screen is seven cheap
/// queries, big enough that a shelf does not grow a card at a time.
const PER_SHELF = 20

/// How many things you can be in the middle of before the row stops
/// helping. Past this it is a list of abandoned evenings, and it pushes
/// the shelves off the screen.
const CONTINUING = 12

/// One card's width in a shelf, and how much of a shelf a press of an
/// arrow moves. Kept here rather than measured: a shelf scrolls by
/// whole cards, so the number that decides the step is the same one the
/// CSS lays out with.
const SHELF_CARD_PX = 150
const SHELF_STEP = SHELF_CARD_PX * 3

type Shelf = {
  library: LibrarySummary
  items: Item[]
  total: number
  /// A shelf whose fetch failed. Distinct from an empty one: an empty
  /// library genuinely has no shelf, and conflating the two deleted whole
  /// libraries from the home screen without a word.
  failed?: boolean
  /// Still on its way. Every library gets one of these the moment the list
  /// of libraries is known, so the page has its shape before any content
  /// arrives — on a slow link that was several seconds of blank.
  pending?: boolean
}

/// A lane of card-shaped ghosts, the same ones the library grid shows for
/// rows that have not arrived. Used while a failed shelf is being asked for
/// again, so the row keeps its height and its place instead of the page
/// jumping as it succeeds.
function PendingLane() {
  return (
    <div className="shelf-lane" aria-hidden="true">
      {Array.from({ length: 8 }, (_, i) => (
        <div className="shelf-card card-pending" key={i}>
          <span className="card-artbox">
            <span className="card-art" />
          </span>
          <span className="shelf-title">&nbsp;</span>
          <span className="shelf-meta mono">&nbsp;</span>
        </div>
      ))}
    </div>
  )
}

function kindGlyph(kind: string): IconName | null {
  if (kind === 'movie') return 'movie'
  if (kind === 'show' || kind === 'episode') return 'show'
  if (kind === 'album' || kind === 'track') return 'album'
  return null
}

/// Artwork, with the kahawai swell on the box behind it — so a poster
/// that is slow shows the swell rather than a white flash, and one that
/// never arrives simply keeps it.
///
/// `progress` draws the resume bar across the bottom of the art. A shelf
/// card wants it there — the art is all it has. A continue-watching card
/// does not: its whole text column ends in a progress bar, and drawing it
/// twice on the same card says it twice.
function Art({
  item,
  size,
  className,
  progress = true,
  posterOf,
}: {
  item: Item
  size: 'thumb' | 'card'
  className: string
  progress?: boolean
  /// Show somebody else's artwork: an episode's own is a landscape still,
  /// and in a row of portrait posters it is the one thing that does not
  /// belong. Its show's poster is the same shape as everything beside it.
  ///
  /// No `art_version` travels with it — that number describes THIS item's
  /// artwork, and pinning the parent's URL with the child's version would
  /// be a cache key that lies. The cost is that a re-matched show keeps its
  /// old poster here until the browser's copy expires.
  posterOf?: string | null
}) {
  const glyph = kindGlyph(item.kind)
  const done = progress ? watchedPct(item) : null
  const artId = posterOf ?? item.id
  const artVersion = posterOf ? undefined : item.art_version
  return (
    <span className="card-artbox">
      <img
        className={className}
        src={artworkUrl(artId, artVersion, size)}
        // Only the card sizes come in a pair; a thumb is already smaller
        // than any display it lands on.
        srcSet={size === 'card' ? artworkSrcSet(artId, artVersion) : undefined}
        loading="lazy"
        alt=""
        // A poster that will not load is hidden, revealing the swell on
        // the box behind it. Hidden rather than emptied: an <img> with no
        // source still gets the browser's own broken-artwork mark.
        onError={(e) => e.currentTarget.classList.add('art-failed')}
      />
      {glyph && (
        <span className="kind-badge" title={item.kind === 'show' ? 'series' : item.kind}>
          <Icon name={glyph} />
        </span>
      )}
      {item.played && (
        <span className="seen-badge" title="seen">
          <Icon name="check" />
        </span>
      )}
      {done !== null && !item.played && (
        <span className="card-progress">
          <span className="card-progress-fill" style={{ width: `${done}%` }} />
        </span>
      )}
    </span>
  )
}

/// A shelf: what arrived lately in one library. `Lane` owns the sideways
/// scrolling and its arrows.
function Shelf({
  shelf,
  onOpen,
  onOpenItem,
  onRetry,
}: {
  shelf: Shelf
  onOpen: (id: string) => void
  onOpenItem: (id: string, fromLib: string) => void
  /// Ask for this one library again. Resolves to false if it failed again,
  /// so the row can go back to offering the button rather than spinning.
  onRetry: () => Promise<boolean>
}) {
  // The shelf owns its items past the first page: each one grows on its
  // own as it is scrolled, and none of them is the home screen's business.
  const [items, setItems] = useState(shelf.items)
  const busy = useRef(false)
  const [retrying, setRetrying] = useState(false)

  const more = () => {
    if (busy.current || items.length >= shelf.total) return
    busy.current = true
    fetchItems({
      library: shelf.library.id,
      sort: '-added',
      limit: PER_SHELF,
      offset: items.length,
    })
      .then((r) =>
        setItems((prev) => {
          // Ids, not lengths: a rescan between two pages can shift what
          // sits at an offset, and appending a duplicate would give React
          // two children with one key.
          const seen = new Set(prev.map((i) => i.id))
          return [...prev, ...r.items.filter((i) => !seen.has(i.id))]
        }),
      )
      // A lane that stops growing is indistinguishable from a lane that has
      // reached the end of its library, so silence here is a lie by omission.
      .catch(() => notify(`Could not load more from ${shelf.library.name}.`))
      .finally(() => {
        busy.current = false
      })
  }

  return (
    // A sleeve is square and a poster is two by three. Passed as a custom
    // property so the stylesheet still owns the rule and this owns only
    // the value.
    <section
      className="shelf"
      style={
        {
          '--card-ratio': shelf.library.media_type === 'music' ? '1' : '2 / 3',
        } as React.CSSProperties
      }
    >
      <div className="shelf-head">
        <button className="shelf-name" onClick={() => onOpen(shelf.library.id)}>
          {shelf.library.name} <span className="shelf-arrow">→</span>
        </button>
        <span className="shelf-note mono">latest added</span>
        {!shelf.pending && (
          <span className="shelf-note mono dimmer">
            {items.length} of {shelf.total}
          </span>
        )}
      </div>
      {shelf.pending ? (
        <PendingLane />
      ) : shelf.failed ? (
        retrying ? (
          // Its height and its place, held, so the page does not jump when
          // the answer arrives.
          <PendingLane />
        ) : (
          <div className="shelf-failed">
            <span className="dim">This one would not load.</span>
            <button
              className="btn ghost small"
              onClick={async () => {
                setRetrying(true)
                // Back to the button on a second failure: a row that
                // silently keeps ghosting is the vanishing shelf again in a
                // different costume.
                if (!(await onRetry())) setRetrying(false)
              }}
            >
              Try again
            </button>
          </div>
        )
      ) : (
        <Lane step={SHELF_STEP} onNearEnd={more}>
          {items.map((i) => (
            <button
              key={i.id}
              className="shelf-card"
              onClick={() => onOpenItem(targetOf(i), shelf.library.id)}
            >
              <Art item={i} size="card" className="card-art" />
              <span className="shelf-title">{i.title}</span>
              <span className="shelf-meta mono">{metaLine(i)}</span>
            </button>
          ))}
        </Lane>
      )}
    </section>
  )
}

export default function Libraries({
  onOpen,
  onOpenItem,
}: {
  onOpen: (id: string) => void
  onOpenItem: (id: string, fromLib: string) => void
}) {
  const [libs, setLibs] = useState<LibrarySummary[] | null>(null)
  const [shelves, setShelves] = useState<Shelf[] | null>(null)

  /// Ask for one library again and put it back in place. Only that shelf is
  /// touched — the others are fine and re-fetching them would throw away
  /// pages the viewer has already scrolled into.
  const retryShelf = async (library: LibrarySummary) => {
    try {
      const r = await fetchItems({ library: library.id, sort: '-added', limit: PER_SHELF })
      setShelves((prev) =>
        (prev ?? []).map((s) =>
          s.library.id === library.id
            ? { library, items: r.items, total: r.total, failed: false }
            : s,
        ),
      )
      return true
    } catch {
      return false
    }
  }
  const [continuing, setContinuing] = useState<Item[]>([])
  const [error, setError] = useState('')
  /// Bumped by Try again. No way `away` from here — this IS home.
  const [attempt, setAttempt] = useState(0)

  useEffect(() => {
    // Fenced, because the error and the button now stay on screen for the whole
    // request — so a second Try again is possible, where clearing the error used
    // to unmount the button and hide the race. Two loads in flight and the older
    // one rejecting last put the Failed screen back over libraries that had
    // arrived, which is the state this whole change exists to prevent.
    let live = true
    fetchLibraries()
      .then((r) => {
        if (!live) return
        setError('')
        setLibs(r.libraries)
      })
      .catch((e) => live && setError(String(e)))
    return () => {
      live = false
    }
  }, [attempt])

  // The home screen proper: what you are part-way through, then what arrived
  // lately in each library. No longer skipped while searching: the results are
  // a panel over this screen now, so it stays where it is — which is the whole
  // point of the change, and what makes dismissing the panel free.
  useEffect(() => {
    if (!libs) return
    let stale = false
    // Cross-library and in one request, because recency only means
    // anything across the whole set: per-library calls would each be
    // ordered correctly and could not be merged, since the timestamp
    // they were ordered by is not in the response.
    fetchItems({ in_progress: true, limit: CONTINUING })
      .then((r) => {
        if (stale) return
        // An item in no library has nowhere for its page to live: the
        // URL is /library/{id}/item/{id}. Only an unrestricted account
        // can see one at all.
        setContinuing(r.items.filter((i) => i.library_id))
      })
      // Same lie, louder: no row at all reads as "you have nothing on the go".
      .catch(() => notify('Could not load what you were watching.'))
    // One at a time, in place, rather than one `Promise.all` that shows
    // nothing until the slowest library answers. Every shelf is on screen as
    // a skeleton immediately and fills itself in; on a slow connection that
    // is the difference between a page and a blank.
    setShelves(libs.map((library) => ({ library, items: [], total: 0, pending: true })))
    const settle = (id: string, patch: Partial<Shelf>) =>
      setShelves((prev) =>
        (prev ?? []).map((s) => (s.library.id === id ? { ...s, ...patch, pending: false } : s)),
      )
    for (const library of libs) {
      fetchItems({ library: library.id, sort: '-added', limit: PER_SHELF })
        .then((r) => {
          if (!stale) settle(library.id, { items: r.items, total: r.total })
        })
        // Marked, not emptied. A failed fetch used to become an empty shelf,
        // and empty shelves are dropped — so a library that would not load
        // disappeared from the home screen entirely, with nothing said. The
        // one case that must not look like the other.
        .catch(() => {
          if (!stale) settle(library.id, { failed: true })
        })
    }
    return () => {
      stale = true
    }
  }, [libs])

  if (error)
    return (
      <Failed
        what="Could not load your libraries."
        message={error}
        // The error stays up while the retry is out. Clearing it here fell
        // through to `if (!libs) return null` — a blank page for the length of
        // the request, with the error reappearing after it, which reads as the
        // button having done something wrong rather than nothing yet.
        onRetry={() => setAttempt((n) => n + 1)}
      />
    )
  if (!libs) return null

  if (libs.length === 0) {
    return (
      <main>
        <div className="library-head">
          <h1>Libraries</h1>
        </div>
        <p className="dim">
          {/* An account with no grants is not an operator: `list_libraries`
              answers an empty list rather than a 403, so the day-one
              experience of a user nobody has granted anything was an
              instruction to install server software. */}
          {isAdmin()
            ? 'No libraries yet. Connect a mediahost — each collection it announces becomes a library here.'
            : 'Nothing here yet. Ask whoever runs this hub to give your account access to a library.'}
        </p>
      </main>
    )
  }

  return (
    <main>
      {continuing.length > 0 && (
        <>
          <h2 className="home-head">Continue watching</h2>
          <div className="continuing">
            {continuing.map((i) => (
              <button
                key={i.id}
                className="continue-card"
                onClick={() => onOpenItem(targetOf(i), i.library_id as string)}
              >
                <Art
                  item={i}
                  size="card"
                  className="continue-art"
                  progress={false}
                  posterOf={i.kind === 'episode' ? i.parent_id : null}
                />
                <span className="continue-text">
                  <span className="continue-title">{i.title}</span>
                  <span className="continue-meta mono">{metaLine(i)}</span>
                  <span className="waterline">
                    <span className="waterline-fill" style={{ width: `${watchedPct(i) ?? 0}%` }} />
                  </span>
                </span>
              </button>
            ))}
          </div>
        </>
      )}
      {/* A library with nothing in it gets no shelf: an empty rail under a
          heading reads as a failure to load. That judgement can only be made
          once it has answered — before then it is a skeleton, and if it
          failed it keeps its heading to say so. */}
      {shelves
        ?.filter((s) => s.pending || s.failed || s.items.length > 0)
        .map((s) => (
          <Shelf
            // The key carries the state, so recovering from a failure remounts
            // the shelf: its `items` are seeded from this prop exactly once,
            // and without the remount a repaired shelf would keep the empty
            // list it failed with.
            key={`${s.library.id}:${s.pending ? 'pending' : s.failed ? 'failed' : 'ok'}`}
            shelf={s}
            onOpen={onOpen}
            onOpenItem={onOpenItem}
            onRetry={() => retryShelf(s.library)}
          />
        ))}
    </main>
  )
}
