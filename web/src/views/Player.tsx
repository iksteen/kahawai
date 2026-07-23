import { useEffect, useRef, useState } from 'react'
import Hls from 'hls.js'
import {
  accessToken,
  endSession,
  postProgress,
  seekSession,
  type Item,
  type Session,
} from '../api'

function fmt(ms: number) {
  const s = Math.max(0, Math.floor(ms / 1000))
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  const sec = s % 60
  return h > 0
    ? `${h}:${String(m).padStart(2, '0')}:${String(sec).padStart(2, '0')}`
    : `${m}:${String(sec).padStart(2, '0')}`
}

export default function Player({
  item,
  session,
  resumeMs,
  onClose,
}: {
  item: Item
  session: Session
  resumeMs: number
  onClose: () => void
}) {
  const videoRef = useRef<HTMLVideoElement>(null)
  const hlsRef = useRef<Hls | null>(null)
  const isHls = session.stream_url.endsWith('.m3u8')
  // For HLS sessions the pipeline itself starts at resumeMs, so the
  // playlist's t=0 is that offset; direct sessions play the real file.
  const offsetRef = useRef(isHls ? resumeMs : 0)
  const durationMs = session.duration_ms ?? 0
  const [posMs, setPosMs] = useState(offsetRef.current)
  const [seeking, setSeeking] = useState(false)

  const attach = () => {
    const video = videoRef.current!
    hlsRef.current?.destroy()
    if (isHls && Hls.isSupported()) {
      const hls = new Hls({
        // Media requests carry the Bearer token; the cookie is the
        // fallback for engines we don't drive ourselves.
        xhrSetup: (xhr) => {
          const t = accessToken()
          if (t) xhr.setRequestHeader('Authorization', `Bearer ${t}`)
        },
      })
      hls.loadSource(session.stream_url)
      hls.attachMedia(video)
      hlsRef.current = hls
    } else {
      video.src = session.stream_url // cookie-authenticated
    }
    void video.play().catch(() => undefined)
  }

  // Seek anywhere on the full timeline: inside the produced range it is
  // a plain element seek; beyond it the hub restarts the pipeline at the
  // target (§6) and we re-attach to the same URL.
  const seekTo = async (targetMs: number) => {
    const video = videoRef.current!
    if (!isHls) {
      video.currentTime = targetMs / 1000
      return
    }
    const inRunS = (targetMs - offsetRef.current) / 1000
    const producedEnd =
      video.seekable.length > 0 ? video.seekable.end(video.seekable.length - 1) : 0
    if (inRunS >= 0 && inRunS <= producedEnd) {
      video.currentTime = inRunS
      return
    }
    setSeeking(true)
    try {
      await seekSession(session.session_id, targetMs)
      offsetRef.current = targetMs
      setPosMs(targetMs)
      attach()
    } finally {
      setSeeking(false)
    }
  }

  useEffect(() => {
    const video = videoRef.current!
    attach()

    const seekToResume = () => {
      if (!isHls && resumeMs > 0) video.currentTime = resumeMs / 1000
    }
    video.addEventListener('loadedmetadata', seekToResume)

    const absMs = () => offsetRef.current + video.currentTime * 1000
    const onTime = () => setPosMs(absMs())
    video.addEventListener('timeupdate', onTime)

    const report = (keepalive = false) =>
      postProgress(session.session_id, absMs(), keepalive)
    const tick = setInterval(() => {
      if (!video.paused) report()
    }, 10_000)
    const onPause = () => report()
    const onEnded = () => report()
    video.addEventListener('pause', onPause)
    video.addEventListener('ended', onEnded)
    const onUnload = () => {
      report(true)
      endSession(session.session_id, true)
    }
    window.addEventListener('beforeunload', onUnload)

    return () => {
      clearInterval(tick)
      video.removeEventListener('loadedmetadata', seekToResume)
      video.removeEventListener('timeupdate', onTime)
      video.removeEventListener('pause', onPause)
      video.removeEventListener('ended', onEnded)
      window.removeEventListener('beforeunload', onUnload)
      report(true)
      endSession(session.session_id, true)
      hlsRef.current?.destroy()
    }
  }, [session.session_id])

  const pct = durationMs > 0 ? Math.min(100, (posMs / durationMs) * 100) : 0
  return (
    <main className="player">
      <button className="btn ghost small" onClick={onClose}>
        ← Back
      </button>
      <video ref={videoRef} controls playsInline />
      {isHls && durationMs > 0 && (
        <div
          className={`seekbar${seeking ? ' busy' : ''}`}
          title="Seek anywhere — the stream restarts at the target if needed"
          onClick={(e) => {
            const r = e.currentTarget.getBoundingClientRect()
            void seekTo(((e.clientX - r.left) / r.width) * durationMs)
          }}
        >
          <div className="seekbar-fill" style={{ width: `${pct}%` }} />
          <span className="seekbar-time mono">
            {fmt(posMs)} / {fmt(durationMs)}
          </span>
        </div>
      )}
      <div className="playback-info mono">
        {item.title} · {session.mode}
        {session.streams
          ? ` · video: ${session.streams.video} · audio: ${session.streams.audio}`
          : ''}{' '}
        · {session.content_type}
      </div>
    </main>
  )
}
