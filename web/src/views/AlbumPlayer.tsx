import { useEffect, useRef, useState } from 'react'
import { endSession, postProgress, startSessionDirect, type Item, type Session } from '../api'

/// Queue playback for an album (HUB-27): one direct-play session per
/// track, auto-advance on ended, prev/next. The <audio> element streams
/// with the media cookie.
export default function AlbumPlayer({
  tracks,
  startAt,
  onTrackChange,
  onStop,
}: {
  tracks: Item[]
  startAt: number
  onTrackChange: (index: number) => void
  onStop: () => void
}) {
  const [index, setIndex] = useState(startAt)
  const [session, setSession] = useState<Session | null>(null)
  const [error, setError] = useState('')
  const audioRef = useRef<HTMLAudioElement>(null)
  const sessionRef = useRef<Session | null>(null)

  // jump when the user clicks another track row
  useEffect(() => setIndex(startAt), [startAt])

  useEffect(() => {
    let cancelled = false
    const prev = sessionRef.current
    sessionRef.current = null
    setSession(null)
    if (prev) void endSession(prev.session_id)
    const track = tracks[index]
    if (!track) return
    onTrackChange(index)
    startSessionDirect(track.id)
      .then((s) => {
        if (cancelled) {
          void endSession(s.session_id)
          return
        }
        sessionRef.current = s
        setSession(s)
        setError('')
      })
      .catch((e) => setError(String(e)))
    return () => {
      cancelled = true
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [index, tracks])

  // one cleanup at unmount for whatever session is live then
  useEffect(
    () => () => {
      if (sessionRef.current) void endSession(sessionRef.current.session_id, true)
    },
    []
  )

  const advance = (dir: number) => {
    const next = index + dir
    if (next < 0 || next >= tracks.length) {
      onStop()
      return
    }
    setIndex(next)
  }

  const track = tracks[index]
  return (
    <div className="queue-bar">
      <button className="btn ghost small" onClick={() => advance(-1)} disabled={index === 0}>
        ⏮
      </button>
      <button className="btn ghost small" onClick={() => advance(1)}>
        ⏭
      </button>
      <span className="now mono">
        {track ? `${index + 1}/${tracks.length} · ${track.title}` : ''}
      </span>
      {error && <span className="error">{error}</span>}
      {session && (
        <audio
          ref={audioRef}
          src={session.stream_url}
          autoPlay
          controls
          onEnded={() => {
            const s = sessionRef.current
            const el = audioRef.current
            if (s && el) void postProgress(s.session_id, el.duration * 1000)
            advance(1)
          }}
        />
      )}
      <button className="btn ghost small" onClick={onStop}>
        ✕
      </button>
    </div>
  )
}
