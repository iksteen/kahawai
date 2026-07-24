import { useEffect, useState } from 'react'
import {
  artworkUrl,
  fetchChildren,
  fetchItem,
  startSession,
  type Item,
  type ItemDetail,
  type Session,
  type Source,
} from '../api'
import AlbumPlayer from './AlbumPlayer'
import tmdbLogo from '../assets/tmdb.svg'

const GB = 1024 * 1024 * 1024

// S01E02 for seasoned episodes; E11 for absolute numbering (anime).
function seLabel(season: number | null, episode: number | null) {
  const e = `E${String(episode ?? 0).padStart(2, '0')}`
  return season === null ? e : `S${String(season).padStart(2, '0')}${e}`
}

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
  autoPlay,
  fromLib,
  onPlay,
  onOpenEpisode,
  onOpenLibrary,
}: {
  id: string
  autoPlay?: boolean
  fromLib: string
  onPlay: (item: Item, session: Session, resumeMs: number) => void
  onOpenEpisode: (id: string) => void
  onOpenLibrary: (id: string) => void
}) {
  const [item, setItem] = useState<ItemDetail | null>(null)
  const [episodes, setEpisodes] = useState<Item[]>([])
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    setItem(null)
    setEpisodes([])
    fetchItem(id).then(setItem).catch((e) => setError(String(e)))
  }, [id])
  useEffect(() => {
    if (item?.kind === 'show' || item?.kind === 'album') {
      fetchChildren(item.id)
        .then((c) => setEpisodes(c.children))
        .catch((e) => setError(String(e)))
    }
  }, [item?.id, item?.kind])
  const [queueAt, setQueueAt] = useState<number | null>(null)
  // Deep-linked or history-forwarded /play URLs: start playback once
  // the item is loaded (shows have nothing to autoplay).
  const [autoPlayed, setAutoPlayed] = useState(false)
  useEffect(() => {
    if (autoPlay && !autoPlayed && item && item.kind !== 'show') {
      setAutoPlayed(true)
      const best = item.sources_detail[0]
      if (best?.available) void play(autoMode(best))
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [autoPlay, autoPlayed, item?.id])

  if (error) return <div className="error page-pad">{error}</div>
  if (!item) return null

  // Hierarchical back: episode → its series page, series/movie → the
  // library in the URL (the one we navigated from).
  const goUp = () => {
    if (item.kind === 'episode' && item.parent_id) onOpenEpisode(item.parent_id)
    else onOpenLibrary(fromLib)
  }
  const upLabel =
    item.kind === 'episode' && item.parent_id ? `← ${item.show_title ?? 'Series'}` : '← Library'

  if (item.kind === 'album') {
    const tracks = episodes // children, ordered disc/track by the API
    return (
      <main>
        <button className="btn ghost small" onClick={goUp}>
          {upLabel}
        </button>
        <div className="detail-head album-head">
          <img
            className="card-art album-art"
            src={artworkUrl(item.id)}
            alt=""
            onError={(e) => (e.currentTarget.style.display = 'none')}
          />
          <div>
            <h1>
              {item.title} {item.year && <span className="year">({item.year})</span>}
            </h1>
            <div className="detail-sub mono">
              {item.artist ?? ''} · {tracks.length} tracks
            </div>
            <div className="play-row">
              <button className="btn" disabled={!tracks.length} onClick={() => setQueueAt(0)}>
                Play album
              </button>
            </div>
          </div>
        </div>
        <ul className="rows">
          {tracks.map((t, i) => (
            <li key={t.id}>
              <button
                className={`card episode track-row${queueAt === i ? ' playing' : ''}`}
                onClick={() => setQueueAt(i)}
              >
                <span className="tno mono">{t.episode ?? i + 1}</span>
                <span>{t.title}</span>
                {t.played && <span className="seen"> ✓</span>}
              </button>
            </li>
          ))}
        </ul>
        {queueAt !== null && (
          <AlbumPlayer
            tracks={tracks}
            startAt={queueAt}
            onTrackChange={(i) => setQueueAt(i)}
            onStop={() => setQueueAt(null)}
          />
        )}
      </main>
    )
  }

  if (item.kind === 'show') {
    // null season = absolute numbering (anime); distinct from Specials.
    const seasonLabel = (s: number | null) =>
      s === null ? 'Episodes' : s === 0 ? 'Specials' : `Season ${s}`
    const seasons = [...new Set(episodes.map((e) => e.season))]
    // First unwatched (or in-progress) episode = the continue point.
    const next = episodes.find((e) => !e.played)
    return (
      <main>
        <button className="btn ghost small" onClick={goUp}>
          {upLabel}
        </button>
        <div className="detail-head">
          <h1>
            {item.title} {item.year && <span className="year">({item.year})</span>}
          </h1>
          <div className="detail-sub mono">
            {episodes.length} episodes
            {next && (
              <>
                {' · next: '}
                <button className="btn ghost small" onClick={() => onOpenEpisode(next.id)}>
                  {seLabel(next.season, next.episode)} {next.title}
                </button>
              </>
            )}
          </div>
        </div>
        {seasons.map((s) => (
          <section key={String(s)}>
            <h2>{seasonLabel(s)}</h2>
            <ul className="rows">
              {episodes
                .filter((e) => e.season === s)
                .map((e) => (
                  <li key={e.id}>
                    <button className="card episode" onClick={() => onOpenEpisode(e.id)}>
                      <span className="mono dim">
                        E{String(e.episode ?? 0).padStart(2, '0')}
                      </span>{' '}
                      {e.title}
                      {e.played && <span className="seen"> ✓</span>}
                      {!e.played && e.resume_position_ms ? (
                        <span className="dim"> · resume</span>
                      ) : null}
                    </button>
                  </li>
                ))}
            </ul>
          </section>
        ))}
        {error && <div className="error">{error}</div>}
      </main>
    )
  }

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
      // Remux/transcode sessions start their pipeline at the resume
      // point (§6) — no waiting for a transcode to catch up. Direct
      // sessions resume client-side.
      const start = fromStart ? 0 : resumeMs
      const session = await startSession(item!.id, mode, mode === 'direct' ? 0 : start)
      onPlay(item!, session, start)
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <main>
      <button className="btn ghost small" onClick={goUp}>
        {upLabel}
      </button>
      <div className="detail-head">
        <h1>
          {item.kind === 'episode' && item.show_title ? `${item.show_title} · ` : ''}
          {item.kind === 'episode'
            ? `${seLabel(item.season, item.episode)} · `
            : ''}
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

      {item.metadata?.overview && (
        <section className="meta-block">
          <p className="overview">{item.metadata.overview}</p>
          <div className="detail-sub mono">
            {item.metadata.premiered && <span>{item.metadata.premiered}</span>}
            {item.metadata.rating ? <span> · ★ {item.metadata.rating.toFixed(1)}</span> : null}
            {item.metadata.confidence === 'weak' && (
              <span className="dim"> · uncertain match</span>
            )}
          </div>
        </section>
      )}

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
      {item.metadata && (
        <footer className="tmdb-attrib">
          <img src={tmdbLogo} alt="TMDB" />
          <span>
            This product uses TMDB and the TMDB APIs but is not endorsed, certified, or
            otherwise approved by TMDB.
          </span>
        </footer>
      )}
    </main>
  )
}
