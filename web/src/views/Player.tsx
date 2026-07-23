import { useEffect, useRef } from 'react'
import Hls from 'hls.js'
import { accessToken, endSession, postProgress, type Item, type Session } from '../api'

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

  useEffect(() => {
    const video = videoRef.current!
    let hls: Hls | null = null

    if (session.stream_url.endsWith('.m3u8') && Hls.isSupported()) {
      hls = new Hls({
        // Media requests carry the Bearer token; the cookie is the
        // fallback for engines we don't drive ourselves.
        xhrSetup: (xhr) => {
          const t = accessToken()
          if (t) xhr.setRequestHeader('Authorization', `Bearer ${t}`)
        },
      })
      hls.loadSource(session.stream_url)
      hls.attachMedia(video)
    } else {
      video.src = session.stream_url // cookie-authenticated
    }

    const seekToResume = () => {
      if (resumeMs > 0) video.currentTime = resumeMs / 1000
    }
    video.addEventListener('loadedmetadata', seekToResume)
    void video.play().catch(() => undefined)

    const report = (keepalive = false) =>
      postProgress(session.session_id, video.currentTime * 1000, keepalive)
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
      video.removeEventListener('pause', onPause)
      video.removeEventListener('ended', onEnded)
      window.removeEventListener('beforeunload', onUnload)
      report(true)
      endSession(session.session_id, true)
      hls?.destroy()
    }
  }, [session.session_id])

  return (
    <main className="player">
      <button className="btn ghost small" onClick={onClose}>
        ← Back
      </button>
      <video ref={videoRef} controls playsInline />
      <div className="playback-info mono">
        {item.title} · {session.mode}
        {session.mode === 'remux' ? ' (repackaged in the hub, no re-encoding)' : ''} ·{' '}
        {session.content_type}
      </div>
    </main>
  )
}
