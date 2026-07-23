import { useEffect, useState } from 'react'
import { json, type Item } from '../api'

function fmtResume(ms: number) {
  const s = Math.floor(ms / 1000)
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`
}

export default function Library({ onOpen }: { onOpen: (id: string) => void }) {
  const [items, setItems] = useState<Item[] | null>(null)
  const [filter, setFilter] = useState('')
  const [error, setError] = useState('')

  useEffect(() => {
    json<{ items: Item[] }>('/api/v1/items')
      .then((r) => setItems(r.items))
      .catch((e) => setError(String(e)))
  }, [])

  if (error) return <div className="error page-pad">{error}</div>
  if (!items) return null

  const needle = filter.toLowerCase()
  const shown = items.filter((i) => !needle || i.title.toLowerCase().includes(needle))

  return (
    <main>
      <div className="library-head">
        <h1>Library</h1>
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
          Nothing here yet. Connect a mediahost with a movies collection and
          its scan will fill this page.
        </p>
      )}
      <ul className="grid">
        {shown.map((i) => (
          <li key={i.id}>
            <button className="card" onClick={() => onOpen(i.id)}>
              {i.kind === 'show' && <span className="chip dim">series</span>}
              <span className="card-title">{i.title}</span>
              <span className="card-meta mono">
                {i.year ?? '—'}
                {i.sources > 1 ? ` · ${i.sources} sources` : ''}
              </span>
              <span className="card-state">
                {i.played ? (
                  <span className="seen">seen</span>
                ) : i.resume_position_ms ? (
                  <span className="resume">resume {fmtResume(i.resume_position_ms)}</span>
                ) : null}
              </span>
            </button>
          </li>
        ))}
      </ul>
    </main>
  )
}
