import { useEffect, useState, type ReactNode } from 'react'
import { artworkUrl, fetchItems, fetchLibraries, isAdmin, type Item } from '../api'
import placeholder from '../assets/placeholder.svg'
import MatchDialog from './MatchDialog'

// Kind glyph for the card art corner (feather icons, MIT).
function KindIcon({ kind }: { kind: string }) {
  const paths: Record<string, ReactNode> = {
    movie: (
      <>
        <rect x="2" y="2" width="20" height="20" rx="2.18" />
        <path d="M7 2v20M17 2v20M2 12h20M2 7h5M2 17h5M17 7h5M17 17h5" />
      </>
    ),
    show: (
      <>
        <rect x="2" y="7" width="20" height="15" rx="2" />
        <polyline points="17 2 12 7 7 2" />
      </>
    ),
    album: (
      <>
        <path d="M9 18V5l12-2v13" />
        <circle cx="6" cy="18" r="3" />
        <circle cx="18" cy="16" r="3" />
      </>
    ),
  }
  const glyph = paths[kind]
  if (!glyph) return null
  return (
    <span className="kind-badge" title={kind === 'show' ? 'series' : kind}>
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
        {glyph}
      </svg>
    </span>
  )
}

export default function Library({
  libraryId,
  onOpen,
}: {
  libraryId: string
  onOpen: (id: string) => void
}) {
  const [items, setItems] = useState<Item[] | null>(null)
  const [name, setName] = useState('Library')
  const [filter, setFilter] = useState('')
  const [error, setError] = useState('')
  const [matching, setMatching] = useState<Item | null>(null)

  const reload = () =>
    fetchItems(libraryId)
      .then((r) => setItems(r.items))
      .catch((e) => setError(String(e)))

  useEffect(() => {
    setItems(null)
    fetchItems(libraryId)
      .then((r) => setItems(r.items))
      .catch((e) => setError(String(e)))
    fetchLibraries()
      .then((r) => setName(r.libraries.find((l) => l.id === libraryId)?.name ?? 'Library'))
      .catch(() => {})
  }, [libraryId])

  if (error) return <div className="error page-pad">{error}</div>
  if (!items) return null

  const needle = filter.toLowerCase()
  const shown = items.filter((i) => !needle || i.title.toLowerCase().includes(needle))

  return (
    <main>
      <div className="library-head">
        <h1>{name}</h1>
        <input
          className="filter"
          placeholder="Filter titles"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
        <span className="count mono">
          {shown.length}/{items.length}
        </span>
      </div>
      {items.length === 0 && (
        <p className="dim">
          Nothing here yet. Attach a collection to this library and its scan
          will fill this page.
        </p>
      )}
      <ul className="grid">
        {shown.map((i) => (
          <li key={i.id} className="card-cell">
            {isAdmin() && (i.kind === 'movie' || i.kind === 'show') && (
              <button
                className={`match-btn ${
                  !i.match_confidence || i.match_confidence === 'miss' || i.match_confidence === 'rejected'
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
                onClick={() => setMatching(i)}
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
            <button className="card" onClick={() => onOpen(i.id)}>
              <span className="card-artbox">
                <img
                  className="card-art"
                  src={artworkUrl(i.id)}
                  loading="lazy"
                  alt=""
                  onError={(e) => {
                    e.currentTarget.onerror = null
                    e.currentTarget.src = placeholder
                  }}
                />
                <KindIcon kind={i.kind} />
                {i.played && (
                  <span className="seen-badge" title="seen">
                    <svg
                      width="14"
                      height="14"
                      viewBox="0 0 24 24"
                      fill="none"
                      stroke="currentColor"
                      strokeWidth="2.5"
                      strokeLinecap="round"
                      strokeLinejoin="round"
                    >
                      <polyline points="20 6 9 17 4 12" />
                    </svg>
                  </span>
                )}
                {!i.played &&
                  !!i.resume_position_ms &&
                  !!i.resume_duration_ms && (
                    <span className="card-progress">
                      <span
                        className="card-progress-fill"
                        style={{
                          width: `${Math.min(100, (i.resume_position_ms / i.resume_duration_ms) * 100)}%`,
                        }}
                      />
                    </span>
                  )}
              </span>
              <span className="card-title">{i.title}</span>
              <span className="card-meta mono">
                {i.kind === 'album' ? (i.artist ?? '—') : (i.year ?? '—')}
                {i.kind === 'album' && i.year ? ` · ${i.year}` : ''}
                {i.sources > 1 ? ` · ${i.sources} sources` : ''}
              </span>

            </button>
          </li>
        ))}
      </ul>
      {matching && (
        <MatchDialog
          item={matching}
          onClose={() => setMatching(null)}
          onApplied={() => void reload()}
        />
      )}
    </main>
  )
}
