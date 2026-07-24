import { useEffect, useState } from 'react'
import {
  adminApplyMatch,
  adminReviewSearch,
  type Item,
  type MatchCandidate,
} from '../api'

/// HUB-8 hand-matching, launched from a card's search button: provider
/// search prefilled with the item's title, poster grid, one-click pick.
export default function MatchDialog({
  item,
  onClose,
  onApplied,
}: {
  item: Item
  onClose: () => void
  onApplied: () => void
}) {
  // Anchor on the FILE identity: the display title is the (possibly
  // wrong) match we're here to judge.
  const fileTitle = item.file_title ?? item.title
  const fileYear = item.file_year ?? null
  const [query, setQuery] = useState(fileTitle)
  const [results, setResults] = useState<MatchCandidate[] | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const weak = item.match_confidence === 'weak'

  const search = async (q: string) => {
    setBusy(true)
    setError('')
    try {
      const r = await adminReviewSearch(item.kind, q, fileYear)
      setResults(r.candidates)
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  // Search immediately with the file title.
  useEffect(() => {
    void search(fileTitle)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [item.id])

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && onClose()
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])

  const apply = async (action: 'pick' | 'confirm' | 'reject', c?: MatchCandidate) => {
    setBusy(true)
    try {
      await adminApplyMatch(item.id, action, c)
      onApplied()
      onClose()
    } catch (e) {
      setError(String(e))
      setBusy(false)
    }
  }

  return (
    <div className="dialog-backdrop" onClick={onClose}>
      <div className="dialog" onClick={(e) => e.stopPropagation()}>
        <div className="dialog-head">
          <h2>
            Match “{fileTitle}” {fileYear ? `(${fileYear})` : ''}
          </h2>
          <button className="btn ghost small" onClick={onClose}>
            ✕
          </button>
        </div>
        {weak && (
          <div className="row-form dialog-weak">
            <span className="dim">
              Uncertain match: <b>{item.matched_title ?? item.title}</b>
              {item.year ? ` (${item.year})` : ''} — confirm it or pick a better one.
            </span>
            <button className="btn small" disabled={busy} onClick={() => void apply('confirm')}>
              Confirm current
            </button>
            <button className="btn ghost small" disabled={busy} onClick={() => void apply('reject')}>
              Reject
            </button>
          </div>
        )}
        <div className="row-form">
          <input
            autoFocus
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && void search(query)}
            placeholder="Search providers"
          />
          <button className="btn small" disabled={busy} onClick={() => void search(query)}>
            Search
          </button>
        </div>
        {error && <div className="error">{error}</div>}
        {results && (
          <ul className="grid candidates">
            {results.map((c) => (
              <li key={`${c.provider}-${c.id}`}>
                <button className="card" disabled={busy} onClick={() => void apply('pick', c)}>
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
            {!results.length && <li className="dim">no candidates — try a different query</li>}
          </ul>
        )}
      </div>
    </div>
  )
}
