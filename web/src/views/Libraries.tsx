import { useEffect, useState } from 'react'
import { fetchLibraries, type LibrarySummary } from '../api'

export default function Libraries({ onOpen }: { onOpen: (id: string) => void }) {
  const [libs, setLibs] = useState<LibrarySummary[] | null>(null)
  const [error, setError] = useState('')

  useEffect(() => {
    fetchLibraries()
      .then((r) => setLibs(r.libraries))
      .catch((e) => setError(String(e)))
  }, [])

  if (error) return <div className="error page-pad">{error}</div>
  if (!libs) return null

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
