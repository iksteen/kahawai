import { useEffect, useState } from 'react'
import {
  adminApplyMatch,
  adminReviewList,
  adminReviewSearch,
  type MatchCandidate,
  type ReviewEntry,
} from '../api'

/// HUB-8 match review: fix the misses, audit the weak matches.
export default function ReviewQueue() {
  const [entries, setEntries] = useState<ReviewEntry[]>([])
  const [open, setOpen] = useState<string | null>(null)
  const [query, setQuery] = useState('')
  const [results, setResults] = useState<MatchCandidate[] | null>(null)
  const [busy, setBusy] = useState(false)

  const refresh = () =>
    adminReviewList()
      .then((r) => setEntries(r.entries))
      .catch(() => {})
  useEffect(() => {
    refresh()
  }, [])

  const expand = (e: ReviewEntry) => {
    setOpen(e.item_id)
    setQuery(e.title)
    setResults(null)
  }

  const search = async (e: ReviewEntry) => {
    setBusy(true)
    try {
      const r = await adminReviewSearch(e.kind, query, e.year)
      setResults(r.candidates)
    } finally {
      setBusy(false)
    }
  }

  const apply = async (
    e: ReviewEntry,
    action: 'pick' | 'confirm' | 'reject',
    candidate?: MatchCandidate,
  ) => {
    await adminApplyMatch(e.item_id, action, candidate)
    setOpen(null)
    setResults(null)
    refresh()
  }

  if (!entries.length) return null
  return (
    <>
      <h2>Match review ({entries.length})</h2>
      <ul className="rows review">
        {entries.map((e) => (
          <li key={e.item_id}>
            <div className="review-row">
              <button className="card episode" onClick={() => expand(e)}>
                <span className={`chip ${e.confidence === 'miss' ? 'warn' : 'dim'}`}>
                  {e.confidence}
                </span>{' '}
                <b>{e.title}</b> {e.year ? `(${e.year})` : ''}{' '}
                <span className="dim mono small-note">{e.path}</span>
                {e.confidence === 'weak' && (
                  <span className="dim">
                    {' '}
                    → {e.matched_title} ({e.premiered?.slice(0, 4)}) via {e.provider}
                  </span>
                )}
              </button>
              {e.confidence === 'weak' && (
                <>
                  <button className="btn small" onClick={() => void apply(e, 'confirm')}>
                    Confirm
                  </button>
                  <button className="btn ghost small" onClick={() => void apply(e, 'reject')}>
                    Reject
                  </button>
                </>
              )}
            </div>
            {open === e.item_id && (
              <div className="review-search">
                <div className="row-form">
                  <input
                    value={query}
                    onChange={(ev) => setQuery(ev.target.value)}
                    onKeyDown={(ev) => ev.key === 'Enter' && void search(e)}
                    placeholder="Search providers"
                  />
                  <button className="btn small" disabled={busy} onClick={() => void search(e)}>
                    Search
                  </button>
                </div>
                {results && (
                  <ul className="grid candidates">
                    {results.map((c) => (
                      <li key={`${c.provider}-${c.id}`}>
                        <button className="card" onClick={() => void apply(e, 'pick', c)}>
                          {c.poster_url && (
                            <img className="card-art" src={c.poster_url} alt="" loading="lazy" />
                          )}
                          <span className="card-title">{c.title}</span>
                          <span className="card-meta mono">
                            {c.release_date?.slice(0, 4) ?? '—'} · {c.provider}
                          </span>
                        </button>
                      </li>
                    ))}
                    {!results.length && <li className="dim">no candidates</li>}
                  </ul>
                )}
              </div>
            )}
          </li>
        ))}
      </ul>
    </>
  )
}
