import { useEffect, useState } from 'react'
import { artworkUrl, fetchItems, fetchLibraries, type Item, type LibrarySummary } from '../api'
import placeholder from '../assets/placeholder.svg'

/// Per library, in the cross-library view. Enough to recognise whether
/// what you wanted is in there; the library itself is one click away for
/// the rest.
const PER_LIBRARY = 5

type Hit = { library: LibrarySummary; items: Item[]; total: number }

export default function Libraries({
  query,
  onOpen,
  onOpenItem,
}: {
  query: string
  onOpen: (id: string) => void
  onOpenItem: (id: string, fromLib: string) => void
}) {
  const [libs, setLibs] = useState<LibrarySummary[] | null>(null)
  const [hits, setHits] = useState<Hit[] | null>(null)
  const [error, setError] = useState('')

  useEffect(() => {
    fetchLibraries()
      .then((r) => setLibs(r.libraries))
      .catch((e) => setError(String(e)))
  }, [])

  // One request per library rather than one for everything: the endpoint
  // does not say which library an item came from, and "at most five each"
  // is not something a single LIMIT can express. They run concurrently
  // and each is a bounded query, so it costs one round trip.
  useEffect(() => {
    if (!libs) return
    if (!query) {
      setHits(null)
      return
    }
    let stale = false
    Promise.all(
      libs.map((library) =>
        fetchItems({ library: library.id, q: query, limit: PER_LIBRARY })
          .then((r) => ({ library, items: r.items, total: r.total }))
          .catch(() => ({ library, items: [], total: 0 })),
      ),
    ).then((all) => {
      // Answers can arrive after the query has moved on; only the newest
      // set may paint.
      if (stale) return
      setHits(all.filter((h) => h.total > 0))
    })
    return () => {
      stale = true
    }
  }, [libs, query])

  if (error) return <div className="error page-pad">{error}</div>
  if (!libs) return null

  if (query) {
    return (
      <main>
        <div className="library-head">
          <h1>Results</h1>
          <span className="count mono">
            {hits === null ? '' : `${hits.reduce((n, h) => n + h.total, 0)} in ${hits.length}`}
          </span>
        </div>
        {hits !== null && hits.length === 0 && (
          <p className="dim">Nothing in any library matches “{query}”.</p>
        )}
        {hits?.map((h) => (
          <section className="result-lib" key={h.library.id}>
            {/* The library's own name is the way through to it. The
                query stays in the bar, where it becomes that library's
                filter — so this reads as "show me all of these". */}
            <button className="result-lib-head" onClick={() => onOpen(h.library.id)}>
              <span className="result-lib-name">{h.library.name}</span>
              <span className="chip dim">{h.library.media_type}</span>
              <span className="count mono">
                {h.total > PER_LIBRARY ? `${PER_LIBRARY} of ${h.total} →` : `${h.total} →`}
              </span>
            </button>
            <ul className="result-list">
              {h.items.map((i) => (
                <li key={i.id}>
                  <button
                    className="result-row"
                    onClick={() => onOpenItem(i.id, h.library.id)}
                  >
                    <img
                      className="result-art"
                      src={artworkUrl(i.id, i.art_version, 'thumb')}
                      loading="lazy"
                      alt=""
                      onError={(e) => {
                        e.currentTarget.onerror = null
                        e.currentTarget.src = placeholder
                      }}
                    />
                    <span className="result-title">{i.title}</span>
                    <span className="result-meta mono">
                      {i.kind === 'album' ? (i.artist ?? '') : (i.year ?? '')}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          </section>
        ))}
      </main>
    )
  }

  return (
    <main>
      <div className="library-head">
        <h1>Libraries</h1>
      </div>
      {libs.length === 0 && (
        <p className="dim">
          No libraries yet. Connect a mediahost — each collection it announces
          becomes a library here.
        </p>
      )}
      <ul className="grid">
        {libs.map((l) => (
          <li key={l.id}>
            <button className="card" onClick={() => onOpen(l.id)}>
              <span className="chip dim">{l.media_type}</span>
              <span className="card-title">{l.name}</span>
            </button>
          </li>
        ))}
      </ul>
    </main>
  )
}
