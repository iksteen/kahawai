import { useEffect, useRef, useState } from 'react'
import Hls from 'hls.js'
import {
  accessToken,
  api,
  endSession,
  fetchItem,
  fetchSubtitles,
  postProgress,
  seekSession,
  subtitleLabel,
  type Item,
  type Session,
  type Subtitle,
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
  const [subs, setSubs] = useState<Subtitle[]>([])
  const [subKey, setSubKey] = useState('')
  const [audioTracks, setAudioTracks] = useState<
    { codec: string; channels: number; language?: string | null }[]
  >([])
  const [audioTrack, setAudioTrack] = useState(0)
  const [videoTracks, setVideoTracks] = useState<
    { codec: string; width: number; height: number }[]
  >([])
  const [videoTrack, setVideoTrack] = useState(0)
  // The <track> URL must shift cues when the HLS timeline starts mid-file;
  // bump on seek-restarts so the track reloads with the new shift.
  const [trackEpoch, setTrackEpoch] = useState(0)

  useEffect(() => {
    fetchSubtitles(item.id)
      .then((r) => setSubs(r.subtitles))
      .catch(() => setSubs([]))
    fetchItem(item.id)
      .then((d) => {
        setAudioTracks(d.sources_detail[0]?.streams?.audio ?? [])
        setVideoTracks(d.sources_detail[0]?.streams?.video ?? [])
      })
      .catch(() => {
        setAudioTracks([])
        setVideoTracks([])
      })
  }, [item.id])

  // Track switching is a seek-restart at the current position with the
  // new track (§6 machinery; ~2 s hiccup, same as a deep seek).
  const switchTracks = async (audio: number, video_: number) => {
    const video = videoRef.current!
    setAudioTrack(audio)
    setVideoTrack(video_)
    setSeeking(true)
    try {
      const absMs = offsetRef.current + video.currentTime * 1000
      await seekSession(session.session_id, absMs, audio, video_)
      offsetRef.current = Math.round(absMs)
      setPosMs(offsetRef.current)
      setTrackEpoch((e) => e + 1)
      attach()
    } finally {
      setSeeking(false)
    }
  }

  // <track> is lazy about mode; force the selected one to display.
  useEffect(() => {
    const tracks = videoRef.current?.textTracks
    if (!tracks) return
    for (const t of Array.from(tracks)) t.mode = subKey ? 'showing' : 'disabled'
  }, [subKey, trackEpoch])

  // Offset starts snap to the keyframe before the requested position;
  // the pipeline reports the true playlist origin in start.pos. Adopt
  // it so subtitle cues and the seekbar line up exactly.
  const syncOrigin = async () => {
    if (offsetRef.current === 0) return
    const base = session.stream_url.replace(/[^/]*$/, '')
    for (let attempt = 0; attempt < 3; attempt++) {
      try {
        const r = await api(`${base}start.pos`)
        if (r.ok) {
          const n = Math.round(Number(await r.text()))
          if (
            Number.isFinite(n) &&
            n !== offsetRef.current &&
            Math.abs(n - offsetRef.current) < 60000
          ) {
            const video = videoRef.current
            offsetRef.current = n
            if (video) setPosMs(n + video.currentTime * 1000)
            setTrackEpoch((e) => e + 1)
          }
          return
        }
      } catch {
        /* retry */
      }
      await new Promise((res) => setTimeout(res, 700))
    }
  }

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
        // Our EVENT playlists are growing recordings, not live TV: the
        // pipeline paces itself a window ahead of THIS player, so the
        // default live-edge sync creates a feedback loop — hls.js chases
        // the edge, the edge moves with it, playback lives at the
        // starved frontier and buffers on every segment. Watch from the
        // beginning and never chase.
        startPosition: 0,
        liveSyncDurationCount: 1e6,
        liveMaxLatencyDurationCount: Infinity,
        maxBufferLength: 60,
      })
      hls.loadSource(session.stream_url)
      hls.attachMedia(video)
      hlsRef.current = hls
    } else {
      video.src = session.stream_url // cookie-authenticated
    }
    void video.play().catch(() => undefined)
    void syncOrigin()
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
      offsetRef.current = Math.round(targetMs)
      setPosMs(targetMs)
      setTrackEpoch((e) => e + 1)
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
      <video ref={videoRef} controls playsInline crossOrigin="use-credentials">
        {subKey && (
          <track
            key={`${subKey}-${trackEpoch}`}
            default
            kind="subtitles"
            src={`/api/v1/items/${item.id}/subtitles/${subKey}.vtt?shift_ms=${-Math.round(offsetRef.current)}`}
          />
        )}
      </video>
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
      {isHls && audioTracks.length > 1 && (
        <label className="subpick">
          Audio{' '}
          <select
            value={audioTrack}
            disabled={seeking}
            onChange={(e) => void switchTracks(Number(e.target.value), videoTrack)}
          >
            {audioTracks.map((a, i) => (
              <option key={i} value={i}>
                {a.language ?? '?'} · {a.codec} {a.channels}ch
              </option>
            ))}
          </select>
        </label>
      )}
      {isHls && videoTracks.length > 1 && (
        <label className="subpick">
          Video{' '}
          <select
            value={videoTrack}
            disabled={seeking}
            onChange={(e) => void switchTracks(audioTrack, Number(e.target.value))}
          >
            {videoTracks.map((v, i) => (
              <option key={i} value={i}>
                {v.codec} {v.width}×{v.height}
              </option>
            ))}
          </select>
        </label>
      )}
      {subs.length > 0 && (
        <label className="subpick">
          Subtitles{' '}
          <select value={subKey} onChange={(e) => setSubKey(e.target.value)}>
            <option value="">Off</option>
            {subs.map((s) => (
              <option key={s.key} value={s.key}>
                {subtitleLabel(s)}
              </option>
            ))}
          </select>
        </label>
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
