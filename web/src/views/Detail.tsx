import { useEffect, useState } from 'react'
import {
  fetchItem,
  startSession,
  type Item,
  type ItemDetail,
  type Session,
  type Source,
} from '../api'

const GB = 1024 * 1024 * 1024

function fmtDuration(ms?: number) {
  if (!ms) return null
  const m = Math.round(ms / 60000)
  return m >= 60 ? `${Math.floor(m / 60)} h ${m % 60} min` : `${m} min`
}

// Browsers demux mp4/webm natively; everything else goes through the
// in-hub remuxer (no transcoder involved).
function autoMode(s?: Source): 'direct' | 'remux' {
  const c = s?.streams?.container
  return c === 'mp4' || c === 'webm' ? 'direct' : 'remux'
}

function Chips({ s }: { s: Source }) {
  const st = s.streams
  if (!st) return null
  return (
    <span className="chips">
      {st.container && <span className="chip">{st.container}</span>}
      {st.video?.map((v, i) => (
        <span className="chip" key={`v${i}`}>
          {v.codec} {v.height ? `${v.height}p` : ''}
        </span>
      ))}
      {st.audio?.map((a, i) => (
        <span className="chip" key={`a${i}`}>
          {a.codec}
          {a.language ? ` ${a.language}` : ''}
        </span>
      ))}
      {st.subtitles?.map((t, i) => (
        <span className="chip dim" key={`s${i}`}>
          {t.format}
        </span>
      ))}
      <span className="chip dim">{(s.size / GB).toFixed(1)} GB</span>
      {!s.available && <span className="chip warn">offline</span>}
    </span>
  )
}

export default function Detail({
  id,
  onBack,
  onPlay,
}: {
  id: string
  onBack: () => void
  onPlay: (item: Item, session: Session, resumeMs: number) => void
}) {
  const [item, setItem] = useState<ItemDetail | null>(null)
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    fetchItem(id).then(setItem).catch((e) => setError(String(e)))
  }, [id])

  if (error) return <div className="error page-pad">{error}</div>
  if (!item) return null

  const best = item.sources_detail[0]
  const duration = best?.streams?.duration_ms
  const resumeMs =
    item.resume_position_ms && duration && item.resume_position_ms < duration * 0.9
      ? item.resume_position_ms
      : 0
  const progress = duration && resumeMs ? (resumeMs / duration) * 100 : 0

  async function play(mode: 'direct' | 'remux', fromStart = false) {
    setBusy(true)
    setError('')
    try {
      const session = await startSession(item!.id, mode)
      onPlay(item!, session, fromStart ? 0 : resumeMs)
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <main>
      <button className="btn ghost small" onClick={onBack}>
        ← Library
      </button>
      <div className="detail-head">
        <h1>
          {item.title} {item.year && <span className="year">({item.year})</span>}
        </h1>
        <div className="detail-sub mono">
          {fmtDuration(duration)}
          {item.played && <span className="seen"> · seen ×{item.play_count}</span>}
        </div>
        {progress > 0 && (
          <div className="waterline">
            <div className="waterline-fill" style={{ width: `${progress}%` }} />
          </div>
        )}
      </div>

      <div className="play-row">
        <button className="btn" disabled={busy || !best?.available} onClick={() => play(autoMode(best))}>
          {resumeMs ? `Resume` : 'Play'}
        </button>
        {resumeMs > 0 && (
          <button className="btn ghost" disabled={busy} onClick={() => play(autoMode(best), true)}>
            Play from start
          </button>
        )}
        <span className="dim small-note">
          {autoMode(best) === 'remux'
            ? 'remuxed to HLS in the hub — streams converted only when needed'
            : 'direct play'}
        </span>
      </div>
      {error && <div className="error">{error}</div>}

      <h2>Sources</h2>
      <ul className="sources">
        {item.sources_detail.map((s) => (
          <li key={s.path_rel}>
            <span className="path mono">{s.path_rel}</span>
            <Chips s={s} />
          </li>
        ))}
      </ul>
    </main>
  )
}
