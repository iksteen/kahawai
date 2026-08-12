import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { artworkSrcSet, artworkUrl, fetchItems, fetchLibraries, isAdmin, type Item } from '../api'
import Icon, { type IconName } from '../icons'
import { metaLine, targetOf, watchedPct } from '../label'
import { notify } from '../toast'
import MatchDialog from './MatchDialog'

/// Items per request. Smaller than the hub's 200 default because a chunk
/// is now fetched to fill a viewport, not to be a page someone reads.
const CHUNK = 100
/// Rows kept live above and below the viewport, so a flick of the wheel
/// lands on cards that are already there.
const OVERSCAN = 3
/// Must match `.grid { gap }` in styles.css — the one number this file
/// cannot measure, because it is between the cells rather than in one.
const GAP = 14

/// How a library's items are shaped. A sleeve is square and a poster is
/// two by three, and the narrower shape can sit in a narrower column.
/// Both are handed to CSS as custom properties so the stylesheet keeps
/// the rules and this keeps only the values.
function shapeOf(mediaType: string) {
  return mediaType === 'music'
    ? { '--card-min': '150px', '--card-ratio': '1' }
    : { '--card-min': '140px', '--card-ratio': '2 / 3' }
}

function kindGlyph(kind: string): IconName | null {
  if (kind === 'movie') return 'movie'
  if (kind === 'show' || kind === 'episode') return 'show'
  if (kind === 'album' || kind === 'track') return 'album'
  return null
}

function Card({
  item: i,
  onOpen,
  onMatch,
}: {
  item: Item
  onOpen: (id: string) => void
  onMatch: (item: Item) => void
}) {
  return (
    <>
      {isAdmin() && (i.kind === 'movie' || i.kind === 'show') && (
        <button
          className={`match-btn ${
            !i.match_confidence ||
            i.match_confidence === 'miss' ||
            i.match_confidence === 'rejected'
              ? 'miss'
              : i.match_confidence === 'weak'
                ? 'weak'
                : ''
          }`}
          title={
            i.match_confidence === 'weak'
              ? 'Uncertain match — review'
              : i.match_confidence === 'auto' || i.match_confidence === 'manual'
                ? 'Re-match metadata'
                : 'No metadata match — fix'
          }
          onClick={() => onMatch(i)}
        >
          <svg
            width="14"
            height="14"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <circle cx="11" cy="11" r="8" />
            <line x1="21" y1="21" x2="16.65" y2="16.65" />
          </svg>
        </button>
      )}
      <button className="card" onClick={() => onOpen(targetOf(i))}>
        <span className="card-artbox">
          <img
            className="card-art"
            src={artworkUrl(i.id, i.art_version, 'card')}
            srcSet={artworkSrcSet(i.id, i.art_version)}
            loading="lazy"
            alt=""
            // See Libraries.tsx: hide it and let the swell behind show.
            onError={(e) => e.currentTarget.classList.add('art-failed')}
          />
          {kindGlyph(i.kind) && (
            <span className="kind-badge" title={i.kind === 'show' ? 'series' : i.kind}>
              <Icon name={kindGlyph(i.kind) as IconName} />
            </span>
          )}
          {i.played && (
            <span className="seen-badge" title="seen">
              <Icon name="check" />
            </span>
          )}
          {!i.played && watchedPct(i) !== null && (
            <span className="card-progress" title="Partly watched">
              <span className="card-progress-fill" style={{ width: `${watchedPct(i)}%` }} />
            </span>
          )}
        </span>
        <span className="card-title">{i.title}</span>
        <span className="card-meta mono">
          {[metaLine(i) || '—', i.sources > 1 ? `${i.sources} sources` : '']
            .filter(Boolean)
            .join(' · ')}
        </span>
      </button>
    </>
  )
}

export default function Library({
  libraryId,
  query,
  onOpen,
  onResetSearch,
  onHome,
}: {
  libraryId: string
  /// Already debounced, and owned by the header's search box — on this
  /// screen that box filters this library.
  query: string
  onOpen: (id: string) => void
  /// The library title doubles as "show me everything again": the
  /// filter lives in the app header, so the view can only ask.
  onResetSearch: () => void
  onHome: () => void
}) {
  const [name, setName] = useState('Library')
  // Decides the card's shape, so it is an input to the row-height
  // measurement below and not merely a label.
  const [mediaType, setMediaType] = useState('')
  const [sort, setSort] = useState('title')
  const [total, setTotal] = useState<number | null>(null)
  // The library's size with no filter applied, remembered from the last
  // time there was none, so a filtered count can say "12 of 2242" without
  // a second request asking something we already knew.
  const [libTotal, setLibTotal] = useState<number | null>(null)
  // Sparse: index in the FULL result set → item. Holes are rows that
  // exist and have not been fetched, which is not the same as rows that
  // do not exist, and the difference is what keeps the layout still.
  const [loaded, setLoaded] = useState<Map<number, Item>>(new Map())
  const [rows, setRows] = useState({ start: 0, end: 0 })
  // Measured, never assumed: the card art is `aspect-ratio: 1` on a fluid
  // grid column, so a row's height is a function of the window width.
  const [metric, setMetric] = useState<{ cols: number; rowH: number } | null>(null)
  // Width, not a resize counter: a resize that does not change the width
  // cannot change the column count or the row height.
  const [width, setWidth] = useState(() => window.innerWidth)
  const [error, setError] = useState('')
  const [matching, setMatching] = useState<Item | null>(null)

  const wrapRef = useRef<HTMLDivElement>(null)
  const gridRef = useRef<HTMLUListElement>(null)
  const asked = useRef(new Set<number>())
  /// Chunks that failed and have not since succeeded. The error line is about
  /// this set being non-empty, not about the last thing that happened.
  const failed = useRef(new Set<number>())
  // Bumped whenever the result set changes identity. A reply carrying an
  // older generation describes a library or a search we have left.
  const gen = useRef(0)

  // The generation whose first reply REPLACES what is on screen rather
  // than merging into it.
  const replacing = useRef(0)

  // Start over on a different result set — WITHOUT clearing what is
  // displayed. Blanking here emptied the page for the length of a round
  // trip, and an empty page is a different tree, so React unmounted the
  // search box mid-keystroke and took the caret with it. The old results
  // stay up until the new ones arrive to replace them.
  const reset = () => {
    gen.current += 1
    replacing.current = gen.current
    asked.current.clear()
    // The failures belonged to the result set being replaced. Left standing,
    // the line stayed on screen over results that had loaded perfectly — the
    // only thing that clears it is a chunk arriving while `failed` is empty,
    // and the chunk that failed is in a set nobody is asking for any more. So
    // a filter keystroke or a jump to another library carried the old
    // library's red line with it, for as long as the page was open.
    failed.current.clear()
    setError('')
    setRows({ start: 0, end: 0 })
  }

  const loadChunk = (chunk: number) => {
    if (asked.current.has(chunk)) return
    asked.current.add(chunk)
    const mine = gen.current
    fetchItems({ library: libraryId, q: query, sort, limit: CHUNK, offset: chunk * CHUNK })
      .then((r) => {
        if (mine !== gen.current) return
        const swap = replacing.current === mine
        if (swap) replacing.current = 0
        setTotal(r.total)
        if (!query) setLibTotal(r.total)
        setLoaded((prev) => {
          const next = swap ? new Map<number, Item>() : new Map(prev)
          r.items.forEach((it, k) => next.set(r.offset + k, it))
          return next
        })
        // Cleared only when nothing is still missing. Clearing on ANY arrival
        // hid a real hole — one chunk failing beside one succeeding left a
        // hundred placeholder cards and silence — and never clearing left a
        // red line over a grid that had been complete for minutes.
        failed.current.delete(chunk)
        if (failed.current.size === 0) setError('')
      })
      .catch((e) => {
        if (mine !== gen.current) return
        asked.current.delete(chunk) // a failed chunk must be retryable
        failed.current.add(chunk)
        setError(String(e))
      })
  }

  // Note: the query is NOT cleared here. Arriving from a search on the
  // home screen must land with that search still applied — it is the
  // whole point of following a library through from its results.
  useEffect(() => {
    reset()
    setLibTotal(null)
    fetchLibraries()
      .then((r) => {
        const lib = r.libraries.find((l) => l.id === libraryId)
        setName(lib?.name ?? 'Library')
        setMediaType(lib?.media_type ?? '')
      })
      // Silence here read as a library called "Library" holding cards of the
      // wrong shape: `mediaType` is what `shapeOf` reads for the poster aspect
      // ratio, so a music library laid itself out as films with nothing said.
      // A toast rather than taking the screen, because this request failing
      // alone leaves a perfectly good grid underneath it. When the whole hub is
      // down the grid says so too and this is a second sentence about the same
      // outage — worth it, since the two name different things and neither is
      // guessable from the other.
      .catch((e: unknown) => notify(`Could not load the library details: ${e}`))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [libraryId])

  // A new result set: forget everything and start from the top, because
  // scroll position in the old one means nothing in the new one.
  useEffect(() => {
    reset()
    window.scrollTo({ top: 0 })
    loadChunk(0)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [libraryId, query, sort])

  // Measure once there is a card to measure. Row pitch is the cell height
  // plus the gap; column count comes from the resolved track list, so it
  // agrees with what CSS actually did rather than with a copy of the rule.
  //
  // Deliberately NOT run on every render. Measuring writes state, so a
  // render-driven measurement is a cycle, and it only stays quiet while
  // every cell is the same height — one that is not locks the renderer.
  // These four inputs are the only things that can change the answer.
  //
  // `mediaType` is one of them: it sets the art's aspect ratio, so it
  // decides the cell's height. It arrives a round trip after the first
  // cards, and without it here a music library would keep the row pitch
  // it measured while its square sleeves were still poster-shaped — every
  // row then reserving a poster's height for a sleeve, so the reserved
  // total ran long and the last screenful was empty space.
  useLayoutEffect(() => {
    const grid = gridRef.current
    const cell = grid?.firstElementChild as HTMLElement | null
    if (!grid || !cell) return
    const cols = getComputedStyle(grid).gridTemplateColumns.split(' ').filter(Boolean).length
    const rowH = cell.getBoundingClientRect().height + GAP
    if (!cols || rowH <= GAP) return
    // ponytail: the half-pixel tolerance exists so a hair of text-metric
    // jitter cannot start a measure/render loop, and its cost is that the
    // same hair is multiplied by the row count — measured at 5px of slack
    // over 321 rows of a 2242-album library, 0.007% of the reserved
    // height. Reserve from an integer pitch if that ever shows.
    if (!metric || metric.cols !== cols || Math.abs(metric.rowH - rowH) > 0.5) {
      setMetric({ cols, rowH })
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [total, width, loaded.size > 0, mediaType])

  // Which rows the window is over. Re-run on scroll and on resize; resize
  // also invalidates the measurement, which the layout effect above
  // retakes on the next paint.
  useEffect(() => {
    const recompute = () => {
      const wrap = wrapRef.current
      if (!wrap || !metric || total === null) return
      const top = wrap.getBoundingClientRect().top + window.scrollY
      const firstVisible = Math.floor((window.scrollY - top) / metric.rowH)
      const deep = Math.ceil(window.innerHeight / metric.rowH)
      const totalRows = Math.ceil(total / metric.cols)
      const start = Math.max(0, firstVisible - OVERSCAN)
      const end = Math.min(totalRows - 1, firstVisible + deep + OVERSCAN)
      setRows((prev) => (prev.start === start && prev.end === end ? prev : { start, end }))
    }
    const onResize = () => {
      setWidth(window.innerWidth)
      recompute()
    }
    recompute()
    window.addEventListener('scroll', recompute, { passive: true })
    window.addEventListener('resize', onResize)
    return () => {
      window.removeEventListener('scroll', recompute)
      window.removeEventListener('resize', onResize)
    }
  }, [metric, total])

  // Fetch whatever the visible rows need and do not have.
  useEffect(() => {
    if (!metric || total === null) return
    const from = rows.start * metric.cols
    const to = Math.min(total - 1, rows.end * metric.cols + metric.cols - 1)
    for (let c = Math.floor(from / CHUNK); c <= Math.floor(to / CHUNK); c++) {
      if (!loaded.has(c * CHUNK)) loadChunk(c)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [rows, metric, total])

  // Deliberately no early `return` for loading or error. Every one of
  // them swaps the whole tree, and swapping the tree destroys the search
  // box the user is typing into. The chrome is always mounted; only what
  // hangs below it changes.
  const count = total ?? 0
  const cols = metric?.cols ?? 1
  const totalRows = Math.max(1, Math.ceil(count / cols))
  // Before the first measurement there is nothing to reserve against, so
  // render the first chunk plainly and let the layout effect measure it.
  const window_: number[] = []
  if (metric) {
    for (let r = rows.start; r <= rows.end; r++) {
      for (let c = 0; c < cols; c++) {
        const i = r * cols + c
        if (i < count) window_.push(i)
      }
    }
  } else {
    for (let i = 0; i < Math.min(CHUNK, count); i++) window_.push(i)
  }

  return (
    <main>
      {/* The wordmark opens the jump menu now, so home needs saying
          somewhere. */}
      <button className="btn ghost small back" onClick={onHome}>
        ← Home
      </button>
      <div className="library-head">
        <h1 className="clickable" onClick={onResetSearch}>
          {name}
        </h1>
        <select className="sort filter" value={sort} onChange={(e) => setSort(e.target.value)}>
          <option value="title">Title A–Z</option>
          <option value="-title">Title Z–A</option>
          <option value="-added">Recently added</option>
          <option value="added">Oldest added</option>
          <option value="-year">Newest first</option>
          <option value="year">Oldest first</option>
        </select>
        {/* Filtering says what it is filtering: 12 on its own leaves you
            wondering whether the other 2230 are missing or excluded. */}
        <span className="count mono">
          {total === null ? '' : query && libTotal !== null ? `${total}/${libTotal}` : total}
        </span>
      </div>
      {error && (
        <p className="error">
          {error}{' '}
          <button
            className="linklike"
            onClick={() => {
              setError('')
              // Ask again, rather than only repainting. `loaded` is not a
              // dependency of the effect that fetches, so a new Map identity
              // ran nothing at all — the message went and the page stayed
              // empty for ever. `rows` IS one, so a fresh object re-runs it;
              // and when the FIRST chunk failed there is no `total`, so that
              // effect returns early and chunk 0 has to be asked for by hand.
              for (const c of failed.current) asked.current.delete(c)
              if (total === null) loadChunk(0)
              else setRows((r) => ({ ...r }))
            }}
          >
            Try again
          </button>
        </p>
      )}
      {total === 0 && (
        <p className="dim">
          {query
            ? `Nothing matches “${query}”.`
            : 'Nothing here yet. Attach a collection to this library and its scan will fill this page.'}
        </p>
      )}
      {/* The whole library's height is reserved from the first response,
          before a single card past the fold has been fetched. That is the
          difference from infinite scroll, where the page grows as you go
          and the scrollbar jumps under the thumb every time it does. */}
      <div
        className="grid-scroll"
        ref={wrapRef}
        style={metric ? { height: totalRows * metric.rowH - GAP } : undefined}
      >
        <ul
          className="grid"
          ref={gridRef}
          style={
            {
              ...shapeOf(mediaType),
              ...(metric ? { transform: `translateY(${rows.start * metric.rowH}px)` } : {}),
            } as React.CSSProperties
          }
        >
          {window_.map((i) => {
            const item = loaded.get(i)
            return (
              <li key={i} className="card-cell">
                {item ? (
                  <Card item={item} onOpen={onOpen} onMatch={setMatching} />
                ) : (
                  // The SAME box, structurally — art, title line, meta
                  // line — not merely a similar one. A row that has not
                  // arrived must occupy exactly what it will occupy once
                  // it does, or the grid resizes as chunks land, which is
                  // the layout shift the reserved height exists to avoid.
                  // It also keeps row height a constant the measurement
                  // below can trust; when it did not, measuring a short
                  // placeholder and then a tall card re-entered the
                  // measure/render cycle and locked the renderer.
                  <div className="card card-pending" aria-hidden="true">
                    <span className="card-artbox">
                      <span className="card-art" />
                    </span>
                    <span className="card-title">&nbsp;</span>
                    <span className="card-meta mono">&nbsp;</span>
                  </div>
                )}
              </li>
            )
          })}
        </ul>
      </div>
      {matching && (
        <MatchDialog
          item={matching}
          onClose={() => setMatching(null)}
          onApplied={() => {
            // Re-fetch just the chunk the changed item lives in.
            const at = [...loaded.entries()].find(([, v]) => v.id === matching.id)?.[0]
            if (at !== undefined) {
              const chunk = Math.floor(at / CHUNK)
              asked.current.delete(chunk)
              loadChunk(chunk)
            }
          }}
        />
      )}
    </main>
  )
}
